//! SNES S-DSP voice model for offline instrument audition at the nominal
//! 32000 Hz sample clock.
//!
//! References: Martin
//! Korth's fullsnes ("SNES APU DSP" BRR, pitch, envelope, volume and control
//! chapters) and Anomie's S-DSP Doc (jwdonal revision 1212), whose envelope
//! counter rates and offsets are also tabulated on the nesdev SNES wiki
//! ("DSP envelopes").
//!
//! Modelled per voice: 14-bit pitch stepping through a 12-sample ring of BRR
//! groups decoded four at a time with the integer filters, the 4-point
//! Gaussian interpolation with its 16-bit wrap and final clamp, end/loop flags
//! with decoder history carried into the loop, the five empty key-on samples,
//! ADSR and GAIN envelopes on the shared global counter, KON/KOFF polled every
//! other sample, FLG soft reset and mute, VxVOL scaling, the clamped voice mix
//! and MVOL. Not modelled: echo/FIR (EVOL contributes nothing), noise, pitch
//! modulation, the per-voice cycle order inside one sample, the decode that
//! precedes key-on, and the analog output stage.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Nominal output rate. Real units run from a ceramic resonator, so an
/// observed sample clock (for example about 32040 Hz) differs; compare in DSP
/// sample units.
pub const SAMPLE_RATE: u32 = 32000;
/// DSP samples from a note's end until its voice is silent, at most: the
/// key-off waits up to 2 samples for the even-sample poll, then the release
/// lowers the envelope by 8 per sample from at most 0x7FF to 0 (256 samples).
pub const RELEASE_BOUND_SAMPLES: u32 = 258;
/// The global counter counts down from 0x77FF and wraps (Anomie, COUNTERS).
pub const COUNTER_RANGE: u32 = 0x7800;
/// Longest audition, in DSP samples (20 s at the nominal rate).
pub const MAX_SAMPLES: u32 = 640_000;

/// Gaussian interpolation coefficients, identical in fullsnes and Anomie's doc.
pub const GAUSS: [i32; 512] = [
    0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000,
    0x000, 0x000, 0x000, 0x001, 0x001, 0x001, 0x001, 0x001, 0x001, 0x001, 0x001, 0x001, 0x001,
    0x001, 0x002, 0x002, 0x002, 0x002, 0x002, 0x002, 0x002, 0x003, 0x003, 0x003, 0x003, 0x003,
    0x004, 0x004, 0x004, 0x004, 0x004, 0x005, 0x005, 0x005, 0x005, 0x006, 0x006, 0x006, 0x006,
    0x007, 0x007, 0x007, 0x008, 0x008, 0x008, 0x009, 0x009, 0x009, 0x00A, 0x00A, 0x00A, 0x00B,
    0x00B, 0x00B, 0x00C, 0x00C, 0x00D, 0x00D, 0x00E, 0x00E, 0x00F, 0x00F, 0x00F, 0x010, 0x010,
    0x011, 0x011, 0x012, 0x013, 0x013, 0x014, 0x014, 0x015, 0x015, 0x016, 0x017, 0x017, 0x018,
    0x018, 0x019, 0x01A, 0x01B, 0x01B, 0x01C, 0x01D, 0x01D, 0x01E, 0x01F, 0x020, 0x020, 0x021,
    0x022, 0x023, 0x024, 0x024, 0x025, 0x026, 0x027, 0x028, 0x029, 0x02A, 0x02B, 0x02C, 0x02D,
    0x02E, 0x02F, 0x030, 0x031, 0x032, 0x033, 0x034, 0x035, 0x036, 0x037, 0x038, 0x03A, 0x03B,
    0x03C, 0x03D, 0x03E, 0x040, 0x041, 0x042, 0x043, 0x045, 0x046, 0x047, 0x049, 0x04A, 0x04C,
    0x04D, 0x04E, 0x050, 0x051, 0x053, 0x054, 0x056, 0x057, 0x059, 0x05A, 0x05C, 0x05E, 0x05F,
    0x061, 0x063, 0x064, 0x066, 0x068, 0x06A, 0x06B, 0x06D, 0x06F, 0x071, 0x073, 0x075, 0x076,
    0x078, 0x07A, 0x07C, 0x07E, 0x080, 0x082, 0x084, 0x086, 0x089, 0x08B, 0x08D, 0x08F, 0x091,
    0x093, 0x096, 0x098, 0x09A, 0x09C, 0x09F, 0x0A1, 0x0A3, 0x0A6, 0x0A8, 0x0AB, 0x0AD, 0x0AF,
    0x0B2, 0x0B4, 0x0B7, 0x0BA, 0x0BC, 0x0BF, 0x0C1, 0x0C4, 0x0C7, 0x0C9, 0x0CC, 0x0CF, 0x0D2,
    0x0D4, 0x0D7, 0x0DA, 0x0DD, 0x0E0, 0x0E3, 0x0E6, 0x0E9, 0x0EC, 0x0EF, 0x0F2, 0x0F5, 0x0F8,
    0x0FB, 0x0FE, 0x101, 0x104, 0x107, 0x10B, 0x10E, 0x111, 0x114, 0x118, 0x11B, 0x11E, 0x122,
    0x125, 0x129, 0x12C, 0x130, 0x133, 0x137, 0x13A, 0x13E, 0x141, 0x145, 0x148, 0x14C, 0x150,
    0x153, 0x157, 0x15B, 0x15F, 0x162, 0x166, 0x16A, 0x16E, 0x172, 0x176, 0x17A, 0x17D, 0x181,
    0x185, 0x189, 0x18D, 0x191, 0x195, 0x19A, 0x19E, 0x1A2, 0x1A6, 0x1AA, 0x1AE, 0x1B2, 0x1B7,
    0x1BB, 0x1BF, 0x1C3, 0x1C8, 0x1CC, 0x1D0, 0x1D5, 0x1D9, 0x1DD, 0x1E2, 0x1E6, 0x1EB, 0x1EF,
    0x1F3, 0x1F8, 0x1FC, 0x201, 0x205, 0x20A, 0x20F, 0x213, 0x218, 0x21C, 0x221, 0x226, 0x22A,
    0x22F, 0x233, 0x238, 0x23D, 0x241, 0x246, 0x24B, 0x250, 0x254, 0x259, 0x25E, 0x263, 0x267,
    0x26C, 0x271, 0x276, 0x27B, 0x280, 0x284, 0x289, 0x28E, 0x293, 0x298, 0x29D, 0x2A2, 0x2A6,
    0x2AB, 0x2B0, 0x2B5, 0x2BA, 0x2BF, 0x2C4, 0x2C9, 0x2CE, 0x2D3, 0x2D8, 0x2DC, 0x2E1, 0x2E6,
    0x2EB, 0x2F0, 0x2F5, 0x2FA, 0x2FF, 0x304, 0x309, 0x30E, 0x313, 0x318, 0x31D, 0x322, 0x326,
    0x32B, 0x330, 0x335, 0x33A, 0x33F, 0x344, 0x349, 0x34E, 0x353, 0x357, 0x35C, 0x361, 0x366,
    0x36B, 0x370, 0x374, 0x379, 0x37E, 0x383, 0x388, 0x38C, 0x391, 0x396, 0x39B, 0x39F, 0x3A4,
    0x3A9, 0x3AD, 0x3B2, 0x3B7, 0x3BB, 0x3C0, 0x3C5, 0x3C9, 0x3CE, 0x3D2, 0x3D7, 0x3DC, 0x3E0,
    0x3E5, 0x3E9, 0x3ED, 0x3F2, 0x3F6, 0x3FB, 0x3FF, 0x403, 0x408, 0x40C, 0x410, 0x415, 0x419,
    0x41D, 0x421, 0x425, 0x42A, 0x42E, 0x432, 0x436, 0x43A, 0x43E, 0x442, 0x446, 0x44A, 0x44E,
    0x452, 0x455, 0x459, 0x45D, 0x461, 0x465, 0x468, 0x46C, 0x470, 0x473, 0x477, 0x47A, 0x47E,
    0x481, 0x485, 0x488, 0x48C, 0x48F, 0x492, 0x496, 0x499, 0x49C, 0x49F, 0x4A2, 0x4A6, 0x4A9,
    0x4AC, 0x4AF, 0x4B2, 0x4B5, 0x4B7, 0x4BA, 0x4BD, 0x4C0, 0x4C3, 0x4C5, 0x4C8, 0x4CB, 0x4CD,
    0x4D0, 0x4D2, 0x4D5, 0x4D7, 0x4D9, 0x4DC, 0x4DE, 0x4E0, 0x4E3, 0x4E5, 0x4E7, 0x4E9, 0x4EB,
    0x4ED, 0x4EF, 0x4F1, 0x4F3, 0x4F5, 0x4F6, 0x4F8, 0x4FA, 0x4FB, 0x4FD, 0x4FF, 0x500, 0x502,
    0x503, 0x504, 0x506, 0x507, 0x508, 0x50A, 0x50B, 0x50C, 0x50D, 0x50E, 0x50F, 0x510, 0x511,
    0x511, 0x512, 0x513, 0x514, 0x514, 0x515, 0x516, 0x516, 0x517, 0x517, 0x517, 0x518, 0x518,
    0x518, 0x518, 0x518, 0x519, 0x519,
];

/// Samples per envelope step for each 5-bit rate; 0 never fires.
const RATES: [u32; 32] = [
    0, 2048, 1536, 1280, 1024, 768, 640, 512, 384, 320, 256, 192, 160, 128, 96, 80, 64, 48, 40, 32,
    24, 20, 16, 12, 10, 8, 6, 5, 4, 3, 2, 1,
];
/// Counter offsets per rate: 0, 1040 and 536 repeating by column.
const OFFSETS: [u32; 32] = [
    0, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040,
    536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0,
];

pub const MVOLL: usize = 0x0c;
pub const MVOLR: usize = 0x1c;
pub const KON: usize = 0x4c;
pub const KOFF: usize = 0x5c;
pub const DIR: usize = 0x5d;
pub const FLG: usize = 0x6c;
pub const ENDX: usize = 0x7c;
/// Per-voice register offsets within each 16-byte voice row.
pub const VOLL: usize = 0;
pub const VOLR: usize = 1;
pub const PITCHL: usize = 2;
pub const PITCHH: usize = 3;
pub const SRCN: usize = 4;
pub const ADSR1: usize = 5;
pub const ADSR2: usize = 6;
pub const GAIN: usize = 7;
pub const ENVX: usize = 8;
pub const OUTX: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Attack,
    Decay,
    Sustain,
    Release,
}

/// A voice-level occurrence at a DSP sample index, for render reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VoiceEvent {
    pub sample: u64,
    pub voice: usize,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<i64>,
}

#[derive(Debug, Clone)]
struct Voice {
    /// Decoded samples as the BRR filter returns them: 15 bits shifted left.
    ring: [i32; 12],
    /// Start of the oldest of the three four-sample groups.
    ring_pos: usize,
    /// Interpolation index: bits 12-14 select the sample, 4-11 the weight.
    pos: u32,
    address: u16,
    offset: u16,
    load_loop: bool,
    /// Samples left in the key-on start sequence (5 down to 1), else 0.
    delay: u8,
    env: i32,
    /// The last candidate level clipped to 11 bits, used by bent GAIN.
    hidden: i32,
    phase: Phase,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            ring: [0; 12],
            ring_pos: 0,
            pos: 0,
            address: 0,
            offset: 1,
            load_loop: false,
            delay: 0,
            env: 0,
            hidden: 0,
            phase: Phase::Release,
        }
    }
}

/// Eight voices sharing 64 KiB of ARAM and the 128 DSP registers.
pub struct Dsp {
    pub ram: Vec<u8>,
    pub regs: [u8; 128],
    voices: [Voice; 8],
    /// Global counter value seen by the next sample's envelope updates.
    counter: u32,
    /// KON bits written since the last poll.
    kon_pending: u8,
    sample: u64,
    pub events: Vec<VoiceEvent>,
}

fn clamp16(value: i32) -> i32 {
    value.clamp(-32768, 32767)
}

impl Dsp {
    /// A DSP with cleared registers whose first sample sees global counter
    /// `counter_start` (0 just after reset). The counter phase is not readable
    /// from the registers, so audition reports state it explicitly.
    pub fn new(ram: Vec<u8>, counter_start: u32) -> Self {
        assert_eq!(ram.len(), 65536, "ARAM is 64 KiB");
        Self {
            ram,
            regs: [0; 128],
            voices: Default::default(),
            counter: counter_start % COUNTER_RANGE,
            kon_pending: 0,
            sample: 0,
            events: Vec::new(),
        }
    }

    /// A register write as the SPC700 makes it through $F2/$F3. KON is
    /// latched until the next 16 kHz poll; any ENDX write clears every bit.
    pub fn write(&mut self, register: usize, value: u8) {
        let register = register & 0x7f;
        match register {
            KON => {
                self.kon_pending = value;
                self.regs[KON] = value;
            }
            ENDX => self.regs[ENDX] = 0,
            _ => self.regs[register] = value,
        }
    }

    pub fn sample_index(&self) -> u64 {
        self.sample
    }

    pub fn phase(&self, voice: usize) -> Phase {
        self.voices[voice].phase
    }

    pub fn envelope(&self, voice: usize) -> i32 {
        self.voices[voice].env
    }

    fn word(&self, address: usize) -> u16 {
        u16::from_le_bytes([self.ram[address & 0xffff], self.ram[(address + 1) & 0xffff]])
    }

    fn event(&mut self, voice: usize, kind: &'static str, value: Option<i64>) {
        if self.events.len() < 4096 {
            self.events.push(VoiceEvent {
                sample: self.sample,
                voice,
                kind,
                value,
            });
        }
    }

    /// Produces one stereo output sample.
    pub fn tick(&mut self) -> [i16; 2] {
        // KON and KOFF are loaded every other sample (Anomie, cycle 30).
        let (kon, koff) = if self.sample.is_multiple_of(2) {
            (std::mem::take(&mut self.kon_pending), self.regs[KOFF])
        } else {
            (0, 0)
        };
        let mut mix = [0i32; 2];
        for v in 0..8 {
            let out = self.voice(v, kon & (1 << v) != 0, koff & (1 << v) != 0);
            for (channel, register) in [VOLL, VOLR].into_iter().enumerate() {
                let volume = self.regs[v * 16 + register] as i8 as i32;
                mix[channel] = clamp16(mix[channel] + ((out * volume) >> 6));
            }
        }
        let mute = self.regs[FLG] & 0x40 != 0;
        let mut output = [0i16; 2];
        for (channel, register) in [MVOLL, MVOLR].into_iter().enumerate() {
            let master = self.regs[register] as i8 as i32;
            // Echo is not modelled, so the FIR term adds nothing.
            let value = clamp16((mix[channel] * master) >> 7);
            output[channel] = if mute { 0 } else { value as i16 };
        }
        self.counter = self.counter.checked_sub(1).unwrap_or(COUNTER_RANGE - 1);
        self.sample += 1;
        output
    }

    /// One voice's sample: the 15-bit output after the envelope.
    fn voice(&mut self, v: usize, kon: bool, koff: bool) -> i32 {
        let row = v * 16;
        let directory = self.regs[DIR] as usize * 256 + self.regs[row + SRCN] as usize * 4;
        let pitch = (u16::from_le_bytes([self.regs[row + PITCHL], self.regs[row + PITCHH]])
            & 0x3fff) as u32;
        // S2: the first sample after key-on reads the start address; a block
        // that ended flagged the loop address for this step.
        if self.voices[v].delay == 5 {
            self.voices[v].address = self.word(directory);
            self.voices[v].offset = 1;
        } else if self.voices[v].load_loop {
            let address = self.word(directory + 2);
            let voice = &mut self.voices[v];
            voice.load_loop = false;
            voice.address = address;
            voice.offset = 1;
            self.event(v, "loop", Some(address as i64));
        }
        let header = self.ram[self.voices[v].address as usize];
        // S3c: the output uses the interpolation position and envelope left
        // by the previous sample.
        let voice = &self.voices[v];
        let out = if voice.delay == 0 {
            (interpolate(voice) * voice.env) >> 11
        } else {
            0
        };
        self.regs[row + OUTX] = (out >> 7) as u8;
        // A loaded end block without the loop flag releases with envelope 0
        // at once; its own samples are never output.
        if self.voices[v].delay != 5 && header & 3 == 1 {
            let voice = &mut self.voices[v];
            let audible = voice.phase != Phase::Release || voice.env != 0;
            voice.phase = Phase::Release;
            voice.env = 0;
            if audible {
                self.event(v, "end_without_loop", None);
            }
        }
        if self.regs[FLG] & 0x80 != 0 {
            self.voices[v].phase = Phase::Release;
            self.voices[v].env = 0;
        }
        if koff && self.voices[v].phase != Phase::Release {
            self.voices[v].phase = Phase::Release;
            self.event(v, "key_off", None);
        }
        if kon {
            let voice = &mut self.voices[v];
            *voice = Voice {
                ring: voice.ring,
                phase: Phase::Attack,
                delay: 5,
                ..Voice::default()
            };
            self.regs[ENDX] &= !(1 << v);
            self.event(v, "key_on", None);
        } else if self.voices[v].delay <= 1 {
            let before = self.voices[v].phase;
            let (adsr1, adsr2, gain) = (
                self.regs[row + ADSR1],
                self.regs[row + ADSR2],
                self.regs[row + GAIN],
            );
            self.update_envelope(v, adsr1, adsr2, gain);
            let after = self.voices[v].phase;
            if after != before {
                let level = self.voices[v].env as i64;
                let kind = match after {
                    Phase::Decay => "decay",
                    Phase::Sustain => "sustain",
                    Phase::Attack => "attack",
                    Phase::Release => "release",
                };
                self.event(v, kind, Some(level));
            }
        }
        self.regs[row + ENVX] = (self.voices[v].env >> 4) as u8;
        // S4: decode a group when needed, then advance by the pitch.
        match self.voices[v].delay {
            0 => {
                if self.voices[v].pos >= 0x4000 {
                    self.decode_group(v);
                    self.voices[v].pos -= 0x4000;
                }
                let voice = &mut self.voices[v];
                voice.pos = (voice.pos + pitch).min(0x7fff);
            }
            2..=4 => self.decode_group(v),
            _ => {}
        }
        let voice = &mut self.voices[v];
        if voice.delay > 0 && !kon {
            voice.delay -= 1;
        }
        out
    }

    fn decode_group(&mut self, v: usize) {
        let (address, offset) = (
            self.voices[v].address as usize,
            self.voices[v].offset as usize,
        );
        let header = self.ram[address];
        let data = [
            self.ram[(address + offset) & 0xffff],
            self.ram[(address + offset + 1) & 0xffff],
        ];
        let voice = &mut self.voices[v];
        // History comes from the ring, so after key-on a non-direct first
        // block reads its physical end (Anomie, BRR DECODING).
        let mut p1 = voice.ring[(voice.ring_pos + 11) % 12];
        let mut p2 = voice.ring[(voice.ring_pos + 10) % 12];
        for (i, nibble) in [data[0] >> 4, data[0] & 15, data[1] >> 4, data[1] & 15]
            .into_iter()
            .enumerate()
        {
            let n = (nibble as i32 ^ 8) - 8;
            let value = crate::sample_codec::step(n, header >> 4, (header >> 2) & 3, p1, p2);
            voice.ring[voice.ring_pos + i] = value;
            (p2, p1) = (p1, value);
        }
        voice.ring_pos = (voice.ring_pos + 4) % 12;
        voice.offset += 2;
        if voice.offset >= 9 {
            voice.offset = 1;
            if header & 1 != 0 {
                voice.load_loop = true;
                self.regs[ENDX] |= 1 << v;
            } else {
                voice.address = voice.address.wrapping_add(9);
            }
        }
    }

    fn fires(&self, rate: u8) -> bool {
        let rate = rate as usize;
        RATES[rate] != 0 && (self.counter + OFFSETS[rate]).is_multiple_of(RATES[rate])
    }

    /// Envelope step as described by Anomie's VxADSR/VxGAIN notes: compute the
    /// candidate level every sample, commit it when the rate fires, and change
    /// phase from the candidate even when it was not committed.
    fn update_envelope(&mut self, v: usize, adsr1: u8, adsr2: u8, gain: u8) {
        let voice = &self.voices[v];
        let env = voice.env;
        if voice.phase == Phase::Release {
            self.voices[v].env = (env - 8).max(0);
            return;
        }
        let exponential = env - 1 - ((env - 1) >> 8);
        let (rate, candidate, boundary) = if adsr1 & 0x80 != 0 {
            let boundary = (adsr2 >> 5) as i32;
            match voice.phase {
                Phase::Attack if adsr1 & 15 == 15 => (Some(31), env + 1024, boundary),
                Phase::Attack => (Some((adsr1 & 15) * 2 + 1), env + 32, boundary),
                Phase::Decay => (Some(((adsr1 >> 4) & 7) * 2 + 16), exponential, boundary),
                _ => (Some(adsr2 & 31), exponential, boundary),
            }
        } else if gain & 0x80 == 0 {
            // Direct GAIN sets the level every sample regardless of rate.
            (None, (gain & 0x7f) as i32 * 16, (gain >> 5) as i32)
        } else {
            let candidate = match (gain >> 5) & 3 {
                0 => env - 32,
                1 => exponential,
                2 => env + 32,
                _ => env + if voice.hidden < 0x600 { 32 } else { 8 },
            };
            // Outside ADSR mode the sustain boundary comes from GAIN bits 5-7.
            (Some(gain & 31), candidate, (gain >> 5) as i32)
        };
        let commit = rate.is_none_or(|rate| self.fires(rate));
        let voice = &mut self.voices[v];
        if commit {
            voice.env = candidate.clamp(0, 0x7ff);
        }
        if voice.phase == Phase::Decay && candidate >> 8 == boundary {
            voice.phase = Phase::Sustain;
        }
        if voice.phase == Phase::Attack && !(0..=0x7ff).contains(&candidate) {
            voice.phase = Phase::Decay;
        }
        voice.hidden = candidate & 0x7ff;
    }
}

/// 4-point Gaussian interpolation over 15-bit samples (fullsnes, BRR Pitch):
/// the first three products wrap at 16 bits and the fourth addition clamps.
fn interpolate(voice: &Voice) -> i32 {
    let index = (voice.pos >> 12) as usize;
    let weight = ((voice.pos >> 4) & 0xff) as usize;
    let sample = |k: usize| voice.ring[(voice.ring_pos + index + k) % 12] >> 1;
    let partial = ((GAUSS[255 - weight] * sample(0)) >> 10)
        + ((GAUSS[511 - weight] * sample(1)) >> 10)
        + ((GAUSS[256 + weight] * sample(2)) >> 10);
    clamp16(partial as i16 as i32 + ((GAUSS[weight] * sample(3)) >> 10)) >> 1
}

/// Interleaved stereo 16-bit WAV at the declared rate.
pub fn stereo_wav(frames: &[[i16; 2]], rate: u32) -> Vec<u8> {
    let data = (frames.len() * 4) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend(b"RIFF");
    out.extend((36 + data).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * 4).to_le_bytes());
    out.extend(4u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data.to_le_bytes());
    for frame in frames {
        for value in frame {
            out.extend(value.to_le_bytes());
        }
    }
    out
}

/// One keyed note on voice 0, with every register the audition writes.
#[derive(Debug, Clone)]
pub struct VoiceSetup {
    /// Complete BRR sample ending with the end flag in its last block.
    pub brr: Vec<u8>,
    /// Byte offset of the loop block when the end block loops.
    pub loop_offset: Option<usize>,
    pub pitch: u16,
    pub adsr1: u8,
    pub adsr2: u8,
    pub gain: u8,
    pub volume: [i8; 2],
    pub master: [i8; 2],
    /// Samples from key-on until KOFF is written.
    pub key_on_samples: u32,
    /// Samples rendered after KOFF is written.
    pub release_samples: u32,
    pub counter_start: u32,
}

/// Audition ARAM layout: the directory page and the sample start.
pub const AUDITION_DIR: u8 = 0x02;
pub const AUDITION_START: usize = 0x0300;

pub struct Audition {
    pub frames: Vec<[i16; 2]>,
    pub report: Value,
}

/// Checks that the sample ends exactly at its slice end and, if looping, that
/// the loop offset is an aligned block inside it. Returns whether it loops.
pub fn check_sample(brr: &[u8], loop_offset: Option<usize>) -> Result<bool, String> {
    if brr.is_empty() || !brr.len().is_multiple_of(9) || brr.len() > 65536 - AUDITION_START {
        return Err("BRR audition needs complete nine-byte blocks within 64 KiB of ARAM".into());
    }
    let last = brr.len() - 9;
    if (0..last).step_by(9).any(|i| brr[i] & 1 != 0) || brr[last] & 1 == 0 {
        return Err("the BRR end flag must be in the final block only".into());
    }
    let looping = brr[last] & 2 != 0;
    if looping && !loop_offset.is_some_and(|o| o < brr.len() && o.is_multiple_of(9)) {
        return Err("a looping BRR sample needs an aligned in-sample loop offset".into());
    }
    if !looping && loop_offset.is_some() {
        return Err("the end block has no loop flag; omit the loop offset".into());
    }
    Ok(looping)
}

/// ARAM holding a checked sample at the audition layout: directory entry 0
/// at page [`AUDITION_DIR`] points to [`AUDITION_START`] and its loop block.
pub fn audition_ram(brr: &[u8], loop_offset: Option<usize>) -> Vec<u8> {
    let mut ram = vec![0u8; 65536];
    let start = AUDITION_START;
    let loop_address = start + loop_offset.unwrap_or(0);
    let directory = AUDITION_DIR as usize * 256;
    ram[directory..directory + 2].copy_from_slice(&(start as u16).to_le_bytes());
    ram[directory + 2..directory + 4].copy_from_slice(&(loop_address as u16).to_le_bytes());
    ram[start..start + brr.len()].copy_from_slice(brr);
    ram
}

/// Signed left/right register pair such as VxVOLL/VxVOLR or MVOLL/MVOLR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StereoVolume {
    pub left: i8,
    pub right: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DspEnvelope {
    /// Explicit VxADSR1, VxADSR2 and VxGAIN. ADSR1 bit 7 selects ADSR, otherwise GAIN.
    Registers { adsr1: u8, adsr2: u8, gain: u8 },
    /// The bank's own envelope index, such as a native track's `settings.adsr`:
    /// below 128 an ADSR1/ADSR2 pair from the declared table; 128 is the reviewed
    /// Compile drivers' fixed GAIN. GAIN programs (above 128) are driver scripts and refuse.
    BankTable { index: u8 },
}

/// ADSR1, ADSR2 and GAIN for an envelope description, with how they were
/// obtained. `bank` is the saved bank of a bank slot, if any.
pub fn envelope_registers(
    envelope: DspEnvelope,
    bank: Option<&crate::instruments::InstrumentBank>,
) -> Result<(u8, u8, u8, Value), String> {
    match envelope {
        DspEnvelope::Registers { adsr1, adsr2, gain } => {
            Ok((adsr1, adsr2, gain, json!({"kind": "registers"})))
        }
        DspEnvelope::BankTable { index } => {
            let bank =
                bank.ok_or("bank_table needs a bank slot; give a plan explicit registers")?;
            let reviewed = crate::compile_instruments::is_supported(bank);
            match index {
                0..=127 => {
                    let pair = bank.envelope(index)?;
                    if pair[0] & 0x80 == 0 {
                        return Err("this table pair does not select ADSR (ADSR1 bit 7 clear), so the driver's GAIN value would matter; give explicit registers".into());
                    }
                    Ok((
                        pair[0],
                        pair[1],
                        0,
                        json!({"kind": "bank_table", "index": index, "bytes": pair,
                            "interpretation": if reviewed {"reviewed Compile driver: index below 128 selects a direct ADSR1/ADSR2 pair"} else {"declared layout: entry bytes applied as ADSR1, ADSR2; driver unreviewed"}}),
                    ))
                }
                128 if reviewed => Ok((
                    0,
                    0,
                    0x7f,
                    json!({"kind": "bank_table", "index": 128,
                        "interpretation": "reviewed Compile driver: 0x80 selects fixed GAIN 0x7F"}),
                )),
                _ => Err("this index selects a driver GAIN program or has no reviewed meaning; render it with explicit registers".into()),
            }
        }
    }
}

/// How MIDI notes map to the 14-bit pitch register.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteRoot {
    /// `root_register` sounds `root_note`.
    Register { root_note: u8, root_register: u16 },
    /// A sample plan's declared-rate register sounds its recorded root.
    Plan {
        declared_rate_register: f64,
        root_note: f64,
        root_cents: f64,
    },
}

/// The equal-tempered register for `note`, rounded, with its derivation.
pub fn note_register(note: u8, root: NoteRoot) -> Result<(u16, Value), String> {
    let (value, detail) = match root {
        NoteRoot::Plan {
            declared_rate_register: unity,
            root_note,
            root_cents: cents,
        } => {
            if note > 127 {
                return Err("notes are MIDI 0..127".into());
            }
            let exact = unity * ((note as f64 - root_note - cents / 100.0) / 12.0).exp2();
            let rounded = exact.round();
            (
                rounded,
                json!({"kind": "plan_note", "note": note, "root_note": root_note, "root_cents": cents,
                    "declared_rate_register": unity, "exact_register": exact,
                    "rounding_cents": 1200.0 * (rounded.max(1.0) / exact).log2()}),
            )
        }
        NoteRoot::Register {
            root_note,
            root_register,
        } => {
            if !(1..=0x3fff).contains(&root_register) || note > 127 || root_note > 127 {
                return Err("root_register is 1..0x3FFF and notes are MIDI 0..127".into());
            }
            let exact = root_register as f64 * 2f64.powf((note as f64 - root_note as f64) / 12.0);
            let rounded = exact.round();
            let cents = 1200.0 * (rounded.max(1.0) / exact).log2();
            (
                rounded,
                json!({"kind": "note", "note": note, "root_note": root_note, "root_register": root_register,
                    "exact_register": exact, "rounding_cents": cents}),
            )
        }
    };
    check_pitch_register(value)?;
    Ok((value as u16, detail))
}

/// Refuses register values outside 1..0x3FFF.
pub fn check_pitch_register(value: f64) -> Result<(), String> {
    if (1.0..=16383.0).contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "the pitch register is 14 bits; {value} is outside 1..0x3FFF. Choose another note or root"
        ))
    }
}

/// Plays one note: KON at sample 0, KOFF written after `key_on_samples`.
pub fn audition(setup: &VoiceSetup) -> Result<Audition, String> {
    let looping = check_sample(&setup.brr, setup.loop_offset)?;
    if setup.pitch > 0x3fff {
        return Err("the pitch register is 14 bits (0..0x3FFF)".into());
    }
    let total = setup.key_on_samples as u64 + setup.release_samples as u64;
    if total == 0 || total > MAX_SAMPLES as u64 {
        return Err(format!(
            "render 1..{MAX_SAMPLES} DSP samples in total (key-on plus release)"
        ));
    }
    if setup.counter_start >= COUNTER_RANGE {
        return Err("counter_start must be below 0x7800".into());
    }
    let start = AUDITION_START;
    let loop_address = start + setup.loop_offset.unwrap_or(0);
    let mut dsp = Dsp::new(
        audition_ram(&setup.brr, setup.loop_offset),
        setup.counter_start,
    );
    let writes: Vec<(usize, u8)> = vec![
        (FLG, 0x20),
        (DIR, AUDITION_DIR),
        (MVOLL, setup.master[0] as u8),
        (MVOLR, setup.master[1] as u8),
        (VOLL, setup.volume[0] as u8),
        (VOLR, setup.volume[1] as u8),
        (PITCHL, setup.pitch as u8),
        (PITCHH, (setup.pitch >> 8) as u8),
        (SRCN, 0),
        (ADSR1, setup.adsr1),
        (ADSR2, setup.adsr2),
        (GAIN, setup.gain),
        (KON, 1),
    ];
    for &(register, value) in &writes {
        dsp.write(register, value);
    }
    let mut frames = Vec::with_capacity(total as usize);
    let mut key_off_level = None;
    let mut silent_from = None;
    for index in 0..total {
        if index == setup.key_on_samples as u64 {
            dsp.write(KOFF, 1);
            key_off_level = Some(dsp.envelope(0));
        }
        let frame = dsp.tick();
        if dsp.phase(0) == Phase::Release && dsp.envelope(0) == 0 {
            silent_from.get_or_insert(index + 1);
        } else {
            silent_from = None;
        }
        frames.push(frame);
    }
    let loops: Vec<u64> = dsp
        .events
        .iter()
        .filter(|e| e.kind == "loop")
        .map(|e| e.sample)
        .collect();
    let events: Vec<&VoiceEvent> = dsp.events.iter().filter(|e| e.kind != "loop").collect();
    let peak = |channel: usize| {
        frames
            .iter()
            .map(|f| (f[channel] as i32).abs())
            .max()
            .unwrap_or(0)
    };
    let first_audible = frames.iter().position(|f| f != &[0, 0]);
    let hex = |value: u8| format!("0x{value:02X}");
    let report = json!({
        "registers": writes.iter().map(|(r, v)| json!({"register": format!("0x{r:02X}"), "value": hex(*v)})).collect::<Vec<_>>(),
        "voice": 0,
        "pitch_register": setup.pitch,
        "pitch_register_hex": format!("0x{:04X}", setup.pitch),
        "playback_rate_ratio": setup.pitch as f64 / 4096.0,
        "envelope_mode": if setup.adsr1 & 0x80 != 0 { "adsr" } else if setup.gain & 0x80 == 0 { "direct_gain" } else { "custom_gain" },
        "adsr1": hex(setup.adsr1), "adsr2": hex(setup.adsr2), "gain": hex(setup.gain),
        "volume": {"left": setup.volume[0], "right": setup.volume[1]},
        "master_volume": {"left": setup.master[0], "right": setup.master[1]},
        "aram": {"directory_page": hex(AUDITION_DIR), "srcn": 0, "start": format!("0x{start:04X}"),
            "loop": format!("0x{loop_address:04X}"), "bytes": setup.brr.len(), "blocks": setup.brr.len() / 9,
            "looping": looping, "layout": "audition layout, not the native ARAM layout"},
        "timing": {"rate_nominal_hz": SAMPLE_RATE, "unit": "DSP samples", "key_on_written_before_sample": 0,
            "key_off_written_before_sample": setup.key_on_samples, "total_samples": total,
            "poll": "KON/KOFF are polled on even samples; the first data sample follows five empty samples",
            "counter_start": setup.counter_start, "first_audible_sample": first_audible,
            "silent_from_sample": silent_from},
        "envelope_at_key_off": key_off_level,
        "loops": {"count": loops.len(), "first_samples": loops.iter().take(8).collect::<Vec<_>>()},
        "events": events,
        "peak": {"left": peak(0), "right": peak(1)},
        "modelled": "BRR decode with history, 12-sample ring, Gaussian interpolation, ADSR/GAIN with counter offsets, KON delay, KOFF release, end/loop flags, VxVOL, voice clamp, MVOL",
        "not_modelled": "echo/FIR, noise, pitch modulation, intra-sample voice cycle order, pre-key-on decode, analog output stage and the console's real sample clock",
    });
    Ok(Audition { frames, report })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block of sixteen equal nibbles.
    fn block(header: u8, nibble: u8) -> Vec<u8> {
        let mut out = vec![header];
        out.extend([nibble << 4 | nibble; 8]);
        out
    }

    fn setup(brr: Vec<u8>, loop_offset: Option<usize>) -> VoiceSetup {
        VoiceSetup {
            brr,
            loop_offset,
            pitch: 0x1000,
            adsr1: 0,
            adsr2: 0,
            gain: 0x7f,
            volume: [64, 32],
            master: [127, 127],
            key_on_samples: 200,
            release_samples: 400,
            counter_start: 0,
        }
    }

    #[test]
    fn gauss_rows_sum_to_the_documented_range() {
        for i in 0..256 {
            let sum = GAUSS[i] + GAUSS[255 - i] + GAUSS[256 + i] + GAUSS[511 - i];
            assert!((0x7ff..=0x801).contains(&sum), "{i}: {sum:#x}");
        }
        assert_eq!(
            (GAUSS[255], GAUSS[511], GAUSS[256], GAUSS[0]),
            (0x172, 0x519, 0x176, 0)
        );
    }

    #[test]
    fn key_on_outputs_five_empty_samples_then_the_second_decoded_sample_dominates() {
        // Shift 12 filter 0 nibble 1: every decoded 15-bit sample is 2048.
        let brr = [block(0xc0, 1), block(0xc3, 1)].concat();
        let out = audition(&setup(brr, Some(9))).unwrap();
        // Indices 0..=5 are KON (#0) and the five empty samples (#1..#5).
        assert!(out.frames[..6].iter().all(|f| f == &[0, 0]));
        // #6: interpolation of four equal samples (weights sum 0x801 at i=0),
        // direct gain 0x7F0, voice volume 64 then master 127.
        let interp =
            (((0x172 * 2048) >> 10) + ((0x519 * 2048) >> 10) + ((0x176 * 2048) >> 10)) >> 1;
        let env = (interp * 0x7f0) >> 11;
        let left = (((env * 64) >> 6) * 127) >> 7;
        assert_eq!(out.frames[6][0] as i32, left);
        let right = (((env * 32) >> 6) * 127) >> 7;
        assert_eq!(out.frames[6][1] as i32, right);
        assert_eq!(out.report["timing"]["first_audible_sample"], 6);
    }

    #[test]
    fn interpolation_wraps_the_partial_sum_and_clamps_the_last_term() {
        // Repeated -8 << 12 samples: fullsnes documents +0x3FF8 instead of
        // -0x4000 where the three weights sum to 0x801.
        let low = Voice {
            ring: [-32768; 12],
            ..Voice::default()
        };
        assert_eq!(interpolate(&low), 0x3ff8);
        let high = Voice {
            ring: [32766; 12],
            ..Voice::default()
        };
        assert!(interpolate(&high) <= 0x3fff);
    }

    #[test]
    fn loop_reenters_with_carried_history_and_one_shot_mutes_its_end_block() {
        // Filter 1 at the loop block depends on history from the end block.
        let looping = [block(0xc0, 2), block(0xc4, 1), block(0xc7, 0xf)].concat();
        let out = audition(&VoiceSetup {
            pitch: 0x3fff,
            key_on_samples: 400,
            ..setup(looping, Some(9))
        })
        .unwrap();
        assert!(out.report["loops"]["count"].as_u64().unwrap() > 10);
        assert!(out.frames[200..400].iter().any(|f| f[0] != 0));
        // One-shot: the final end block is never heard; the voice mutes as
        // soon as its header is loaded and stays silent.
        let one_shot = [block(0xc0, 3), block(0xc0, 3), block(0xc1, 7)].concat();
        let out = audition(&setup(one_shot, None)).unwrap();
        let mute = out.report["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "end_without_loop")
            .unwrap()["sample"]
            .as_u64()
            .unwrap() as usize;
        assert!(out.frames[mute + 1..].iter().all(|f| f == &[0, 0]));
        assert!(mute < 6 + 32, "two blocks at unit pitch last 32 samples");
    }

    #[test]
    fn adsr_follows_the_counter_rates_and_release_steps_by_eight() {
        let brr = [block(0xc0, 1), block(0xc3, 1)].concat();
        // Attack rate 31 (+1024 per sample), decay rate 30, sustain level 3,
        // sustain rate 0 (hold).
        let out = audition(&VoiceSetup {
            adsr1: 0x8f | (7 << 4),
            adsr2: 3 << 5,
            gain: 0,
            key_on_samples: 1000,
            ..setup(brr.clone(), Some(9))
        })
        .unwrap();
        let events = out.report["events"].as_array().unwrap();
        let decay = events.iter().find(|e| e["kind"] == "decay").unwrap();
        assert_eq!(decay["value"], 0x7ff);
        let sustain = events.iter().find(|e| e["kind"] == "sustain").unwrap();
        // The phase changes on the candidate level even when the counter does
        // not commit it, so the held level sits near the (SL + 1) * 0x100 boundary.
        let level = out.report["envelope_at_key_off"].as_i64().unwrap();
        assert_eq!(sustain["value"].as_i64().unwrap(), level);
        assert!((0x3f8..=0x408).contains(&level), "{level:#x}");
        // Release: -8 per sample from the key-off poll to zero.
        let silent = out.report["timing"]["silent_from_sample"].as_u64().unwrap();
        assert_eq!(silent, 1000 + (level as u64).div_ceil(8));
        // From the highest sustain level the release still ends within the
        // bound check_project counts, which adds 2 samples for the poll.
        let loud = audition(&VoiceSetup {
            adsr1: 0x8f | (7 << 4),
            adsr2: 7 << 5,
            gain: 0,
            key_on_samples: 1000,
            ..setup(brr.clone(), Some(9))
        })
        .unwrap();
        assert!(loud.report["envelope_at_key_off"].as_i64().unwrap() >= 0x7f0);
        let silent = loud.report["timing"]["silent_from_sample"]
            .as_u64()
            .unwrap();
        assert!(
            silent - 1000 + 2 <= u64::from(RELEASE_BOUND_SAMPLES),
            "{silent}"
        );
        // Slow attack: rate 1 (2048 samples per step) never reaches decay here.
        let slow = audition(&VoiceSetup {
            adsr1: 0x80,
            adsr2: 0,
            gain: 0,
            ..setup(brr, Some(9))
        })
        .unwrap();
        assert!(slow.report["envelope_at_key_off"].as_i64().unwrap() <= 32);
    }

    #[test]
    fn counter_offsets_shift_the_first_envelope_step() {
        // Linear increase GAIN rate 21 (period 20, offset 536): with the
        // counter decrementing each sample, steps land where (c + 536) % 20 == 0.
        let brr = [block(0xc0, 1), block(0xc3, 1)].concat();
        let mut dsp_steps = Vec::new();
        for start in [0u32, 7] {
            let mut ram = vec![0u8; 65536];
            ram[0x200..0x204].copy_from_slice(&[0x00, 0x03, 0x09, 0x03]);
            ram[0x300..0x300 + brr.len()].copy_from_slice(&brr);
            let mut dsp = Dsp::new(ram, start);
            for (r, v) in [(DIR, 2), (PITCHH, 0x10), (GAIN, 0xc0 | 21), (KON, 1)] {
                dsp.write(r, v);
            }
            let mut first = None;
            for i in 0..200u32 {
                dsp.tick();
                if dsp.envelope(0) > 0 && first.is_none() {
                    first = Some(i);
                }
            }
            let first = first.unwrap();
            let counter = (start + COUNTER_RANGE - first) % COUNTER_RANGE;
            assert_eq!((counter + 536) % 20, 0);
            dsp_steps.push(first);
        }
        assert_ne!(dsp_steps[0], dsp_steps[1]);
    }

    #[test]
    fn bent_gain_slows_above_0x600_and_exponential_decrease_follows_the_formula() {
        let brr = [block(0xc0, 1), block(0xc3, 1)].concat();
        let mut ram = vec![0u8; 65536];
        ram[0x200..0x204].copy_from_slice(&[0x00, 0x03, 0x09, 0x03]);
        ram[0x300..0x300 + brr.len()].copy_from_slice(&brr);
        let mut dsp = Dsp::new(ram, 0);
        for (r, v) in [(DIR, 2), (PITCHH, 0x10), (GAIN, 0xe0 | 31), (KON, 1)] {
            dsp.write(r, v);
        }
        let mut levels = Vec::new();
        for _ in 0..120 {
            dsp.tick();
            levels.push(dsp.envelope(0));
        }
        let steps: Vec<i32> = levels.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(steps.contains(&32) && steps.contains(&8));
        assert!(levels.iter().all(|&l| l <= 0x7ff));
        dsp.write(GAIN, 0xa0 | 31);
        let before = dsp.envelope(0);
        dsp.tick();
        assert_eq!(dsp.envelope(0), before - 1 - ((before - 1) >> 8));
    }

    #[test]
    fn malformed_samples_and_registers_refuse() {
        assert!(audition(&setup(vec![0; 10], None)).is_err());
        assert!(
            audition(&setup(block(0xc0, 1), None)).is_err(),
            "no end flag"
        );
        assert!(
            audition(&setup(block(0xc3, 1), None)).is_err(),
            "loop needs offset"
        );
        assert!(audition(&setup(block(0xc1, 1), Some(0))).is_err());
        let ok = setup(block(0xc1, 1), None);
        assert!(
            audition(&VoiceSetup {
                pitch: 0x4000,
                ..ok.clone()
            })
            .is_err()
        );
        assert!(
            audition(&VoiceSetup {
                counter_start: COUNTER_RANGE,
                ..ok.clone()
            })
            .is_err()
        );
        assert!(
            audition(&VoiceSetup {
                key_on_samples: 0,
                release_samples: 0,
                ..ok
            })
            .is_err()
        );
    }
}
