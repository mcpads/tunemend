//! One PlayStation SPU voice playing an SPU ADPCM region: the documented
//! pitch counter, 4-point Gaussian interpolation, flag-driven block
//! addressing and the ADSR envelope generator, at the 44100 Hz output rate.
//!
//! Voice volume, sweep, pitch modulation, noise, reverb and the mixer are
//! not modelled; the output is the interpolated sample times the envelope.
use serde_json::{Value, json};

use crate::psx_adpcm::{BLOCK_BYTES, BLOCK_SAMPLES, LOOP_END, LOOP_REPEAT, LOOP_START, step};

pub const OUTPUT_RATE: u32 = 44100;
/// Pitch values above 0x3FFF step as 0x4000.
pub const MAX_STEP: u32 = 0x4000;
/// Longest audition, in output samples (30 seconds).
pub const MAX_SAMPLES: usize = 30 * OUTPUT_RATE as usize;
pub const MODEL: &str = "documented SPU voice: pitch counter (0x1000 = 44100 Hz, steps above 0x3FFF act as 0x4000), 4-point Gaussian interpolation over the newest four decoded samples, block flags (loop start sets the repeat address, loop end jumps to it or releases with envelope 0), ADSR generator from ADSR1/ADSR2 with key-on level 0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Attack,
    Decay,
    Sustain,
    Release,
    Off,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Attack => "attack",
            Phase::Decay => "decay",
            Phase::Sustain => "sustain",
            Phase::Release => "release",
            Phase::Off => "off",
        }
    }
}

/// One envelope phase's rate parameters.
#[derive(Debug, Clone, Copy)]
struct Rate {
    exponential: bool,
    decreasing: bool,
    shift: i32,
    step: i32,
    /// Shift and step are all ones: the documented "never step" rate.
    frozen: bool,
}

fn rate(phase: Phase, adsr1: u16, adsr2: u16) -> Rate {
    match phase {
        Phase::Attack => {
            let (shift, step) = (((adsr1 >> 10) & 0x1F) as i32, ((adsr1 >> 8) & 3) as i32);
            Rate {
                exponential: adsr1 & 0x8000 != 0,
                decreasing: false,
                shift,
                step,
                frozen: shift == 0x1F && step == 3,
            }
        }
        Phase::Decay => Rate {
            exponential: true,
            decreasing: true,
            shift: ((adsr1 >> 4) & 0xF) as i32,
            step: 0,
            frozen: false,
        },
        Phase::Sustain => {
            let (shift, step) = (((adsr2 >> 8) & 0x1F) as i32, ((adsr2 >> 6) & 3) as i32);
            Rate {
                exponential: adsr2 & 0x8000 != 0,
                decreasing: adsr2 & 0x4000 != 0,
                shift,
                step,
                frozen: shift == 0x1F && step == 3,
            }
        }
        Phase::Release | Phase::Off => Rate {
            exponential: adsr2 & 0x20 != 0,
            decreasing: true,
            shift: (adsr2 & 0x1F) as i32,
            step: 0,
            frozen: false,
        },
    }
}

/// One 44100 Hz envelope cycle as listed in the documentation. The counter
/// is cleared when it steps; every increment is a power of two not above
/// 0x8000, so clearing only bit 15 would give the same steps.
fn envelope_cycle(level: &mut i32, counter: &mut u32, r: Rate) {
    let mut adsr_step = 7 - r.step;
    if r.decreasing {
        adsr_step = !adsr_step;
    }
    adsr_step <<= (11 - r.shift).max(0);
    let mut increment: u32 = 0x8000 >> (r.shift - 11).max(0);
    if r.exponential && !r.decreasing && *level > 0x6000 {
        if r.shift < 10 {
            adsr_step /= 4;
        } else if r.shift >= 11 {
            increment /= 4;
        } else {
            adsr_step /= 2;
            increment /= 2;
        }
    } else if r.exponential && r.decreasing {
        // "AdsrStep * AdsrLevel / 8000h" as an arithmetic shift: a negative
        // step stays at least -1 while the level is positive, so the level
        // keeps falling to 0. Truncating toward zero would stop it at the
        // level where |step| * level < 0x8000 (below 0x1000 for step -8).
        adsr_step = (adsr_step * *level) >> 15;
    }
    if !r.frozen {
        increment = increment.max(1);
    }
    *counter += increment;
    if *counter & 0x8000 == 0 {
        return;
    }
    *counter = 0;
    *level += adsr_step;
    *level = if !r.decreasing {
        (*level).clamp(-0x8000, 0x7FFF)
    } else {
        (*level).max(0)
    };
}

fn nibble(block: &[u8], index: usize) -> i32 {
    let byte = block[2 + index / 2];
    let n = if index.is_multiple_of(2) {
        byte & 15
    } else {
        byte >> 4
    } as i32;
    if n >= 8 { n - 16 } else { n }
}

/// What a voice plays and for how long.
#[derive(Debug, Clone)]
pub struct VoiceSetup<'a> {
    /// The region from its first block; it must already parse as a stream.
    pub region: &'a [u8],
    pub pitch: u16,
    pub adsr1: u16,
    pub adsr2: u16,
    /// Output samples before key-off.
    pub key_on_samples: usize,
    /// Output samples after key-off.
    pub release_samples: usize,
}

/// One keyed-on SPU voice, stepped one 44100 Hz output sample at a time.
#[derive(Debug, Clone)]
pub struct Voice<'a> {
    region: &'a [u8],
    blocks: usize,
    step: u32,
    adsr1: u16,
    adsr2: u16,
    repeat: Option<usize>,
    old: i32,
    older: i32,
    block: usize,
    /// Three samples before the current block, then its 28 samples.
    window: [i32; 3 + BLOCK_SAMPLES],
    level: i32,
    env_counter: u32,
    phase: Phase,
    sustain_level: i32,
    counter: u32,
    /// Output samples produced so far.
    sample: usize,
    record: bool,
    events: Vec<Value>,
    traversals: usize,
    blocks_played: usize,
    peak: i32,
}

impl<'a> Voice<'a> {
    /// Keys the voice on at envelope level zero and decodes the first block.
    /// With `record`, phase changes are kept for `events`.
    pub fn key_on(
        region: &'a [u8],
        pitch: u16,
        adsr1: u16,
        adsr2: u16,
        record: bool,
    ) -> Result<Self, String> {
        let mut voice = Voice {
            region,
            blocks: region.len() / BLOCK_BYTES,
            step: (pitch as u32).min(MAX_STEP),
            adsr1,
            adsr2,
            repeat: None,
            old: 0,
            older: 0,
            block: 0,
            window: [0; 3 + BLOCK_SAMPLES],
            level: 0,
            env_counter: 0,
            phase: Phase::Attack,
            sustain_level: ((adsr1 & 0xF) as i32 + 1) * 0x800,
            counter: 0,
            sample: 0,
            record,
            events: Vec::new(),
            traversals: 0,
            blocks_played: 1,
            peak: 0,
        };
        voice.event(json!({"sample": 0, "event": "key_on", "phase": "attack"}));
        voice.decode()?;
        Ok(voice)
    }

    fn event(&mut self, value: Value) {
        if self.record {
            self.events.push(value);
        }
    }

    fn decode(&mut self) -> Result<(), String> {
        let index = self.block;
        if index >= self.blocks {
            return Err(format!(
                "the voice reached block {index}, past the region's {} blocks",
                self.blocks
            ));
        }
        let b = &self.region[index * BLOCK_BYTES..(index + 1) * BLOCK_BYTES];
        let (shift, filter) = (b[0] & 15, b[0] >> 4);
        if filter > 4 || shift > 12 {
            return Err(format!(
                "block {index}: filter {filter} / shift {shift} is not a documented value"
            ));
        }
        if b[1] & LOOP_START != 0 {
            self.repeat = Some(index);
        }
        self.window.copy_within(BLOCK_SAMPLES..BLOCK_SAMPLES + 3, 0);
        for i in 0..BLOCK_SAMPLES {
            let value = step(nibble(b, i), shift, filter, self.old, self.older);
            self.window[3 + i] = value;
            (self.older, self.old) = (self.old, value);
        }
        Ok(())
    }

    /// Starts the release phase (no effect once the voice is off).
    pub fn key_off(&mut self) {
        if matches!(self.phase, Phase::Off | Phase::Release) {
            return;
        }
        self.phase = Phase::Release;
        self.env_counter = 0;
        let (t, level) = (self.sample, self.level);
        self.event(json!({"sample": t, "event": "key_off", "phase": "release", "level": level}));
    }

    /// A new pitch register for the following samples.
    pub fn set_pitch(&mut self, pitch: u16) {
        self.step = (pitch as u32).min(MAX_STEP);
    }

    /// The envelope reached zero in release, or the region ended.
    pub fn is_off(&self) -> bool {
        self.phase == Phase::Off
    }

    pub fn phase_name(&self) -> &'static str {
        self.phase.name()
    }

    pub fn level(&self) -> i32 {
        self.level
    }

    /// The next output sample: interpolated sample times the envelope.
    pub fn next_sample(&mut self) -> Result<i32, String> {
        let t = self.sample;
        self.sample += 1;
        if self.phase == Phase::Off {
            return Ok(0);
        }
        let p = (self.counter >> 12) as usize;
        let i = ((self.counter >> 4) & 0xFF) as usize;
        let w = &self.window;
        let (oldest, older_s, old_s, new) = (w[p], w[p + 1], w[p + 2], w[p + 3]);
        let interpolated = ((GAUSS[0xFF - i] * oldest) >> 15)
            + ((GAUSS[0x1FF - i] * older_s) >> 15)
            + ((GAUSS[0x100 + i] * old_s) >> 15)
            + ((GAUSS[i] * new) >> 15);
        let value =
            ((interpolated.clamp(-0x8000, 0x7FFF) * self.level) >> 15).clamp(-0x8000, 0x7FFF);
        self.peak = self.peak.max(value.abs());
        envelope_cycle(
            &mut self.level,
            &mut self.env_counter,
            rate(self.phase, self.adsr1, self.adsr2),
        );
        let next = match self.phase {
            Phase::Attack if self.level >= 0x7FFF => Some(Phase::Decay),
            Phase::Decay if self.level <= self.sustain_level => Some(Phase::Sustain),
            Phase::Release if self.level == 0 => Some(Phase::Off),
            _ => None,
        };
        if let Some(next) = next {
            self.phase = next;
            self.env_counter = 0;
            let level = self.level;
            self.event(
                json!({"sample": t + 1, "event": "phase", "phase": next.name(), "level": level}),
            );
        }
        self.counter += self.step;
        while self.counter >> 12 >= BLOCK_SAMPLES as u32 && self.phase != Phase::Off {
            self.counter -= (BLOCK_SAMPLES as u32) << 12;
            let flags = self.region[self.block * BLOCK_BYTES + 1];
            if flags & LOOP_END != 0 {
                if flags & LOOP_REPEAT != 0 {
                    self.block = self.repeat.ok_or_else(|| {
                        format!(
                            "block {} repeats, but no loop-start block set the repeat address",
                            self.block
                        )
                    })?;
                    self.traversals += 1;
                } else {
                    self.phase = Phase::Off;
                    self.level = 0;
                    let block = self.block;
                    self.event(json!({"sample": t + 1, "event": "end_mute", "block": block, "phase": "off"}));
                    break;
                }
            } else {
                self.block += 1;
            }
            self.decode()?;
            self.blocks_played += 1;
        }
        Ok(value)
    }
}

/// Renders the voice as mono samples at 44100 Hz with an event report.
pub fn render(setup: &VoiceSetup) -> Result<(Vec<i32>, Value), String> {
    let total = setup.key_on_samples + setup.release_samples;
    if total == 0 || total > MAX_SAMPLES {
        return Err(format!(
            "key_on_samples plus release_samples must be 1..={MAX_SAMPLES} (30 s at 44100 Hz)"
        ));
    }
    let mut voice = Voice::key_on(setup.region, setup.pitch, setup.adsr1, setup.adsr2, true)?;
    let mut out = Vec::with_capacity(total);
    for t in 0..total {
        if t == setup.key_on_samples {
            voice.key_off();
        }
        out.push(voice.next_sample()?);
    }
    let step_value = voice.step;
    let report = json!({
        "model": MODEL,
        "output_rate": OUTPUT_RATE,
        "pitch": {"register": setup.pitch, "step": step_value, "clamped": setup.pitch as u32 > 0x3FFF, "sample_rate_hz": step_value as f64 * OUTPUT_RATE as f64 / 4096.0},
        "adsr": crate::vab::adsr_fields(setup.adsr1, setup.adsr2),
        "key_on_samples": setup.key_on_samples,
        "release_samples": setup.release_samples,
        "events": voice.events,
        "loop_traversals": voice.traversals,
        "blocks_decoded": voice.blocks_played,
        "final_phase": voice.phase.name(),
        "peak": voice.peak,
        "readings": READINGS,
    });
    Ok((out, report))
}

/// Declared readings where the documentation is silent.
pub const READINGS: [&str; 5] = [
    "the newest interpolation sample is the one at the pitch counter's sample index; the three before it carry across block boundaries and the loop jump, starting from zero at key-on",
    "the envelope counter starts at zero at key-on and at every phase change",
    "attack ends when the level reaches 0x7FFF, decay when it is at or below (sustain level + 1) * 0x800, release at 0",
    "the never-step exception applies to attack and sustain rates whose shift and step are all ones",
    "the exponential decrease step AdsrStep * AdsrLevel / 8000h is an arithmetic shift right by 15 (rounding down), so it stays at least -1 above level 0 and release ends at 0 as documented",
];

/// The SPU 4-point Gaussian interpolation table, 512 entries, as listed in
/// psx-spx "SPU Pitch" (4-Point Gaussian Interpolation), read 2026-10-26.
#[rustfmt::skip]
pub const GAUSS: [i32; 512] = [
    -0x0001, -0x0001, -0x0001, -0x0001, -0x0001, -0x0001, -0x0001, -0x0001,
    -0x0001, -0x0001, -0x0001, -0x0001, -0x0001, -0x0001, -0x0001, -0x0001,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0001,
    0x0001, 0x0001, 0x0001, 0x0002, 0x0002, 0x0002, 0x0003, 0x0003,
    0x0003, 0x0004, 0x0004, 0x0005, 0x0005, 0x0006, 0x0007, 0x0007,
    0x0008, 0x0009, 0x0009, 0x000a, 0x000b, 0x000c, 0x000d, 0x000e,
    0x000f, 0x0010, 0x0011, 0x0012, 0x0013, 0x0015, 0x0016, 0x0018,
    0x0019, 0x001b, 0x001c, 0x001e, 0x0020, 0x0021, 0x0023, 0x0025,
    0x0027, 0x0029, 0x002c, 0x002e, 0x0030, 0x0033, 0x0035, 0x0038,
    0x003a, 0x003d, 0x0040, 0x0043, 0x0046, 0x0049, 0x004d, 0x0050,
    0x0054, 0x0057, 0x005b, 0x005f, 0x0063, 0x0067, 0x006b, 0x006f,
    0x0074, 0x0078, 0x007d, 0x0082, 0x0087, 0x008c, 0x0091, 0x0096,
    0x009c, 0x00a1, 0x00a7, 0x00ad, 0x00b3, 0x00ba, 0x00c0, 0x00c7,
    0x00cd, 0x00d4, 0x00db, 0x00e3, 0x00ea, 0x00f2, 0x00fa, 0x0101,
    0x010a, 0x0112, 0x011b, 0x0123, 0x012c, 0x0135, 0x013f, 0x0148,
    0x0152, 0x015c, 0x0166, 0x0171, 0x017b, 0x0186, 0x0191, 0x019c,
    0x01a8, 0x01b4, 0x01c0, 0x01cc, 0x01d9, 0x01e5, 0x01f2, 0x0200,
    0x020d, 0x021b, 0x0229, 0x0237, 0x0246, 0x0255, 0x0264, 0x0273,
    0x0283, 0x0293, 0x02a3, 0x02b4, 0x02c4, 0x02d6, 0x02e7, 0x02f9,
    0x030b, 0x031d, 0x0330, 0x0343, 0x0356, 0x036a, 0x037e, 0x0392,
    0x03a7, 0x03bc, 0x03d1, 0x03e7, 0x03fc, 0x0413, 0x042a, 0x0441,
    0x0458, 0x0470, 0x0488, 0x04a0, 0x04b9, 0x04d2, 0x04ec, 0x0506,
    0x0520, 0x053b, 0x0556, 0x0572, 0x058e, 0x05aa, 0x05c7, 0x05e4,
    0x0601, 0x061f, 0x063e, 0x065c, 0x067c, 0x069b, 0x06bb, 0x06dc,
    0x06fd, 0x071e, 0x0740, 0x0762, 0x0784, 0x07a7, 0x07cb, 0x07ef,
    0x0813, 0x0838, 0x085d, 0x0883, 0x08a9, 0x08d0, 0x08f7, 0x091e,
    0x0946, 0x096f, 0x0998, 0x09c1, 0x09eb, 0x0a16, 0x0a40, 0x0a6c,
    0x0a98, 0x0ac4, 0x0af1, 0x0b1e, 0x0b4c, 0x0b7a, 0x0ba9, 0x0bd8,
    0x0c07, 0x0c38, 0x0c68, 0x0c99, 0x0ccb, 0x0cfd, 0x0d30, 0x0d63,
    0x0d97, 0x0dcb, 0x0e00, 0x0e35, 0x0e6b, 0x0ea1, 0x0ed7, 0x0f0f,
    0x0f46, 0x0f7f, 0x0fb7, 0x0ff1, 0x102a, 0x1065, 0x109f, 0x10db,
    0x1116, 0x1153, 0x118f, 0x11cd, 0x120b, 0x1249, 0x1288, 0x12c7,
    0x1307, 0x1347, 0x1388, 0x13c9, 0x140b, 0x144d, 0x1490, 0x14d4,
    0x1517, 0x155c, 0x15a0, 0x15e6, 0x162c, 0x1672, 0x16b9, 0x1700,
    0x1747, 0x1790, 0x17d8, 0x1821, 0x186b, 0x18b5, 0x1900, 0x194b,
    0x1996, 0x19e2, 0x1a2e, 0x1a7b, 0x1ac8, 0x1b16, 0x1b64, 0x1bb3,
    0x1c02, 0x1c51, 0x1ca1, 0x1cf1, 0x1d42, 0x1d93, 0x1de5, 0x1e37,
    0x1e89, 0x1edc, 0x1f2f, 0x1f82, 0x1fd6, 0x202a, 0x207f, 0x20d4,
    0x2129, 0x217f, 0x21d5, 0x222c, 0x2282, 0x22da, 0x2331, 0x2389,
    0x23e1, 0x2439, 0x2492, 0x24eb, 0x2545, 0x259e, 0x25f8, 0x2653,
    0x26ad, 0x2708, 0x2763, 0x27be, 0x281a, 0x2876, 0x28d2, 0x292e,
    0x298b, 0x29e7, 0x2a44, 0x2aa1, 0x2aff, 0x2b5c, 0x2bba, 0x2c18,
    0x2c76, 0x2cd4, 0x2d33, 0x2d91, 0x2df0, 0x2e4f, 0x2eae, 0x2f0d,
    0x2f6c, 0x2fcc, 0x302b, 0x308b, 0x30ea, 0x314a, 0x31aa, 0x3209,
    0x3269, 0x32c9, 0x3329, 0x3389, 0x33e9, 0x3449, 0x34a9, 0x3509,
    0x3569, 0x35c9, 0x3629, 0x3689, 0x36e8, 0x3748, 0x37a8, 0x3807,
    0x3867, 0x38c6, 0x3926, 0x3985, 0x39e4, 0x3a43, 0x3aa2, 0x3b00,
    0x3b5f, 0x3bbd, 0x3c1b, 0x3c79, 0x3cd7, 0x3d35, 0x3d92, 0x3def,
    0x3e4c, 0x3ea9, 0x3f05, 0x3f62, 0x3fbd, 0x4019, 0x4074, 0x40d0,
    0x412a, 0x4185, 0x41df, 0x4239, 0x4292, 0x42eb, 0x4344, 0x439c,
    0x43f4, 0x444c, 0x44a3, 0x44fa, 0x4550, 0x45a6, 0x45fc, 0x4651,
    0x46a6, 0x46fa, 0x474e, 0x47a1, 0x47f4, 0x4846, 0x4898, 0x48e9,
    0x493a, 0x498a, 0x49d9, 0x4a29, 0x4a77, 0x4ac5, 0x4b13, 0x4b5f,
    0x4bac, 0x4bf7, 0x4c42, 0x4c8d, 0x4cd7, 0x4d20, 0x4d68, 0x4db0,
    0x4df7, 0x4e3e, 0x4e84, 0x4ec9, 0x4f0e, 0x4f52, 0x4f95, 0x4fd7,
    0x5019, 0x505a, 0x509a, 0x50da, 0x5118, 0x5156, 0x5194, 0x51d0,
    0x520c, 0x5247, 0x5281, 0x52ba, 0x52f3, 0x532a, 0x5361, 0x5397,
    0x53cc, 0x5401, 0x5434, 0x5467, 0x5499, 0x54ca, 0x54fa, 0x5529,
    0x5558, 0x5585, 0x55b2, 0x55de, 0x5609, 0x5632, 0x565b, 0x5684,
    0x56ab, 0x56d1, 0x56f6, 0x571b, 0x573e, 0x5761, 0x5782, 0x57a3,
    0x57c3, 0x57e2, 0x57ff, 0x581c, 0x5838, 0x5853, 0x586d, 0x5886,
    0x589e, 0x58b5, 0x58cb, 0x58e0, 0x58f4, 0x5907, 0x5919, 0x592a,
    0x593a, 0x5949, 0x5958, 0x5965, 0x5971, 0x597c, 0x5986, 0x598f,
    0x5997, 0x599e, 0x59a4, 0x59a9, 0x59ad, 0x59b0, 0x59b2, 0x59b3,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn region(flags_last: u8) -> Vec<u8> {
        let mut r = vec![0u8; 16];
        r.extend([0x00, 0x04]);
        r.extend([0x77u8; 14]);
        r.extend([0x00, flags_last]);
        r.extend([0x99u8; 14]);
        r
    }

    #[test]
    fn gaussian_table_sums_as_documented() {
        for i in 0..256 {
            let sum = GAUSS[i] + GAUSS[0xFF - i] + GAUSS[0x100 + i] + GAUSS[0x1FF - i];
            assert!((0x7F7F..=0x7F81).contains(&sum), "{i}: {sum:#x}");
        }
    }

    #[test]
    fn linear_attack_steps_as_listed() {
        // Shift 0, step 0: +7 << 11 every cycle.
        let (mut level, mut counter) = (0, 0);
        let r = rate(Phase::Attack, 0x0000, 0);
        envelope_cycle(&mut level, &mut counter, r);
        assert_eq!(level, 7 << 11);
        // Shift 13: one step every 4 cycles, of +7.
        let r = rate(Phase::Attack, 13 << 10, 0);
        let (mut level, mut counter) = (0, 0);
        for _ in 0..3 {
            envelope_cycle(&mut level, &mut counter, r);
        }
        assert_eq!(level, 0);
        envelope_cycle(&mut level, &mut counter, r);
        assert_eq!(level, 7);
        // All-ones attack never steps.
        let r = rate(Phase::Attack, 0x7F00, 0);
        let (mut level, mut counter) = (100, 0);
        for _ in 0..100_000 {
            envelope_cycle(&mut level, &mut counter, r);
        }
        assert_eq!(level, 100);
    }

    #[test]
    fn exponential_decrease_scales_with_level() {
        let r = rate(Phase::Release, 0, 0x20);
        let (mut level, mut counter) = (0x4000, 0);
        envelope_cycle(&mut level, &mut counter, r);
        // -8 << 11 = -16384, times 0x4000 / 0x8000 = -8192.
        assert_eq!(level, 0x2000);
    }

    #[test]
    fn exponential_release_reaches_zero_and_ends_the_voice() {
        // Release shift 13, exponential (ADSR2 bit 5): one step of
        // (-8 * level) >> 15 every 4 cycles. A truncating division would hold
        // the level at 4095 forever; the arithmetic shift steps by -1 there.
        let r = rate(Phase::Release, 0, 0x20 | 13);
        let (mut level, mut counter) = (4095, 0);
        for _ in 0..4 {
            envelope_cycle(&mut level, &mut counter, r);
        }
        assert_eq!(level, 4094);
        let (mut level, mut counter, mut cycles) = (0x7FFF, 0, 0u32);
        while level > 0 {
            envelope_cycle(&mut level, &mut counter, r);
            cycles += 1;
            assert!(cycles < 2_000_000, "release stuck at {level}");
        }
        // The voice model turns the zero level into the off phase.
        let looping = region(3);
        let setup = VoiceSetup {
            region: &looping,
            pitch: 0x1000,
            adsr1: 0x000F,
            adsr2: 0x1FC0 | 0x20 | 13,
            key_on_samples: 100,
            release_samples: cycles as usize + 100,
        };
        let (out, report) = render(&setup).unwrap();
        assert_eq!(report["final_phase"], "off");
        assert!(out[out.len() - 100..].iter().all(|&v| v == 0));
    }

    #[test]
    fn looping_voice_repeats_and_one_shot_releases() {
        let looping = region(3);
        let setup = VoiceSetup {
            region: &looping,
            pitch: 0x1000,
            adsr1: 0x000F,
            adsr2: 0x1FC0,
            key_on_samples: 28 * 10,
            release_samples: 0,
        };
        let (out, report) = render(&setup).unwrap();
        assert_eq!(out.len(), 280);
        assert_eq!(report["loop_traversals"].as_u64().unwrap(), 4);
        assert_ne!(*out.last().unwrap(), 0);
        let one_shot = region(1);
        let (out, report) = render(&VoiceSetup {
            region: &one_shot,
            ..setup
        })
        .unwrap();
        assert_eq!(report["final_phase"], "off");
        assert!(out[90..].iter().all(|&v| v == 0));
        // Pitch 0x2000 plays two source samples per output sample.
        let (_, fast) = render(&VoiceSetup {
            region: &looping,
            pitch: 0x2000,
            ..setup
        })
        .unwrap();
        assert_eq!(fast["loop_traversals"].as_u64().unwrap(), 9);
    }
}
