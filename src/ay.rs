//! General Instrument AY-3-8910 and Yamaha YM2149 PSG model: three square
//! tone channels, one noise generator, a mixer, 4-bit amplitudes through a
//! logarithmic DAC and one envelope generator.
//!
//! References: the
//! "General Instrument AY-3-8910" article (register map, 12-bit tone periods,
//! noise clock/16/NP, inverted mixer bits, envelope bits, I/O ports, the
//! AY-3-8912/8913 packages and the YM2149's doubled envelope resolution and
//! pin-26 clock halving), the NESdev wiki "Sunsoft 5B audio" page (a YM2149F
//! core: period 0 acts as 1, the 17-bit noise LFSR with taps at bits 16 and
//! 13, 1.5 dB DAC steps of the 5-bit envelope where two steps equal one 4-bit
//! volume step, the CONT/ATT/ALT/HOLD shape table and the up-counting phase
//! counters) and the VGM 1.71 specification (header clock, chip type, flags and
//! the 0xA0 and 0x31 commands). Behaviour the documents leave open is a
//! declared choice in [`model_scope`].
use crate::psg::{PsgNote, PsgPitch};
use crate::sound_chip::{Chip, ChipWrite, Target};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MIN_CLOCK_HZ: u32 = 500_000;
pub const MAX_CLOCK_HZ: u32 = 6_000_000;
/// The MSX PSG clock (half the 3.579545 MHz system clock).
pub const MSX_CLOCK_HZ: u32 = 1_789_773;
/// Input clock cycles per model tick: tone counters toggle every period ticks,
/// so one full cycle is 16 * period clocks.
pub const TICK_DIVIDER: u32 = 8;

/// AY-3-8910 4-bit DAC: 3 dB steps (ratio 2^-0.5) from 10922, 0 silent, so
/// three channels at full level stay within 16 bits.
pub const AY_LEVELS: [i32; 16] = [
    0, 85, 121, 171, 241, 341, 483, 683, 965, 1365, 1931, 2731, 3862, 5461, 7723, 10922,
];
/// YM2149 5-bit DAC: 1.5 dB steps (ratio 2^-0.25) from 10922; levels 0 and 1
/// are silent. A 4-bit amplitude L uses level 2L+1.
pub const YM_LEVELS: [i32; 32] = [
    0, 0, 72, 85, 101, 121, 144, 171, 203, 241, 287, 341, 406, 483, 574, 683, 812, 965, 1148, 1365,
    1624, 1931, 2296, 2731, 3247, 3862, 4592, 5461, 6494, 7723, 9184, 10922,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AyVariant {
    /// AY-3-8910 (also the AY-3-8912 and AY-3-8913 packages): 16-step
    /// envelope, 4-bit DAC in 3 dB steps.
    Ay8910,
    /// YM2149 (also YM3439): 32-step envelope, 5-bit DAC in 1.5 dB steps.
    Ym2149,
}

impl AyVariant {
    pub fn label(self) -> &'static str {
        match self {
            AyVariant::Ay8910 => "AY-3-8910 PSG",
            AyVariant::Ym2149 => "YM2149 PSG",
        }
    }
    pub fn envelope_steps(self) -> u8 {
        match self {
            AyVariant::Ay8910 => 16,
            AyVariant::Ym2149 => 32,
        }
    }
}

/// Channel routing to the two output sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pan {
    Off,
    Left,
    Right,
    Center,
}

impl Pan {
    fn from_bits(bits: u8) -> Self {
        [Pan::Off, Pan::Left, Pan::Right, Pan::Center][usize::from(bits & 3)]
    }
    fn sides(self) -> (bool, bool) {
        match self {
            Pan::Off => (false, false),
            Pan::Left => (true, false),
            Pan::Right => (false, true),
            Pan::Center => (true, true),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AyConfig {
    pub variant: AyVariant,
    /// Input clock cycles per tick: 8, or 16 when the YM2149 pin 26 halves the clock.
    pub clock_divider: u32,
    pub pan: [Pan; 3],
}

impl AyConfig {
    pub fn new(variant: AyVariant) -> Self {
        Self {
            variant,
            clock_divider: TICK_DIVIDER,
            pan: [Pan::Center; 3],
        }
    }
    pub fn to_json(&self) -> Value {
        json!({"variant":self.variant,"envelope_steps":self.variant.envelope_steps(),
            "dac":match self.variant {AyVariant::Ay8910 => "4-bit, 3 dB steps, 0 silent", AyVariant::Ym2149 => "5-bit, 1.5 dB steps, 0 and 1 silent; amplitude L uses 2L+1"},
            "tick":format!("clock/{}", self.clock_divider),"clock_halved_by_pin_26":self.clock_divider == 2 * TICK_DIVIDER,
            "pan":{"a":self.pan[0],"b":self.pan[1],"c":self.pan[2]},"output":"unipolar: level when the channel gate is high, 0 when low"})
    }
}

pub fn check_clock(clock_hz: u32) -> Result<(), String> {
    if (MIN_CLOCK_HZ..=MAX_CLOCK_HZ).contains(&clock_hz) {
        Ok(())
    } else {
        Err(format!(
            "clock_hz must be {MIN_CLOCK_HZ}..{MAX_CLOCK_HZ}, such as {MSX_CLOCK_HZ} (MSX), 1773400 (ZX Spectrum 128) or 2000000 (Atari ST)"
        ))
    }
}

const REGISTER_NAMES: [&str; 16] = [
    "tone_a_fine",
    "tone_a_coarse",
    "tone_b_fine",
    "tone_b_coarse",
    "tone_c_fine",
    "tone_c_coarse",
    "noise_period",
    "mixer",
    "amplitude_a",
    "amplitude_b",
    "amplitude_c",
    "envelope_fine",
    "envelope_coarse",
    "envelope_shape",
    "io_port_a",
    "io_port_b",
];
const REGISTER_MASKS: [u8; 16] = [
    0xff, 0x0f, 0xff, 0x0f, 0xff, 0x0f, 0x1f, 0xff, 0x1f, 0x1f, 0x1f, 0xff, 0xff, 0x0f, 0xff, 0xff,
];
const CHANNELS: [&str; 3] = ["a", "b", "c"];

pub fn shape_name(shape: u8) -> &'static str {
    match shape & 15 {
        0..=3 => "one fall, then silent",
        4..=7 => "one rise, then silent",
        8 => "repeating fall (sawtooth down)",
        9 => "one fall, then silent (held low)",
        10 => "triangle starting with a fall",
        11 => "one fall, then held high",
        12 => "repeating rise (sawtooth up)",
        13 => "one rise, then held high",
        14 => "triangle starting with a rise",
        _ => "one rise, then held low",
    }
}

pub fn register_name(register: u8) -> &'static str {
    REGISTER_NAMES
        .get(usize::from(register))
        .copied()
        .unwrap_or("not addressed")
}

/// Register and counter state of one AY-3-8910 or YM2149.
#[derive(Debug, Clone)]
pub struct Ay {
    config: AyConfig,
    regs: [u8; 16],
    tone_counter: [u16; 3],
    tone_high: [bool; 3],
    /// Phase of the clock/16 events (noise, and the AY envelope).
    half: bool,
    noise_counter: u8,
    lfsr: u32,
    noise_high: bool,
    envelope_counter: u32,
    envelope_step: u8,
    envelope_attack: bool,
    envelope_holding: bool,
    envelope_level: u8,
    /// Channels left out of the output mix (bit c = channel c); chip state is unaffected.
    output_mute: u8,
}

impl Ay {
    /// Declared power-on state: every register 0, counters 0, tone outputs
    /// low, LFSR 1, the envelope idle at level 0 until register 13 is written.
    pub fn new(config: AyConfig) -> Self {
        Self {
            config,
            regs: [0; 16],
            tone_counter: [0; 3],
            tone_high: [false; 3],
            half: false,
            noise_counter: 0,
            lfsr: 1,
            noise_high: false,
            envelope_counter: 0,
            envelope_step: 0,
            envelope_attack: false,
            envelope_holding: true,
            envelope_level: 0,
            output_mute: 0,
        }
    }
    fn max_level(&self) -> u8 {
        self.config.variant.envelope_steps() - 1
    }
    pub fn tone_period(&self, channel: usize) -> u16 {
        u16::from(self.regs[2 * channel]) | (u16::from(self.regs[2 * channel + 1]) << 8)
    }
    fn envelope_period(&self) -> u32 {
        u32::from(self.regs[11]) | (u32::from(self.regs[12]) << 8)
    }
    fn restart_envelope(&mut self) {
        self.envelope_counter = 0;
        self.envelope_step = 0;
        self.envelope_attack = self.regs[13] & 4 != 0;
        self.envelope_holding = false;
        self.envelope_level = if self.envelope_attack {
            0
        } else {
            self.max_level()
        };
    }
    /// One envelope step: ramps of `envelope_steps` levels, then the shape bits.
    fn step_envelope(&mut self) {
        if self.envelope_holding {
            return;
        }
        let (max, steps) = (self.max_level(), self.config.variant.envelope_steps());
        self.envelope_step += 1;
        if self.envelope_step < steps {
            self.envelope_level = if self.envelope_attack {
                self.envelope_step
            } else {
                max - self.envelope_step
            };
            return;
        }
        self.envelope_step = 0;
        let shape = self.regs[13];
        let (cont, alternate, hold) = (shape & 8 != 0, shape & 2 != 0, shape & 1 != 0);
        if !cont {
            self.envelope_holding = true;
            self.envelope_level = 0;
        } else if hold {
            self.envelope_holding = true;
            let high = self.envelope_attack != alternate;
            self.envelope_level = if high { max } else { 0 };
        } else {
            if alternate {
                self.envelope_attack = !self.envelope_attack;
            }
            self.envelope_level = if self.envelope_attack { 0 } else { max };
        }
    }
    fn shift_noise(&mut self) {
        let bit = (self.lfsr ^ (self.lfsr >> 3)) & 1;
        self.lfsr = (self.lfsr >> 1) | (bit << 16);
        self.noise_high = self.lfsr & 1 != 0;
    }
    /// Amplitude of a channel through the DAC.
    fn amplitude(&self, channel: usize) -> i32 {
        let register = self.regs[8 + channel];
        match (self.config.variant, register & 0x10 != 0) {
            (AyVariant::Ay8910, true) => AY_LEVELS[usize::from(self.envelope_level)],
            (AyVariant::Ay8910, false) => AY_LEVELS[usize::from(register & 15)],
            (AyVariant::Ym2149, true) => YM_LEVELS[usize::from(self.envelope_level)],
            (AyVariant::Ym2149, false) => YM_LEVELS[usize::from(register & 15) * 2 + 1],
        }
    }
    /// One register write; registers above 15 are not addressed and ignored.
    pub fn write_register(&mut self, register: u8, data: u8) -> Value {
        if register > 15 {
            return json!({"register":register,"name":"not addressed","data":data,
                "ignored":"the chip decodes registers 0..15 only"});
        }
        let index = usize::from(register);
        let before = self.regs[index];
        let after = data & REGISTER_MASKS[index];
        self.regs[index] = after;
        let mut effect =
            json!({"register":register,"name":REGISTER_NAMES[index],"before":before,"after":after});
        if data != after {
            effect["unused_bits_dropped"] = json!(data & !REGISTER_MASKS[index]);
        }
        match register {
            0..=5 => {
                let channel = index / 2;
                effect["channel"] = json!(CHANNELS[channel]);
                effect["tone_period"] = json!(self.tone_period(channel));
            }
            6 => effect["noise_period"] = json!(after),
            7 => {
                effect["tone_enabled"] =
                    json!((0..3).map(|c| after & (1 << c) == 0).collect::<Vec<_>>());
                effect["noise_enabled"] =
                    json!((0..3).map(|c| after & (8 << c) == 0).collect::<Vec<_>>());
                effect["io_a_output"] = json!(after & 0x40 != 0);
                effect["io_b_output"] = json!(after & 0x80 != 0);
            }
            8..=10 => {
                effect["channel"] = json!(CHANNELS[index - 8]);
                effect["envelope_mode"] = json!(after & 0x10 != 0);
                effect["fixed_level"] = json!(after & 15);
            }
            11 | 12 => effect["envelope_period"] = json!(self.envelope_period()),
            13 => {
                self.restart_envelope();
                effect["shape"] = json!(shape_name(after));
                effect["continue"] = json!(after & 8 != 0);
                effect["attack"] = json!(after & 4 != 0);
                effect["alternate"] = json!(after & 2 != 0);
                effect["hold"] = json!(after & 1 != 0);
                effect["envelope_restart"] = json!(true);
            }
            _ => effect["audible"] = json!(false),
        }
        effect
    }
    /// The VGM stereo mask: two bits per channel, 00 off, 01 left, 10 right, 11 center.
    pub fn write_stereo_mask(&mut self, mask: u8) -> Value {
        let before = self.config.pan;
        for (channel, pan) in self.config.pan.iter_mut().enumerate() {
            *pan = Pan::from_bits(mask >> (2 * channel));
        }
        json!({"register":"stereo_mask","data":mask,"before":{"a":before[0],"b":before[1],"c":before[2]},
            "after":{"a":self.config.pan[0],"b":self.config.pan[1],"c":self.config.pan[2]}})
    }
    #[cfg(test)]
    pub(crate) fn envelope_level(&self) -> u8 {
        self.envelope_level
    }
    #[cfg(test)]
    pub(crate) fn noise_high(&self) -> bool {
        self.noise_high
    }
}

impl Chip for Ay {
    const TICKS_PER_FRAME: u32 = 1;
    const OUTPUT_DIVISOR: i64 = 1;
    fn mute_output(&mut self, mask: u8) {
        self.output_mute = mask;
    }
    fn apply(&mut self, target: Target, data: u8) -> Value {
        match target {
            Target::StereoMask => self.write_stereo_mask(data),
            Target::Register(register) => self.write_register(register, data),
            _ => json!({"ignored":"not an AY port","data":data}),
        }
    }
    /// One tick (clock/8): tone counters count up and toggle on reaching the
    /// period; every second tick clocks the noise counter (and, on the
    /// AY-3-8910, the envelope counter); the YM2149 envelope counts every tick.
    fn tick(&mut self) -> [i32; 2] {
        for channel in 0..3 {
            let period = self.tone_period(channel).max(1);
            self.tone_counter[channel] += 1;
            if self.tone_counter[channel] >= period {
                self.tone_counter[channel] = 0;
                self.tone_high[channel] = !self.tone_high[channel];
            }
        }
        self.half = !self.half;
        if self.half {
            let period = (self.regs[6] & 31).max(1);
            self.noise_counter += 1;
            if self.noise_counter >= period {
                self.noise_counter = 0;
                self.shift_noise();
            }
        }
        if self.half || self.config.variant == AyVariant::Ym2149 {
            let period = self.envelope_period().max(1);
            self.envelope_counter += 1;
            if self.envelope_counter >= period {
                self.envelope_counter = 0;
                self.step_envelope();
            }
        }
        let mixer = self.regs[7];
        let mut out = [0i32; 2];
        for channel in 0..3 {
            let tone = self.tone_high[channel] || mixer & (1 << channel) != 0;
            let noise = self.noise_high || mixer & (8 << channel) != 0;
            if !(tone && noise) || self.output_mute & (1 << channel) != 0 {
                continue;
            }
            let level = self.amplitude(channel);
            let (left, right) = self.config.pan[channel].sides();
            if left {
                out[0] += level;
            }
            if right {
                out[1] += level;
            }
        }
        out
    }
    fn state(&self) -> Value {
        json!({"registers":self.regs.iter().map(|r| format!("0x{r:02x}")).collect::<Vec<_>>(),
            "tone_periods":(0..3).map(|c| self.tone_period(c)).collect::<Vec<_>>(),"tone_counters":self.tone_counter,
            "noise_period":self.regs[6],"lfsr":format!("0x{:05x}",self.lfsr),"envelope":{"period":self.envelope_period(),
            "shape":self.regs[13],"level":self.envelope_level,"holding":self.envelope_holding,"rising":self.envelope_attack},
            "io_ports":{"a":self.regs[14],"b":self.regs[15]},"pan":{"a":self.config.pan[0],"b":self.config.pan[1],"c":self.config.pan[2]}})
    }
}

pub fn tone_hz(clock_hz: u32, divider: u32, period: u16) -> f64 {
    f64::from(clock_hz) / (2.0 * f64::from(divider) * f64::from(period))
}

fn tick_at(clock_hz: u32, divider: u32, seconds: f64) -> Result<u64, String> {
    let max = crate::psg::MAX_RENDER_SECONDS as f64;
    if !(seconds.is_finite() && (0.0..=max).contains(&seconds)) {
        return Err(format!("times must be 0..{max} seconds"));
    }
    Ok((seconds * f64::from(clock_hz) / f64::from(divider)).round() as u64)
}

/// Register writes for AY notes. A tone note sets the channel's period, a
/// noise note the shared noise period; both open the channel's mixer bits and
/// set its fixed amplitude, and the end closes them and sets amplitude 0.
pub fn note_writes(
    clock_hz: u32,
    divider: u32,
    notes: &[PsgNote],
) -> Result<(Vec<ChipWrite>, Vec<Value>), (usize, String)> {
    struct Span {
        channel: usize,
        start: u64,
        end: u64,
        period: Option<u16>,
        noise: Option<u8>,
        volume: u8,
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut reports = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        let fail = |message: String| (index, message);
        if note.channel > 2 {
            return Err(fail("AY channels are 0..2 (A, B, C)".into()));
        }
        if note.attenuation.is_some()
            || note.duty.is_some()
            || note.envelope.is_some()
            || note.wave.is_some()
        {
            return Err(fail(
                "AY notes take pitch and/or noise.period with volume; attenuation, duty, envelope and wave belong to other chips".into(),
            ));
        }
        let volume = note
            .volume
            .filter(|v| (1..=15).contains(v))
            .ok_or_else(|| fail("volume must be 1..15 (fixed amplitude)".into()))?;
        if !(note.duration_seconds.is_finite() && note.duration_seconds > 0.0) {
            return Err(fail("duration_seconds must be positive".into()));
        }
        let start = tick_at(clock_hz, divider, note.start_seconds).map_err(fail)?;
        let end = tick_at(
            clock_hz,
            divider,
            note.start_seconds + note.duration_seconds,
        )
        .map_err(fail)?;
        if end <= start {
            return Err(fail("the note is shorter than one native sample".into()));
        }
        let channel = usize::from(note.channel);
        if spans
            .iter()
            .any(|s| s.channel == channel && start < s.end && s.start < end)
        {
            return Err(fail(format!("overlaps another note on channel {channel}")));
        }
        let mut report = json!({"index":index,"channel":CHANNELS[channel],"start_tick":start,"end_tick":end,
            "volume":volume,"level_db":20.0 * (f64::from(AY_LEVELS[usize::from(volume)]) / f64::from(AY_LEVELS[15])).log10()});
        let period = match &note.pitch {
            None => None,
            Some(pitch) => {
                let (period, pitch_report) = match *pitch {
                    PsgPitch::Period { period } => (
                        f64::from(period),
                        json!({"source":"period","period":period}),
                    ),
                    PsgPitch::MidiNote { note, a4_hz } => {
                        if note > 127 || !(a4_hz.is_finite() && (220.0..=880.0).contains(&a4_hz)) {
                            return Err(fail("midi note must be 0..127 and a4_hz 220..880".into()));
                        }
                        let target = a4_hz * ((f64::from(note) - 69.0) / 12.0).exp2();
                        let period =
                            (f64::from(clock_hz) / (2.0 * f64::from(divider) * target)).round();
                        let cents = if (1.0..=4095.0).contains(&period) {
                            1200.0 * (tone_hz(clock_hz, divider, period as u16) / target).log2()
                        } else {
                            0.0
                        };
                        (
                            period,
                            json!({"source":"midi_note","note":note,"a4_hz":a4_hz,"target_hz":target,"cents_error":cents}),
                        )
                    }
                };
                if !(1.0..=4095.0).contains(&period) {
                    return Err(fail(format!(
                        "tone period {period} is outside 1..4095 at this clock"
                    )));
                }
                let period = period as u16;
                report["period"] = json!(period);
                report["frequency_hz"] = json!(tone_hz(clock_hz, divider, period));
                report["pitch"] = pitch_report;
                Some(period)
            }
        };
        let noise = match &note.noise {
            None => None,
            Some(noise) => {
                if noise.mode.is_some()
                    || noise.rate.is_some()
                    || noise.divisor.is_some()
                    || noise.shift.is_some()
                {
                    return Err(fail("AY noise takes only period (1..31)".into()));
                }
                let p = noise
                    .period
                    .filter(|p| (1..=31).contains(p))
                    .ok_or_else(|| fail("noise.period must be 1..31".into()))?;
                report["noise_period"] = json!(p);
                report["noise_shift_hz"] =
                    json!(f64::from(clock_hz) / (2.0 * f64::from(divider) * f64::from(p)));
                Some(p)
            }
        };
        if period.is_none() && noise.is_none() {
            return Err(fail("an AY note needs pitch, noise or both".into()));
        }
        if let Some(p) = noise
            && spans
                .iter()
                .any(|s| s.noise.is_some_and(|q| q != p) && start < s.end && s.start < end)
        {
            return Err(fail("the noise period is shared by all channels; overlapping noise notes must use the same period".into()));
        }
        spans.push(Span {
            channel,
            start,
            end,
            period,
            noise,
            volume,
        });
        reports.push(report);
    }
    // Ends before starts at one tick, then note order.
    let mut events: Vec<(u64, bool, usize)> = Vec::new();
    for (i, s) in spans.iter().enumerate() {
        events.push((s.start, true, i));
        events.push((s.end, false, i));
    }
    events.sort_by_key(|&(tick, starts, i)| (tick, starts, i));
    let mut mixer = 0x3fu8;
    let mut writes = Vec::new();
    let push = |writes: &mut Vec<ChipWrite>, tick: u64, register: u8, data: u8| {
        writes.push(ChipWrite {
            tick,
            target: Target::Register(register),
            data,
        });
        [register, data]
    };
    type Logged = Vec<[u8; 2]>;
    let mut note_writes: Vec<(Logged, Logged)> = vec![(Vec::new(), Vec::new()); spans.len()];
    for (tick, starts, i) in events {
        let s = &spans[i];
        let c = s.channel as u8;
        let mut log = Vec::new();
        if starts {
            if let Some(period) = s.period {
                log.push(push(&mut writes, tick, 2 * c, (period & 0xff) as u8));
                log.push(push(&mut writes, tick, 2 * c + 1, (period >> 8) as u8));
                mixer &= !(1 << c);
            } else {
                mixer |= 1 << c;
            }
            if let Some(p) = s.noise {
                log.push(push(&mut writes, tick, 6, p));
                mixer &= !(8 << c);
            } else {
                mixer |= 8 << c;
            }
            log.push(push(&mut writes, tick, 7, mixer));
            log.push(push(&mut writes, tick, 8 + c, s.volume));
            note_writes[i].0 = log;
        } else {
            mixer |= 9 << c;
            log.push(push(&mut writes, tick, 8 + c, 0));
            log.push(push(&mut writes, tick, 7, mixer));
            note_writes[i].1 = log;
        }
    }
    for (report, (start, end)) in reports.iter_mut().zip(note_writes) {
        let show = |w: Vec<[u8; 2]>| {
            w.iter()
                .map(|[r, d]| json!({"register":r,"name":REGISTER_NAMES[usize::from(*r)],"data":d}))
                .collect::<Vec<_>>()
        };
        report["writes"] = json!(show(start));
        report["release_writes"] = json!(show(end));
    }
    Ok((writes, reports))
}

pub fn model_scope() -> Value {
    json!({
        "renderer": "Tunemend AY-3-8910/YM2149 PSG model; not original hardware audio or driver playback",
        "sources": ["General Instrument AY-3-8910 (Wikipedia article: registers, periods, mixer, envelope bits, I/O ports, packages, YM2149 differences)",
            "NESdev wiki: Sunsoft 5B audio (YM2149F core: period 0 as 1, 17-bit noise LFSR taps 16 and 13, 1.5 dB DAC steps, envelope shapes, up-counting phase counters)",
            "VGM 1.71 specification (AY8910 clock, chip type, flags, commands 0xA0 and 0x31)"],
        "modelled": ["three 12-bit tone counters counting up at clock/8 and toggling on reaching the period: frequency clock/(16*TP); period 0 acts as 1",
            "5-bit noise period: the 17-bit LFSR (feedback bit 0 xor bit 3 into bit 16, the taps at bits 16 and 13) shifts every 16*NP clocks; period 0 acts as 1",
            "mixer register 7: inverted tone and noise enables per channel; the channel gate is (tone or tone disabled) and (noise or noise disabled), so both disabled gives a constant level",
            "amplitude registers 8..10: 4-bit fixed level or the envelope (bit 4)",
            "AY-3-8910 DAC: 16 levels in 3 dB steps, 0 silent; YM2149 DAC: 32 levels in 1.5 dB steps, 0 and 1 silent, fixed amplitude L at level 2L+1",
            "envelope: 16-bit period; 16 steps of 16*EP clocks (AY-3-8910) or 32 steps of 8*EP clocks (YM2149), one ramp lasting 256*EP clocks on both; CONT/ATT/ALT/HOLD shapes 0..15; a write to register 13 restarts it",
            "register masks (coarse tone 4 bits, noise and amplitude 5 bits, shape 4 bits); registers above 15 are ignored",
            "I/O ports 14 and 15 and the mixer's port-direction bits are kept and reported but not audible",
            "VGM: AY8910 clock, chip type (AY8910/8912/8913, YM2149/YM3439/YMZ284/YMZ294), flag bit 4 (YM2149 pin 26 clock halving), flag bit 7 (A left, B center, C right) and the 0x31 stereo mask"],
        "declared_choices": ["power-on: every register 0, counters 0, tone outputs low, noise LFSR 1, envelope idle at level 0 until register 13 is written",
            "the noise output is LFSR bit 0 after each shift",
            "clock/16 events (noise, AY-3-8910 envelope) fall on every second tick, starting with the first",
            "outputs are unipolar like the chip's DAC (level when the gate is high, 0 when low); the DC component is kept",
            "default routing: every channel center (both sides), unless the VGM flags or a stereo mask say otherwise",
            "YMZ284 and YMZ294 are rendered with the YM2149 sound core",
            "writes take effect before the tick of their native sample, in order"],
        "not_modelled": ["analog output stage, channel mixing resistors and filtering", "measured DAC curves that deviate from the documented steps",
            "the AY-3-8930 expanded mode and the AY-3-8914 register map (both refuse)", "I/O port input and anything driven by ports",
            "VGM output-mode flag bits 0..3 (legacy, single, discrete, raw)", "the SSG parts of YM2203/YM2608/YM2610 (their commands are other-chip writes)",
            "write timing inside one clock/8 tick"],
        "equality": "Equal register writes and clock do not establish that a game, driver or hardware unit sounds the same."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(variant: AyVariant) -> Ay {
        Ay::new(AyConfig::new(variant))
    }

    #[test]
    fn dac_tables_follow_the_documented_steps() {
        for n in 2..16 {
            let ratio = f64::from(AY_LEVELS[n - 1]) / f64::from(AY_LEVELS[n]);
            assert!(
                (20.0 * ratio.log10() + 3.0103).abs() < 0.1,
                "AY step {n}: {ratio}"
            );
        }
        for l in 3..32 {
            let ratio = f64::from(YM_LEVELS[l - 1]) / f64::from(YM_LEVELS[l]);
            assert!(
                (20.0 * ratio.log10() + 1.5051).abs() < 0.15,
                "YM step {l}: {ratio}"
            );
        }
        // A 4-bit amplitude on the YM2149 equals the AY level (two 1.5 dB steps per 3 dB step).
        for l in 1..16 {
            assert_eq!(YM_LEVELS[2 * l + 1], AY_LEVELS[l]);
        }
        assert_eq!((AY_LEVELS[0], YM_LEVELS[0], YM_LEVELS[1]), (0, 0, 0));
        assert!(3 * AY_LEVELS[15] <= 32767);
    }

    #[test]
    fn tone_frequency_is_clock_over_16_tp() {
        for (period, clock) in [
            (0x1fdu16, MSX_CLOCK_HZ),
            (0x0fe, 2_000_000),
            (0xfff, 1_773_400),
        ] {
            let mut ay = chip(AyVariant::Ay8910);
            ay.write_register(0, (period & 0xff) as u8);
            ay.write_register(1, (period >> 8) as u8);
            ay.write_register(7, 0x3e);
            ay.write_register(8, 15);
            // Count rising edges over a whole number of cycles.
            let ticks = 2 * u64::from(period) * 40;
            let (mut last, mut rises) = (0, 0);
            for _ in 0..ticks {
                let out = ay.tick()[0];
                if out > 0 && last == 0 {
                    rises += 1;
                }
                last = out;
            }
            assert_eq!(rises, 40, "period {period}");
            let hz = tone_hz(clock, TICK_DIVIDER, period);
            assert!((hz - f64::from(clock) / (16.0 * f64::from(period))).abs() < 1e-9);
        }
        // Period 0 behaves like period 1: a toggle every tick.
        let mut ay = chip(AyVariant::Ay8910);
        ay.write_register(7, 0x3e);
        ay.write_register(8, 15);
        let outs: Vec<i32> = (0..6).map(|_| ay.tick()[0]).collect();
        assert_eq!(outs, [AY_LEVELS[15], 0, AY_LEVELS[15], 0, AY_LEVELS[15], 0]);
    }

    #[test]
    fn noise_shifts_every_16_np_clocks_with_a_17_bit_lfsr() {
        let mut ay = chip(AyVariant::Ay8910);
        ay.write_register(6, 5);
        // The 17-bit LFSR from 1: hand trace of bit 0 after each shift.
        let mut reference = 1u32;
        let mut shifts = 0;
        let mut last_lfsr = ay.lfsr;
        for tick in 1..=2 * 5 * 40u32 {
            ay.tick();
            if ay.lfsr != last_lfsr {
                shifts += 1;
                // Shifts happen every 2*NP ticks (16*NP input clocks).
                assert_eq!(tick % 10, 9, "shift at tick {tick}");
                let bit = (reference ^ (reference >> 3)) & 1;
                reference = (reference >> 1) | (bit << 16);
                assert_eq!(ay.lfsr, reference);
                assert_eq!(ay.noise_high(), reference & 1 != 0);
                last_lfsr = ay.lfsr;
            }
        }
        assert_eq!(shifts, 40);
        // The polynomial x^17 + x^14 + 1 is maximal: period 2^17 - 1.
        let (mut l, mut n) = (1u32, 0u32);
        loop {
            let bit = (l ^ (l >> 3)) & 1;
            l = (l >> 1) | (bit << 16);
            n += 1;
            if l == 1 {
                break;
            }
        }
        assert_eq!(n, 131_071);
    }

    /// The documented shape table as ramps: first ramp direction, then what follows.
    fn expected_levels(shape: u8, steps: u8, cycles: usize) -> Vec<u8> {
        let max = steps - 1;
        let up: Vec<u8> = (0..steps).collect();
        let down: Vec<u8> = (0..steps).rev().collect();
        let attack = shape & 4 != 0;
        let mut out = Vec::new();
        let first = if attack { &up } else { &down };
        out.extend(first);
        for cycle in 1..cycles {
            let hold = |level: u8| vec![level; usize::from(steps)];
            let next = match shape {
                0..=7 | 9 | 15 => hold(0),
                11 | 13 => hold(max),
                8 => down.clone(),
                12 => up.clone(),
                10 => {
                    if cycle % 2 == 1 {
                        up.clone()
                    } else {
                        down.clone()
                    }
                }
                _ => {
                    if cycle % 2 == 1 {
                        down.clone()
                    } else {
                        up.clone()
                    }
                }
            };
            out.extend(next);
        }
        out
    }

    #[test]
    fn every_envelope_shape_and_period_matches_the_shape_table() {
        for variant in [AyVariant::Ay8910, AyVariant::Ym2149] {
            let steps = variant.envelope_steps();
            for shape in 0..16u8 {
                for period in [1u16, 3] {
                    let mut ay = chip(variant);
                    ay.write_register(11, (period & 0xff) as u8);
                    ay.write_register(12, (period >> 8) as u8);
                    ay.write_register(13, shape);
                    // Ticks per step: 2*EP on the AY-3-8910, EP on the YM2149.
                    let per_step = match variant {
                        AyVariant::Ay8910 => 2 * u32::from(period),
                        AyVariant::Ym2149 => u32::from(period),
                    };
                    // Both variants complete one ramp in 32*EP ticks (256*EP clocks).
                    assert_eq!(per_step * u32::from(steps), 32 * u32::from(period));
                    // The first step falls on the EP-th envelope clock.
                    let first = match variant {
                        AyVariant::Ay8910 => 2 * u32::from(period) - 1,
                        AyVariant::Ym2149 => u32::from(period),
                    };
                    let samples = usize::from(steps) * 4;
                    let mut levels = vec![ay.envelope_level()];
                    let mut changes = Vec::new();
                    let mut tick = 0;
                    while levels.len() < samples {
                        tick += 1;
                        let before = ay.envelope_level();
                        ay.tick();
                        let on_step = tick >= first && (tick - first) % per_step == 0;
                        if on_step {
                            levels.push(ay.envelope_level());
                        } else if ay.envelope_level() != before {
                            changes.push(tick);
                        }
                    }
                    assert!(
                        changes.is_empty(),
                        "{variant:?} shape {shape}: level changed between steps at {changes:?}"
                    );
                    assert_eq!(
                        levels,
                        expected_levels(shape, steps, 4),
                        "{variant:?} shape {shape} period {period}"
                    );
                }
            }
        }
    }

    #[test]
    fn mixer_gates_and_envelope_amplitude_reach_the_output() {
        let mut ay = chip(AyVariant::Ym2149);
        // Tone and noise disabled on A: a constant level (sample playback through the volume).
        ay.write_register(7, 0x3f);
        ay.write_register(8, 9);
        assert!((0..50).all(|_| ay.tick() == [YM_LEVELS[19]; 2]));
        // Amplitude 0 silences the channel.
        ay.write_register(8, 0);
        assert_eq!(ay.tick(), [0, 0]);
        // Envelope mode follows the envelope level.
        ay.write_register(8, 0x10);
        ay.write_register(13, 0x0d);
        assert_eq!(ay.tick(), [0, 0]);
        for _ in 0..40 {
            ay.tick();
        }
        assert_eq!(ay.tick(), [YM_LEVELS[31]; 2]);
        // I/O ports are kept but not audible.
        let before = ay.tick();
        let effect = ay.write_register(14, 0xaa);
        assert_eq!(effect["audible"], false);
        assert_eq!(ay.tick(), before);
        assert_eq!(
            ay.write_register(16, 1)["ignored"],
            "the chip decodes registers 0..15 only"
        );
        assert_eq!(ay.write_register(1, 0xff)["after"], 0x0f);
    }

    #[test]
    fn stereo_mask_routes_channels() {
        let mut ay = chip(AyVariant::Ay8910);
        ay.write_register(7, 0x3f);
        for r in 8..11 {
            ay.write_register(r, 15);
        }
        assert_eq!(ay.tick(), [3 * AY_LEVELS[15]; 2]);
        // A left, B right, C off.
        ay.write_stereo_mask(0b00_10_01);
        assert_eq!(ay.tick(), [AY_LEVELS[15], AY_LEVELS[15]]);
        ay.write_stereo_mask(0b11_00_01);
        assert_eq!(ay.tick(), [2 * AY_LEVELS[15], AY_LEVELS[15]]);
    }
}
