//! WonderSwan sound unit (WonderSwan, WonderSwan Color, SwanCrystal): four
//! channels playing 32-sample, 4-bit wavetables from internal RAM, channel 2
//! voice (8-bit PCM) mode, the channel 3 frequency sweep, channel 4 LFSR
//! noise with eight documented tap modes, and the stereo headphone mix.
//!
//! References:
//! the WSdev wiki "Sound" page (mixing diagram, wavetable RAM layout and nibble
//! order, ports $80-$9E with the frequency, volume, voice, sweep, noise,
//! wavetable address, channel control and output control fields, the LFSR
//! algorithm and its sequence lengths) and the VGM 1.71 specification
//! (WonderSwan clock 0xC0, commands 0xBC and 0xC6). Behaviour the documents
//! leave open is a declared choice in [`model_scope`].
use crate::psg::{PsgNote, PsgPitch};
use crate::sound_chip::{Chip, ChipWrite, Target};
use serde_json::{Value, json};

/// The VGM header's typical clock: the 3.072 MHz CPU clock.
pub const CLOCK_HZ: u32 = 3_072_000;
pub const MIN_CLOCK_HZ: u32 = 1_000_000;
pub const MAX_CLOCK_HZ: u32 = 8_000_000;
/// One model tick is one CPU cycle.
pub const TICK_DIVIDER: u32 = 1;
/// CPU cycles per output sample: the documented 24000 Hz output at 3.072 MHz.
pub const FRAME_TICKS: u32 = 128;
/// Cycles per tick of the 375 Hz sweep clock.
pub const SWEEP_PRESCALER: u32 = 8192;
pub const CHANNELS: usize = 4;
/// Internal RAM addresses (the sound unit reads at most $3FFF).
pub const MEMORY_BYTES: usize = 0x10000;

/// The LFSR tap bit of each $8E tap mode and its documented sequence length.
pub const TAPS: [(u32, u32); 8] = [
    (14, 32767),
    (10, 1953),
    (13, 254),
    (4, 217),
    (8, 73),
    (6, 63),
    (9, 42),
    (11, 28),
];

const PORT_NAMES: [&str; 0x20] = [
    "ch1_frequency_low",
    "ch1_frequency_high",
    "ch2_frequency_low",
    "ch2_frequency_high",
    "ch3_frequency_low",
    "ch3_frequency_high",
    "ch4_frequency_low",
    "ch4_frequency_high",
    "ch1_volume",
    "ch2_volume_or_voice_sample",
    "ch3_volume",
    "ch4_volume",
    "ch3_sweep_amount",
    "ch3_sweep_ticks",
    "ch4_noise_control",
    "wavetable_address",
    "channel_control",
    "output_control",
    "noise_lfsr_low_read",
    "noise_lfsr_high_read",
    "ch2_voice_volume",
    "sound_test",
    "output_right_low_read",
    "output_right_high_read",
    "output_left_low_read",
    "output_left_high_read",
    "output_sum_low_read",
    "output_sum_high_read",
    "undefined_9c",
    "undefined_9d",
    "speaker_main_volume",
    "undefined_9f",
];

/// Name of the port 0x80 + `offset` (offsets 0x00..0x1F).
pub fn port_name(offset: u8) -> &'static str {
    PORT_NAMES
        .get(usize::from(offset))
        .copied()
        .unwrap_or("outside_the_sound_ports")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsConfig {
    pub clock_hz: u32,
}

impl WsConfig {
    pub fn new(clock_hz: u32) -> Self {
        Self { clock_hz }
    }
    pub fn to_json(&self) -> Value {
        json!({"variant":"wonderswan","clock_hz":self.clock_hz,
            "tick":"one CPU cycle",
            "native_sample":format!("the mix sampled once every {FRAME_TICKS} cycles (clock/{FRAME_TICKS}, 24000 Hz at 3.072 MHz), held for the whole sample"),
            "sample_rate":"clock / (2048 - divisor) per wavetable sample; a 32-sample wave sounds at clock / (32 (2048 - divisor))",
            "sweep_clock":format!("clock / {SWEEP_PRESCALER} (375 Hz at 3.072 MHz)"),
            "output":"the headphone path: (left and right sums of the four channels, 10 bits each) << 5, unipolar, DC kept",
            "power_on_state":"ports 0, channels and noise off, sample indices 0, LFSR 0, internal RAM 0"})
    }
}

pub fn check_clock(clock_hz: u32) -> Result<(), String> {
    if (MIN_CLOCK_HZ..=MAX_CLOCK_HZ).contains(&clock_hz) {
        Ok(())
    } else {
        Err(format!(
            "clock_hz must be {MIN_CLOCK_HZ}..{MAX_CLOCK_HZ}, such as {CLOCK_HZ} (the WonderSwan CPU clock, as VGM files give it)"
        ))
    }
}

/// Wavetable sample rate clock / (2048 - divisor).
pub fn sample_rate_hz(clock_hz: u32, divisor: u16) -> f64 {
    f64::from(clock_hz) / f64::from(2048 - u32::from(divisor & 0x7ff))
}

/// One LFSR step: the new bit is the inverted xor of bit 7 and the tap bit;
/// the register shifts left and takes the new bit as bit 0.
pub fn lfsr_step(lfsr: u16, tap_mode: u8) -> u16 {
    let tap = TAPS[usize::from(tap_mode & 7)].0;
    let bit = !((lfsr >> 7) ^ (lfsr >> tap)) & 1;
    ((lfsr << 1) | bit) & 0x7fff
}

#[derive(Debug, Clone, Copy)]
struct Channel {
    counter: u32,
    index: u8,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            counter: 2048,
            index: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WonderSwan {
    config: WsConfig,
    ports: [u8; 0x20],
    memory: Vec<u8>,
    memory_writes: u64,
    channels: [Channel; CHANNELS],
    lfsr: u16,
    sweep_prescaler: u32,
    sweep_counter: u32,
    sweep_steps: u64,
    latch: [i32; 2],
    cycles: u64,
    /// Channels left out of the output mix (bit c = 0-based channel c); chip
    /// state is unaffected.
    output_mute: u8,
}

impl WonderSwan {
    /// The declared power-on state.
    pub fn new(config: WsConfig) -> Self {
        Self {
            config,
            ports: [0; 0x20],
            memory: vec![0; MEMORY_BYTES],
            memory_writes: 0,
            channels: Default::default(),
            lfsr: 0,
            sweep_prescaler: SWEEP_PRESCALER,
            sweep_counter: 1,
            sweep_steps: 0,
            latch: [0; 2],
            cycles: 0,
            output_mute: 0,
        }
    }

    fn divisor(&self, channel: usize) -> u16 {
        u16::from_le_bytes([self.ports[2 * channel], self.ports[2 * channel + 1] & 7])
    }

    fn enabled(&self, channel: usize) -> bool {
        self.ports[0x10] & (1 << channel) != 0
    }

    fn wave_base(&self) -> usize {
        usize::from(self.ports[0x0f]) << 6
    }

    /// The 4-bit sample `index` of channel `channel`'s wavetable: byte k holds
    /// sample 2k in its low nibble and sample 2k + 1 in its high nibble.
    pub fn wave_sample(&self, channel: usize, index: u8) -> u8 {
        let byte = self.memory[self.wave_base() + 16 * channel + usize::from(index / 2)];
        if index & 1 == 0 { byte & 15 } else { byte >> 4 }
    }

    /// Per-channel (left, right) 8-bit outputs; `None` for a silent channel.
    pub fn channel_outputs(&self) -> [Option<[u32; 2]>; CHANNELS] {
        let control = self.ports[0x10];
        let mut out = [None; CHANNELS];
        for (c, slot) in out.iter_mut().enumerate() {
            if c == 1 && control & 0x20 != 0 {
                let sample = u32::from(self.ports[0x09]);
                let v = self.ports[0x14];
                let side = |full: u8, half: u8| {
                    if v & full != 0 {
                        sample
                    } else if v & half != 0 {
                        sample >> 1
                    } else {
                        0
                    }
                };
                *slot = Some([side(4, 8), side(1, 2)]);
                continue;
            }
            if !self.enabled(c) {
                continue;
            }
            let sample = if c == 3 && control & 0x80 != 0 {
                if self.lfsr & 1 != 0 { 15 } else { 0 }
            } else {
                self.wave_sample(c, self.channels[c].index)
            };
            let volume = self.ports[0x08 + c];
            *slot = Some([
                u32::from(sample) * u32::from(volume >> 4),
                u32::from(sample) * u32::from(volume & 15),
            ]);
        }
        out
    }

    /// The headphone path: (10-bit left and right sums) << 5.
    fn mix(&self) -> [i32; 2] {
        self.mix_muted(0)
    }

    /// [`WonderSwan::mix`] without the channels in `mute` (bit c = channel c).
    fn mix_muted(&self, mute: u8) -> [i32; 2] {
        let mut sum = [0u32; 2];
        for out in self
            .channel_outputs()
            .into_iter()
            .enumerate()
            .filter(|(c, _)| mute & (1 << c) == 0)
            .filter_map(|(_, out)| out)
        {
            sum[0] += out[0];
            sum[1] += out[1];
        }
        sum.map(|s| ((s & 0x3ff) << 5) as i32)
    }

    /// One write to I/O port 0x80 + `offset`.
    pub fn write_port(&mut self, offset: u8, data: u8) -> Value {
        let port = 0x80 + u16::from(offset);
        let address = format!("${port:02X}");
        if offset >= 0x20 {
            return json!({"port":address,"data":data,"ignored":"outside the sound ports $80..$9F"});
        }
        let index = usize::from(offset);
        let before = self.ports[index];
        let mut effect =
            json!({"port":address,"name":port_name(offset),"before":before,"after":data});
        match offset {
            0x12 | 0x13 | 0x16..=0x1b => {
                effect["ignored"] = json!("a read-only port; the write changes nothing");
                return effect;
            }
            0x1c | 0x1d | 0x1f => {
                effect["ignored"] = json!("not a documented sound port");
                return effect;
            }
            _ => {}
        }
        self.ports[index] = data;
        match offset {
            0x00..=0x07 => {
                let c = index / 2;
                let divisor = self.divisor(c);
                effect["channel"] = json!(c + 1);
                effect["divisor"] = json!(divisor);
                effect["sample_rate_hz"] = json!(sample_rate_hz(self.config.clock_hz, divisor));
                effect["wave_frequency_hz"] =
                    json!(sample_rate_hz(self.config.clock_hz, divisor) / 32.0);
            }
            0x08..=0x0b => {
                let c = index - 8;
                effect["channel"] = json!(c + 1);
                effect["left"] = json!(data >> 4);
                effect["right"] = json!(data & 15);
                if c == 1 {
                    effect["voice_sample"] = json!(data);
                    effect["note"] = json!(
                        "port $89 is channel 2's volume in wavetable mode and its unsigned 8-bit sample in voice mode"
                    );
                }
            }
            0x0c => {
                effect["sweep_amount"] = json!(data as i8);
            }
            0x0d => {
                self.sweep_counter = u32::from(data & 31) + 1;
                effect["sweep_ticks"] = json!((data & 31) + 1);
                effect["sweep_period_seconds"] = json!(
                    f64::from(u32::from(data & 31) + 1) * f64::from(SWEEP_PRESCALER)
                        / f64::from(self.config.clock_hz)
                );
                effect["note"] = json!("the sweep counter restarts");
            }
            0x0e => {
                let (tap, length) = TAPS[usize::from(data & 7)];
                effect["tap_mode"] = json!(data & 7);
                effect["tap_bit"] = json!(tap);
                effect["sequence_length"] = json!(length);
                effect["lfsr_enabled"] = json!(data & 0x10 != 0);
                if data & 0x08 != 0 {
                    self.lfsr = 0;
                    effect["lfsr_reset"] = json!(true);
                }
            }
            0x0f => {
                effect["wavetable_address"] = json!(format!("0x{:04X}", self.wave_base()));
                effect["channel_wavetables"] = json!(
                    (0..4)
                        .map(|c| format!("0x{:04X}", self.wave_base() + 16 * c))
                        .collect::<Vec<_>>()
                );
            }
            0x10 => {
                effect["enabled"] = json!((0..4).map(|c| data & (1 << c) != 0).collect::<Vec<_>>());
                effect["ch2_voice"] = json!(data & 0x20 != 0);
                effect["ch3_sweep"] = json!(data & 0x40 != 0);
                effect["ch4_noise"] = json!(data & 0x80 != 0);
            }
            0x11 => {
                effect["speaker_enabled"] = json!(data & 1 != 0);
                effect["speaker_shift"] = json!((data >> 1) & 3);
                effect["headphone_enabled"] = json!(data & 8 != 0);
                effect["note"] = json!(
                    "bit 7 (headphones connected) is a status bit; Tunemend renders the headphone path whatever this port holds"
                );
            }
            0x14 => {
                let level = |full: u8, half: u8| {
                    if data & full != 0 {
                        "100%"
                    } else if data & half != 0 {
                        "50%"
                    } else {
                        "off"
                    }
                };
                effect["voice_left"] = json!(level(4, 8));
                effect["voice_right"] = json!(level(1, 2));
            }
            0x15 => {
                effect["note"] = json!(
                    "sound test port: decoded, not modelled (holds, forced wavetables and the sweep clock source keep their normal behaviour)"
                );
            }
            _ => {
                effect["speaker_volume"] = json!(data & 3);
                effect["note"] = json!("affects the internal speaker only, which is not rendered");
            }
        }
        effect
    }

    /// One byte written to internal RAM.
    pub fn write_memory(&mut self, address: u16, data: u8) -> Value {
        let at = usize::from(address);
        let before = self.memory[at];
        self.memory[at] = data;
        self.memory_writes += 1;
        let mut effect =
            json!({"memory_address":format!("0x{address:04X}"),"before":before,"after":data});
        let base = self.wave_base();
        if (base..base + 64).contains(&at) {
            let offset = at - base;
            effect["wavetable"] = json!({"channel":offset / 16 + 1,"samples":[2 * (offset % 16), 2 * (offset % 16) + 1],
                "values":[data & 15, data >> 4]});
        }
        if at > 0x3fff {
            effect["note"] =
                json!("above $3FFF: the sound unit's wavetable address never reaches it");
        }
        effect
    }

    /// One CPU cycle: sample counters, the noise LFSR and the sweep.
    fn step(&mut self) {
        self.cycles += 1;
        for c in 0..CHANNELS {
            if !self.enabled(c) {
                continue;
            }
            let reload = 2048 - u32::from(self.divisor(c));
            let channel = &mut self.channels[c];
            channel.counter -= 1;
            if channel.counter > 0 {
                continue;
            }
            channel.counter = reload;
            self.channels[c].index = (self.channels[c].index + 1) & 31;
            if c == 3 && self.ports[0x0e] & 0x10 != 0 {
                self.lfsr = lfsr_step(self.lfsr, self.ports[0x0e]);
            }
        }
        self.sweep_prescaler -= 1;
        if self.sweep_prescaler == 0 {
            self.sweep_prescaler = SWEEP_PRESCALER;
            if self.enabled(2) && self.ports[0x10] & 0x40 != 0 {
                self.sweep_counter -= 1;
                if self.sweep_counter == 0 {
                    self.sweep_counter = u32::from(self.ports[0x0d] & 31) + 1;
                    let divisor =
                        (i32::from(self.divisor(2)) + i32::from(self.ports[0x0c] as i8)) & 0x7ff;
                    self.ports[0x04] = divisor as u8;
                    self.ports[0x05] = (divisor >> 8) as u8;
                    self.sweep_steps += 1;
                }
            }
        }
    }
}

impl Chip for WonderSwan {
    const TICKS_PER_FRAME: u32 = FRAME_TICKS;
    const OUTPUT_DIVISOR: i64 = 1;
    fn mute_output(&mut self, mask: u8) {
        self.output_mute = mask;
    }
    fn apply(&mut self, target: Target, data: u8) -> Value {
        match target {
            Target::Register(offset) => self.write_port(offset, data),
            Target::Memory(address) => self.write_memory(address, data),
            _ => json!({"ignored":"not a WonderSwan port or memory address","data":data}),
        }
    }
    /// The mix is sampled at the first cycle of each output sample, after the
    /// writes of that cycle, and held for the sample's 128 cycles.
    fn tick(&mut self) -> [i32; 2] {
        if self.cycles.is_multiple_of(u64::from(FRAME_TICKS)) {
            self.latch = self.mix_muted(self.output_mute);
        }
        self.step();
        self.latch
    }
    fn state(&self) -> Value {
        let outputs = self.channel_outputs();
        let channels: Vec<Value> = (0..CHANNELS)
            .map(|c| {
                json!({"channel":c + 1,"divisor":self.divisor(c),"volume":format!("0x{:02x}", self.ports[8 + c]),
                    "sample_index":self.channels[c].index,"enabled":self.enabled(c),"output":outputs[c],
                    "wavetable":(0..32).map(|i| self.wave_sample(c, i)).collect::<Vec<_>>()})
            })
            .collect();
        let mix = self.mix();
        json!({"ports":self.ports.iter().map(|p| format!("0x{p:02x}")).collect::<Vec<_>>(),
            "channel_control":format!("0x{:02x}", self.ports[0x10]),"wavetable_address":format!("0x{:04X}", self.wave_base()),
            "channels":channels,"noise_lfsr":format!("0x{:04x}", self.lfsr),"sweep_steps":self.sweep_steps,
            "memory_writes":self.memory_writes,"headphone_output":mix,"cycles":self.cycles})
    }
}

/// Setup written at tick 0 by note renders: wavetables at RAM $0000 and the
/// headphone output enabled.
pub const NOTE_SETUP: [(u8, u8); 2] = [(0x0f, 0x00), (0x11, 0x08)];

/// Register and memory writes for WonderSwan notes on channels 0..3
/// (channels 1..4). Tone notes take pitch, volume (0..15, both sides) and a
/// 32-sample wavetable (0..15) written to RAM; channel 3 noise notes take a
/// pitch period (the divisor setting the LFSR rate), noise.taps (0..7) and
/// volume. The channel control port follows every start and end.
pub fn note_writes(
    clock_hz: u32,
    notes: &[PsgNote],
) -> Result<(Vec<ChipWrite>, Vec<Value>), (usize, String)> {
    // (tick, class, note index, write); class 0 setup, 1 release, 2 start.
    let mut events: Vec<(u64, u8, usize, Target, u8)> = NOTE_SETUP
        .iter()
        .map(|&(r, d)| (0, 0, 0, Target::Register(r), d))
        .collect();
    // Channel control changes: (tick, class, index, set bits, clear bits).
    let mut control: Vec<(u64, u8, usize, u8, u8)> = Vec::new();
    let mut reports = Vec::new();
    let mut spans: Vec<(u8, u64, u64)> = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        let fail = |message: String| (index, message);
        let max = crate::psg::MAX_RENDER_SECONDS as f64;
        let at = |s: f64| -> Result<u64, (usize, String)> {
            if !(s.is_finite() && (0.0..=max).contains(&s)) {
                return Err((index, format!("times must be 0..{max} seconds")));
            }
            Ok((s * f64::from(clock_hz)).round() as u64)
        };
        if usize::from(note.channel) >= CHANNELS {
            return Err(fail(
                "WonderSwan channels are 0..3 (channels 1..4; noise on 3)".into(),
            ));
        }
        if note.attenuation.is_some()
            || note.envelope.is_some()
            || note.wave.is_some()
            || note.dpcm.is_some()
            || note.duty.is_some()
        {
            return Err(fail(
                "WonderSwan notes take pitch, volume, wavetable or noise.taps; use writes for voice, sweep and stereo levels".into(),
            ));
        }
        if !(note.duration_seconds.is_finite() && note.duration_seconds > 0.0) {
            return Err(fail("duration_seconds must be positive".into()));
        }
        let start = at(note.start_seconds)?;
        let end = at(note.start_seconds + note.duration_seconds)?;
        if end <= start {
            return Err(fail("the note is shorter than one tick".into()));
        }
        if spans
            .iter()
            .any(|&(c, s, e)| c == note.channel && start < e && s < end)
        {
            return Err(fail(format!(
                "overlaps another note on channel {}",
                note.channel
            )));
        }
        spans.push((note.channel, start, end));
        let volume = note
            .volume
            .filter(|v| *v <= 15)
            .ok_or_else(|| fail("volume must be 0..15 (both sides)".into()))?;
        let c = note.channel;
        let mut report = json!({"index":index,"channel":c,"hardware_channel":c + 1,
            "start_tick":start,"end_tick":end,"volume":volume});
        let Some(pitch) = &note.pitch else {
            return Err(fail(
                "notes take pitch (a period for noise, the LFSR rate)".into(),
            ));
        };
        let mut at_start: Vec<(Target, u8)> = Vec::new();
        let mut bits = 1u8 << c;
        if let Some(noise) = &note.noise {
            if c != 3 {
                return Err(fail("noise plays on channel 3 (channel 4)".into()));
            }
            if note.wavetable.is_some()
                || noise.mode.is_some()
                || noise.rate.is_some()
                || noise.period.is_some()
                || noise.divisor.is_some()
                || noise.shift.is_some()
            {
                return Err(fail(
                    "WonderSwan noise takes noise.taps (0..7) with a pitch period and volume"
                        .into(),
                ));
            }
            let taps = noise
                .taps
                .filter(|t| *t <= 7)
                .ok_or_else(|| fail("noise.taps must be 0..7".into()))?;
            let PsgPitch::Period { period } = *pitch else {
                return Err(fail(
                    "noise takes a pitch period (the divisor 0..2047 setting the LFSR rate clock / (2048 - divisor))".into(),
                ));
            };
            if period > 2047 {
                return Err(fail("the divisor must be 0..2047".into()));
            }
            report["noise"] = json!({"taps":taps,"tap_bit":TAPS[usize::from(taps)].0,
                "sequence_length":TAPS[usize::from(taps)].1,"divisor":period,
                "shift_rate_hz":sample_rate_hz(clock_hz, period)});
            at_start.extend([
                (Target::Register(0x0e), 0x18 | taps),
                (Target::Register(0x06), (period & 0xff) as u8),
                (Target::Register(0x07), (period >> 8) as u8),
            ]);
            bits |= 0x80;
        } else {
            let wave = note
                .wavetable
                .as_ref()
                .ok_or_else(|| fail("tone notes need wavetable: 32 samples 0..15".into()))?;
            if wave.len() != 32 || wave.iter().any(|w| *w > 15) {
                return Err(fail("wavetable must be 32 samples 0..15".into()));
            }
            let (divisor, pitch_report) = match *pitch {
                PsgPitch::Period { period } => (
                    f64::from(period),
                    json!({"source":"period","period":period}),
                ),
                PsgPitch::MidiNote { note, a4_hz } => {
                    if note > 127 || !(a4_hz.is_finite() && (220.0..=880.0).contains(&a4_hz)) {
                        return Err(fail("midi note must be 0..127 and a4_hz 220..880".into()));
                    }
                    let target = a4_hz * ((f64::from(note) - 69.0) / 12.0).exp2();
                    let divisor = (2048.0 - f64::from(clock_hz) / (32.0 * target)).round();
                    let actual = f64::from(clock_hz) / (32.0 * (2048.0 - divisor));
                    (
                        divisor,
                        json!({"source":"midi_note","note":note,"a4_hz":a4_hz,"target_hz":target,
                            "cents_error":1200.0 * (actual / target).log2()}),
                    )
                }
            };
            if !(0.0..=2047.0).contains(&divisor) {
                return Err(fail(format!(
                    "divisor {divisor} is outside 0..2047 at this clock"
                )));
            }
            let divisor = divisor as u16;
            report["period"] = json!(divisor);
            report["frequency_hz"] = json!(sample_rate_hz(clock_hz, divisor) / 32.0);
            report["pitch"] = pitch_report;
            let base = 16 * u16::from(c);
            for k in 0..16u16 {
                let byte = wave[usize::from(2 * k)] | (wave[usize::from(2 * k + 1)] << 4);
                at_start.push((Target::Memory(base + k), byte));
            }
            let c2 = 2 * c;
            at_start.extend([
                (Target::Register(c2), (divisor & 0xff) as u8),
                (Target::Register(c2 + 1), (divisor >> 8) as u8),
            ]);
        }
        at_start.push((Target::Register(0x08 + c), (volume << 4) | volume));
        let release = vec![(Target::Register(0x08 + c), 0u8)];
        let show = |w: &[(Target, u8)]| {
            w.iter()
                .map(|(t, d)| match t {
                    Target::Register(r) => {
                        json!({"port":format!("${:02X}", 0x80 + u16::from(*r)),"name":port_name(*r),"data":d})
                    }
                    Target::Memory(a) => json!({"memory_address":format!("0x{a:04X}"),"data":d}),
                    _ => json!({}),
                })
                .collect::<Vec<_>>()
        };
        report["writes"] = json!(show(&at_start));
        report["release_writes"] = json!(show(&release));
        report["channel_control_bits"] = json!(format!("0x{bits:02x}"));
        for (target, data) in at_start {
            events.push((start, 2, index, target, data));
        }
        for (target, data) in release {
            events.push((end, 1, index, target, data));
        }
        control.push((start, 2, index, bits, 0));
        control.push((end, 1, index, 0, bits));
        reports.push(report);
    }
    // The shared channel control port after each change, in time order.
    control.sort_by_key(|&(tick, class, index, ..)| (tick, class, index));
    let mut value = 0u8;
    for (tick, class, index, set, clear) in control {
        value = (value & !clear) | set;
        events.push((tick, class, index, Target::Register(0x10), value));
    }
    // At one tick: setup, then releases, then starts; note order within each,
    // the channel control write last.
    events.sort_by_key(|&(tick, class, index, target, _)| {
        (tick, class, index, target == Target::Register(0x10))
    });
    Ok((
        events
            .into_iter()
            .map(|(tick, _, _, target, data)| ChipWrite { tick, target, data })
            .collect(),
        reports,
    ))
}

pub fn model_scope() -> Value {
    json!({
        "renderer": "Tunemend WonderSwan sound unit model; not original hardware audio or driver playback",
        "sources": ["WSdev wiki: Sound (mixing diagram, wavetable RAM, ports $80-$9E, LFSR algorithm and sequence lengths)",
            "VGM 1.71 specification (WonderSwan clock 0xC0, commands 0xBC and 0xC6)"],
        "modelled": ["four channels with an 11-bit divisor: the sample index advances every 2048 - divisor cycles while the channel's enable bit in $90 is set",
            "wavetables: 16 bytes per channel at ($8F << 6) + 16 (channel - 1) in internal RAM, sample 2k in the low nibble of byte k and 2k + 1 in the high nibble",
            "volume: sample (0..15) times the left and right nibble of $88..$8B per side",
            "channel 2 voice mode ($90 bit 5, without needing bit 1): the unsigned 8-bit sample in $89 at 100% or 50% per side from $94",
            "channel 3 sweep ($90 bit 6 with bit 2): every ($8D + 1) ticks of the 375 Hz clock (clock / 8192) the signed $8C is added to the divisor, wrapping at 11 bits; a $8D write restarts the counter",
            "channel 4 noise ($90 bit 7 with bit 3): the LFSR advances once per channel 4 sample while $8E bit 4 is set; the new bit is the inverted xor of bit 7 and the tap bit of the mode (14, 10, 13, 4, 8, 6, 9, 11), shifted in at bit 0 and played as sample 0 or 15; $8E bit 3 resets the LFSR",
            "headphone output: the four channels summed per side to 10 bits and shifted left 5 (the mixing diagram's << 5), sampled every 128 cycles (24000 Hz at 3.072 MHz)"],
        "declared_choices": ["power-on: ports 0, channels off, sample indices 0, counters at 2048, LFSR 0, internal RAM 0",
            "one tick is one CPU cycle; the mix is sampled at the first cycle of each 128-cycle output sample, after that cycle's writes, and held",
            "a divisor change takes effect at the next reload; the sweep counter runs only while the sweep is active, from a 375 Hz prescaler running since power-on; an LFSR reset sets it to 0",
            "VGM 0xBC register aa is the port $80 + (aa & 0x7F) (bit 7 selects the second chip), as 0xB3 and 0xB4 count from their chips' first sound register; 0xC6 writes internal RAM of the first chip",
            "the headphone path is rendered whatever $91 holds; the output is unipolar with DC",
            "writes to read-only and undocumented ports change nothing"],
        "not_modelled": ["the internal speaker path (mono, shifted by $91 bits 1-2, 8-bit wraparound, $9E volume whose levels are not documented)", "Hyper Voice (WonderSwan Color ports $64-$6A)",
            "the sound test port $95 (output holds, forced wavetables, the CPU-clock sweep)", "analog output stages",
            "a second chip in dual-chip VGM files renders as an independent sound unit with its own RAM"],
        "equality": "Equal register writes and clock do not establish that a game, driver or hardware unit sounds the same."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> WonderSwan {
        WonderSwan::new(WsConfig::new(CLOCK_HZ))
    }

    fn ports(ws: &mut WonderSwan, writes: &[(u8, u8)]) {
        for &(p, d) in writes {
            ws.write_port(p, d);
        }
    }

    fn run(ws: &mut WonderSwan, cycles: u64) {
        for _ in 0..cycles {
            ws.step();
        }
    }

    #[test]
    fn samples_advance_every_2048_minus_divisor_cycles() {
        for divisor in [0u16, 1000, 2040, 2047] {
            let mut ws = unit();
            ports(
                &mut ws,
                &[
                    (0x00, (divisor & 0xff) as u8),
                    (0x01, (divisor >> 8) as u8),
                    (0x10, 0x01),
                ],
            );
            // The counter starts at 2048 after power-on, then reloads.
            run(&mut ws, 2048);
            assert_eq!(ws.channels[0].index, 1);
            let period = u64::from(2048 - divisor);
            for step in 2..40u8 {
                run(&mut ws, period - 1);
                assert_eq!(ws.channels[0].index, (step - 1) & 31);
                run(&mut ws, 1);
                assert_eq!(ws.channels[0].index, step & 31, "divisor {divisor}");
            }
        }
        // A disabled channel holds its index.
        let mut ws = unit();
        run(&mut ws, 5000);
        assert_eq!(ws.channels[0].index, 0);
        assert!((sample_rate_hz(CLOCK_HZ, 1048) - 3072.0).abs() < 1e-9);
    }

    #[test]
    fn wavetables_read_low_nibbles_first_from_the_wavetable_address() {
        let mut ws = unit();
        // The documented example wave's first bytes.
        ports(&mut ws, &[(0x0f, 0x02)]);
        for (i, b) in [0x98u8, 0xcb, 0xed, 0xff].iter().enumerate() {
            ws.write_memory(0x80 + 16 + i as u16, *b);
        }
        let first: Vec<u8> = (0..8).map(|i| ws.wave_sample(1, i)).collect();
        assert_eq!(first, [8, 9, 11, 12, 13, 14, 15, 15]);
        let effect = ws.write_memory(0x80 + 16, 0x98);
        assert_eq!(effect["wavetable"]["channel"], 2);
        assert_eq!(effect["wavetable"]["values"], json!([8, 9]));
        // Volume: sample times each side's nibble.
        ports(&mut ws, &[(0x09, 0xf3), (0x10, 0x02)]);
        assert_eq!(ws.channel_outputs()[1], Some([8 * 15, 8 * 3]));
        // The headphone path shifts the 10-bit sums left by 5.
        assert_eq!(ws.mix(), [(8 * 15) << 5, (8 * 3) << 5]);
    }

    #[test]
    fn voice_mode_plays_port_89_at_full_or_half_level() {
        let mut ws = unit();
        ports(&mut ws, &[(0x09, 0xc9), (0x10, 0x20), (0x14, 0x06)]);
        // Left 100% (bit 2), right 50% (bit 1); no wavetable enable needed.
        assert_eq!(ws.channel_outputs()[1], Some([0xc9, 0xc9 >> 1]));
        // The 100% bit wins over 50%.
        ports(&mut ws, &[(0x14, 0x0f)]);
        assert_eq!(ws.channel_outputs()[1], Some([0xc9, 0xc9]));
        ports(&mut ws, &[(0x14, 0x00)]);
        assert_eq!(ws.channel_outputs()[1], Some([0, 0]));
        // Four channels at their maximum stay within 10 bits and PCM16.
        let mut ws = unit();
        for c in 0..4u16 {
            for k in 0..16 {
                ws.write_memory(16 * c + k, 0xff);
            }
        }
        ports(
            &mut ws,
            &[
                (0x08, 0xff),
                (0x09, 0xff),
                (0x0a, 0xff),
                (0x0b, 0xff),
                (0x10, 0x2f),
                (0x14, 0x05),
            ],
        );
        assert_eq!(ws.mix(), [(3 * 225 + 255) << 5; 2]);
    }

    #[test]
    fn lfsr_tap_modes_have_the_documented_lengths() {
        for (mode, &(_, length)) in TAPS.iter().enumerate() {
            let mut seen = std::collections::HashMap::new();
            let mut state = 0u16;
            let mut step = 0u32;
            while !seen.contains_key(&state) {
                seen.insert(state, step);
                state = lfsr_step(state, mode as u8);
                step += 1;
            }
            assert_eq!(step - seen[&state], length, "mode {mode}");
        }
        // The first bits from the reset state 0 in mode 0: 1 until bit 7
        // reaches the shifted-in ones.
        let mut ws = unit();
        ports(
            &mut ws,
            &[(0x07, 0x07), (0x0e, 0x18), (0x10, 0x88), (0x0b, 0x11)],
        );
        // The power-on counter (2048) expires first, then every 256 cycles.
        run(&mut ws, 2048);
        let mut bits = vec![ws.lfsr & 1];
        for _ in 0..9 {
            run(&mut ws, 2048 - 0x700);
            bits.push(ws.lfsr & 1);
        }
        assert_eq!(bits, [1, 1, 1, 1, 1, 1, 1, 1, 0, 0]);
        assert_eq!(ws.channel_outputs()[3].map(|o| o[0] % 15), Some(0));
        // Without the LFSR enable bit the register holds.
        let mut ws = unit();
        ports(&mut ws, &[(0x0e, 0x08), (0x10, 0x88)]);
        run(&mut ws, 10_000);
        assert_eq!(ws.lfsr, 0);
    }

    #[test]
    fn sweep_adds_the_signed_amount_every_t_plus_1_ticks_of_375_hz() {
        let mut ws = unit();
        ports(
            &mut ws,
            &[
                (0x04, 0xfe),
                (0x05, 0x07),
                (0x0c, 0x02),
                (0x0d, 0x01),
                (0x10, 0x44),
            ],
        );
        // Ticks of the 375 Hz clock at 8192, 16384 ...: a step every 2 ticks.
        run(&mut ws, 8192);
        assert_eq!(ws.divisor(2), 0x7fe);
        run(&mut ws, 8192);
        // 0x7FE + 2 wraps to 0.
        assert_eq!(ws.divisor(2), 0x000);
        ports(&mut ws, &[(0x0c, 0xfd)]);
        run(&mut ws, 2 * 8192);
        assert_eq!(ws.divisor(2), 0x7fd);
        // Without channel 3's enable bit the sweep does nothing.
        ports(&mut ws, &[(0x10, 0x40)]);
        run(&mut ws, 4 * 8192);
        assert_eq!(ws.divisor(2), 0x7fd);
        assert!((375.0 - f64::from(CLOCK_HZ) / f64::from(SWEEP_PRESCALER)).abs() < 1e-9);
    }

    #[test]
    fn output_is_sampled_every_128_cycles() {
        let mut ws = unit();
        for k in 0..16 {
            ws.write_memory(k, 0xf0);
        }
        ports(
            &mut ws,
            &[(0x08, 0xff), (0x00, 0xff), (0x01, 0x07), (0x10, 0x01)],
        );
        // Sample 0 is 0; index 1 (15) arrives at cycle 2048.
        let mut frames = Vec::new();
        for _ in 0..32 {
            let mut sum = [0i64; 2];
            for _ in 0..FRAME_TICKS {
                let out = ws.tick();
                sum[0] += i64::from(out[0]);
            }
            frames.push(sum[0] / i64::from(FRAME_TICKS));
        }
        assert!(frames[..16].iter().all(|f| *f == 0));
        assert_eq!(frames[16], i64::from(225 << 5));
    }

    #[test]
    fn note_writes_load_wavetables_and_share_the_control_port() {
        let wave: Vec<u8> = (0..32).map(|i| (i / 2) as u8).collect();
        let note = |channel: u8, start: f64| PsgNote {
            channel,
            start_seconds: start,
            duration_seconds: 0.1,
            pitch: Some(PsgPitch::MidiNote {
                note: 69,
                a4_hz: 440.0,
            }),
            noise: None,
            attenuation: None,
            volume: Some(12),
            duty: None,
            envelope: None,
            wave: None,
            dpcm: None,
            wavetable: Some(wave.clone()),
        };
        let mut noise = note(3, 0.05);
        noise.pitch = Some(PsgPitch::Period { period: 1800 });
        noise.wavetable = None;
        noise.noise = Some(crate::psg::PsgNoise {
            taps: Some(2),
            ..Default::default()
        });
        let (writes, reports) =
            note_writes(CLOCK_HZ, &[note(0, 0.0), note(1, 0.02), noise]).unwrap();
        assert_eq!(reports[0]["period"], 1830);
        assert_eq!(reports[2]["noise"]["sequence_length"], 254);
        let controls: Vec<u8> = writes
            .iter()
            .filter(|w| w.target == Target::Register(0x10))
            .map(|w| w.data)
            .collect();
        assert_eq!(controls, [0x01, 0x03, 0x8b, 0x8a, 0x88, 0x00]);
        let mut ws = unit();
        for w in writes.iter().filter(|w| w.tick == 0) {
            ws.apply(w.target, w.data);
        }
        assert_eq!(
            (0..32).map(|i| ws.wave_sample(0, i)).collect::<Vec<_>>(),
            wave
        );
        assert_eq!(ws.ports[0x08], 0xcc);
        let mut bad = note(0, 0.0);
        bad.wavetable = Some(vec![16; 32]);
        assert!(note_writes(CLOCK_HZ, &[bad]).is_err());
        let mut bad = note(2, 0.0);
        bad.noise = Some(crate::psg::PsgNoise {
            taps: Some(1),
            ..Default::default()
        });
        assert!(note_writes(CLOCK_HZ, &[bad]).is_err());
    }
}
