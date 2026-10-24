//! SN76489-family PSG model: three square-wave tone channels and one noise
//! channel, clocked at the input clock divided by 16.
//!
//! References: Maxim's
//! "SN76489" notes on SMS Power (register protocol, counters, noise, LFSR
//! variants, attenuation, Game Gear stereo, power-on state) and the VGM 1.71
//! specification (header feedback, shift-register width and flag fields).
//! Every behaviour the documents leave open is a declared modelling choice in
//! [`model_scope`].
use crate::sample_resample::{bessel_i0, sin_pi};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The chip divides its input clock by 16 before clocking the counters.
pub const DIVIDER: u32 = 16;
pub const NTSC_CLOCK_HZ: u32 = 3_579_545;
pub const PAL_CLOCK_HZ: u32 = 3_546_893;
pub const MIN_CLOCK_HZ: u32 = 500_000;
pub const MAX_CLOCK_HZ: u32 = 4_000_000;
/// Longest rendered range in seconds.
pub const MAX_RENDER_SECONDS: u64 = 120;
pub const MAX_EVENTS: usize = 4096;

/// Per-channel amplitude for attenuation 0..15: 2 dB steps (ratio 10^-0.1)
/// from 8191, so four channels at full level sum without clipping; 15 is off.
pub const VOLUME: [i32; 16] = [
    8191, 6506, 5168, 4105, 3261, 2590, 2057, 1634, 1298, 1031, 819, 651, 517, 411, 326, 0,
];

/// Documented chip variants. The variant fixes the noise LFSR and the
/// meaning of tone period 0; nothing is inferred from a console name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PsgVariant {
    /// The SN76489 core integrated in Sega VDPs (Master System 2, Game Gear,
    /// Mega Drive): 16-bit LFSR with taps 0x0009, periods 0 and 1 hold the
    /// output high, honours the Game Gear stereo port.
    Sega,
    /// Discrete SN76489/SN76489AN (SG-1000, SC-3000H, BBC Micro,
    /// ColecoVision): 15-bit LFSR with taps 0x0003, period 0 counts as 0x400,
    /// period 1 holds the output high, no stereo port.
    Sn76489,
}

/// Resolved model parameters. VGM headers can declare other LFSR widths and
/// taps; the same generic shift rule applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PsgConfig {
    pub lfsr_width: u8,
    pub taps: u16,
    pub zero_period_is_1024: bool,
    pub negate_output: bool,
    pub stereo_port: bool,
}

impl PsgVariant {
    pub fn config(self) -> PsgConfig {
        match self {
            PsgVariant::Sega => PsgConfig {
                lfsr_width: 16,
                taps: 0x0009,
                zero_period_is_1024: false,
                negate_output: false,
                stereo_port: true,
            },
            PsgVariant::Sn76489 => PsgConfig {
                lfsr_width: 15,
                taps: 0x0003,
                zero_period_is_1024: true,
                negate_output: false,
                stereo_port: false,
            },
        }
    }
}

impl PsgConfig {
    pub fn check(&self) -> Result<(), String> {
        if !(2..=16).contains(&self.lfsr_width) {
            return Err(format!(
                "shift-register width {} is outside 2..16",
                self.lfsr_width
            ));
        }
        if self.taps == 0 || u32::from(self.taps) >= 1u32 << self.lfsr_width {
            return Err(format!(
                "noise taps 0x{:04x} must be nonzero and within the {}-bit register",
                self.taps, self.lfsr_width
            ));
        }
        Ok(())
    }
    pub fn seed(&self) -> u16 {
        1 << (self.lfsr_width - 1)
    }
    pub fn to_json(&self) -> Value {
        json!({"lfsr_width":self.lfsr_width,"taps":format!("0x{:04x}",self.taps),"lfsr_seed":format!("0x{:04x}",self.seed()),
            "tone_period_0":if self.zero_period_is_1024 {"counts as 0x400"} else {"holds the output high"},
            "tone_period_1":"holds the output high","negate_output":self.negate_output,"stereo_port":self.stereo_port})
    }
}

const SHIFT_RATES: [&str; 4] = ["clock/512", "clock/1024", "clock/2048", "tone2"];

pub fn register_name(register: u8) -> &'static str {
    [
        "tone0",
        "attenuation0",
        "tone1",
        "attenuation1",
        "tone2",
        "attenuation2",
        "noise",
        "attenuation3",
    ][usize::from(register & 7)]
}

/// What one byte written to the PSG port changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteEffect {
    pub latch: bool,
    pub register: u8,
    pub before: u16,
    pub after: u16,
    pub lfsr_reset: bool,
}

impl WriteEffect {
    pub fn to_json(&self) -> Value {
        let mut value = json!({"byte":if self.latch {"latch"} else {"data"},"register":register_name(self.register),
            "channel":self.register >> 1,"before":self.before,"after":self.after});
        if self.register == 6 {
            value["noise_mode"] = json!(if self.after & 4 != 0 {
                "white"
            } else {
                "periodic"
            });
            value["shift_rate"] = json!(SHIFT_RATES[usize::from(self.after & 3)]);
            value["lfsr_reset"] = json!(self.lfsr_reset);
        }
        value
    }
}

/// Register and counter state of one chip.
#[derive(Debug, Clone)]
pub struct Psg {
    config: PsgConfig,
    tone: [u16; 3],
    attenuation: [u8; 4],
    noise: u8,
    latch: u8,
    counter: [u16; 4],
    /// Tone flip-flops 0..2 and the noise counter's flip-flop.
    flip: [bool; 4],
    lfsr: u16,
    noise_bit: bool,
    stereo: u8,
    /// Channels left out of the output mix (bit c = channel c, 3 = noise);
    /// chip state is unaffected.
    output_mute: u8,
}

impl Psg {
    /// The documented Sega power-on state: tone and noise registers 0,
    /// attenuation 15. Latch, counters and flip-flops are declared choices.
    pub fn new(config: PsgConfig) -> Result<Self, String> {
        config.check()?;
        Ok(Self {
            config,
            tone: [0; 3],
            attenuation: [15; 4],
            noise: 0,
            latch: 0,
            counter: [0; 4],
            flip: [true; 4],
            lfsr: config.seed(),
            noise_bit: false,
            stereo: 0xff,
            output_mute: 0,
        })
    }
    /// Leave the channels in `mask` (bit c = channel c) out of the output mix.
    pub fn mute_output(&mut self, mask: u8) {
        self.output_mute = mask;
    }
    pub fn config(&self) -> PsgConfig {
        self.config
    }
    fn register(&self, register: u8) -> u16 {
        match register {
            0 | 2 | 4 => self.tone[usize::from(register >> 1)],
            6 => u16::from(self.noise),
            _ => u16::from(self.attenuation[usize::from(register >> 1)]),
        }
    }
    /// One byte on the PSG port. A latch byte (bit 7 set) selects a register
    /// and writes its low 4 bits; a data byte writes the latched register:
    /// the high 6 bits of a tone, or the low bits of an attenuation or noise
    /// register. Any write to the noise register resets the LFSR.
    pub fn write(&mut self, byte: u8) -> WriteEffect {
        let latch = byte & 0x80 != 0;
        if latch {
            self.latch = (byte >> 4) & 7;
        }
        let register = self.latch;
        let before = self.register(register);
        let mut lfsr_reset = false;
        match register {
            0 | 2 | 4 => {
                let tone = &mut self.tone[usize::from(register >> 1)];
                *tone = if latch {
                    (*tone & 0x3f0) | u16::from(byte & 0x0f)
                } else {
                    (*tone & 0x00f) | (u16::from(byte & 0x3f) << 4)
                };
            }
            6 => {
                self.noise = byte & 7;
                self.lfsr = self.config.seed();
                lfsr_reset = true;
            }
            _ => self.attenuation[usize::from(register >> 1)] = byte & 0x0f,
        }
        WriteEffect {
            latch,
            register,
            before,
            after: self.register(register),
            lfsr_reset,
        }
    }
    /// The Game Gear stereo port 0x06: bits 0..3 send channels 0..3 right,
    /// bits 4..7 send them left.
    pub fn write_stereo(&mut self, byte: u8) -> u8 {
        let before = self.stereo;
        self.stereo = byte;
        before
    }
    fn reload(&self, period: u16) -> u16 {
        if period == 0 && self.config.zero_period_is_1024 {
            0x400
        } else {
            period
        }
    }
    fn holds_high(&self, period: u16) -> bool {
        period == 1 || (period == 0 && !self.config.zero_period_is_1024)
    }
    fn shift(&mut self) {
        let bit = self.lfsr & 1;
        let feedback = if self.noise & 4 != 0 {
            (self.lfsr & self.config.taps).count_ones() as u16 & 1
        } else {
            bit
        };
        self.noise_bit = bit != 0;
        self.lfsr = (self.lfsr >> 1) | (feedback << (self.config.lfsr_width - 1));
    }
    /// One internal clock tick (input clock / 16), then the mixed output.
    pub fn tick(&mut self) -> [i32; 2] {
        fn step(counter: &mut u16, reload: u16) -> bool {
            if *counter > 0 {
                *counter -= 1;
            }
            if *counter == 0 {
                *counter = reload;
                true
            } else {
                false
            }
        }
        for channel in 0..3 {
            let period = self.tone[channel];
            let reload = self.reload(period);
            if step(&mut self.counter[channel], reload) {
                self.flip[channel] = !self.flip[channel];
            }
            if self.holds_high(period) {
                self.flip[channel] = true;
            }
        }
        let reload = match self.noise & 3 {
            0 => 0x10,
            1 => 0x20,
            2 => 0x40,
            _ => self.reload(self.tone[2]),
        };
        if step(&mut self.counter[3], reload) {
            self.flip[3] = !self.flip[3];
            if self.flip[3] {
                self.shift();
            }
        }
        let mut out = [0i32; 2];
        for channel in 0..4 {
            if self.output_mute & (1 << channel) != 0 {
                continue;
            }
            let high = if channel == 3 {
                self.noise_bit
            } else {
                self.flip[channel]
            };
            let level = VOLUME[usize::from(self.attenuation[channel])];
            let level = if high { level } else { -level };
            let (right, left) = if self.config.stereo_port {
                (
                    self.stereo & (1 << channel) != 0,
                    self.stereo & (0x10 << channel) != 0,
                )
            } else {
                (true, true)
            };
            if left {
                out[0] += level;
            }
            if right {
                out[1] += level;
            }
        }
        if self.config.negate_output {
            out = out.map(|v| -v);
        }
        out
    }
    pub fn state(&self) -> Value {
        json!({"tone":self.tone,"attenuation":self.attenuation,"noise":self.noise,"latched_register":register_name(self.latch),
            "counters":self.counter,"lfsr":format!("0x{:04x}",self.lfsr),"stereo":format!("0x{:02x}",self.stereo)})
    }
    #[cfg(test)]
    fn noise_bit(&self) -> bool {
        self.noise_bit
    }
}

/// The exact output rate and its rounded WAV header value.
pub fn native_rate(clock_hz: u32) -> (f64, u32) {
    let rate = f64::from(clock_hz) / f64::from(DIVIDER);
    (rate, rate.round() as u32)
}

pub fn check_clock(clock_hz: u32) -> Result<(), String> {
    if (MIN_CLOCK_HZ..=MAX_CLOCK_HZ).contains(&clock_hz) {
        Ok(())
    } else {
        Err(format!(
            "clock_hz must be {MIN_CLOCK_HZ}..{MAX_CLOCK_HZ}, such as {NTSC_CLOCK_HZ} (NTSC) or {PAL_CLOCK_HZ} (PAL)"
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PsgPort {
    /// The PSG data port (tone, attenuation and noise registers).
    #[default]
    Psg,
    /// The Game Gear stereo port 0x06.
    Stereo,
}

/// One byte for the chip at a native sample (input clock / 16 ticks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedWrite {
    pub native_sample: u64,
    pub port: PsgPort,
    pub data: u8,
}

pub struct Rendered {
    pub frames: Vec<[i16; 2]>,
    /// Effect of every applied write, in order.
    pub effects: Vec<Value>,
    pub peak: i32,
    pub final_state: Value,
}

/// Run the chip from power-on through `end` native samples, applying writes
/// before the tick of their sample in list order, and keep `start..end`.
pub fn render(
    config: PsgConfig,
    writes: &[TimedWrite],
    start: u64,
    end: u64,
) -> Result<Rendered, String> {
    render_muted(config, writes, start, end, 0)
}

/// [`render`] with the channels in `mute` (bit c = channel c) left out of the
/// output mix.
pub fn render_muted(
    config: PsgConfig,
    writes: &[TimedWrite],
    start: u64,
    end: u64,
    mute: u8,
) -> Result<Rendered, String> {
    if writes
        .windows(2)
        .any(|w| w[1].native_sample < w[0].native_sample)
    {
        return Err("writes must be in time order".into());
    }
    let mut psg = Psg::new(config)?;
    psg.mute_output(mute);
    let mut frames = Vec::with_capacity(end.saturating_sub(start) as usize);
    let mut effects = Vec::with_capacity(writes.len());
    let mut next = writes.iter().peekable();
    let mut peak = 0;
    for n in 0..end {
        while let Some(write) = next.next_if(|w| w.native_sample == n) {
            effects.push(match write.port {
                PsgPort::Psg => psg.write(write.data).to_json(),
                PsgPort::Stereo => {
                    let before = psg.write_stereo(write.data);
                    json!({"register":"stereo","before":format!("0x{before:02x}"),"after":format!("0x{:02x}",write.data),
                        "left_channels":(0..4).filter(|c| write.data & (0x10 << c) != 0).collect::<Vec<_>>(),
                        "right_channels":(0..4).filter(|c| write.data & (1 << c) != 0).collect::<Vec<_>>()})
                }
            });
        }
        let out = psg.tick();
        if n >= start {
            peak = peak.max(out[0].abs()).max(out[1].abs());
            frames.push(out.map(|v| v.clamp(-32768, 32767) as i16));
        }
    }
    Ok(Rendered {
        frames,
        effects,
        peak,
        final_state: psg.state(),
    })
}

/// Tone period for a frequency: clock / (32 * f), rounded.
pub fn period_for(clock_hz: u32, frequency: f64) -> f64 {
    (f64::from(clock_hz) / (32.0 * frequency)).round()
}

pub fn tone_hz(clock_hz: u32, period: u16) -> f64 {
    f64::from(clock_hz) / (32.0 * f64::from(period))
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PsgPitch {
    /// The 10-bit tone register value, 2..1023 (0 and 1 do not sound a tone).
    Period { period: u16 },
    /// Equal temperament against an explicit A4; the period is rounded and
    /// the error reported in cents.
    MidiNote { note: u8, a4_hz: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoiseMode {
    White,
    Periodic,
}

/// Noise settings; which fields apply depends on the chip.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PsgNoise {
    /// SN76489: white or periodic. Game Boy: white (15-bit LFSR) or periodic
    /// (7-bit LFSR). NES: white (feedback from bit 1) or periodic (bit 6).
    /// Not used on the AY-3-8910/YM2149.
    #[serde(default)]
    pub mode: Option<NoiseMode>,
    /// SN76489 only: 0 = clock/512, 1 = clock/1024, 2 = clock/2048, 3 =
    /// follow tone 2's current period (written separately).
    #[serde(default)]
    pub rate: Option<u8>,
    /// AY-3-8910/YM2149: the shared 5-bit noise period 1..31 (shift rate
    /// clock/(16*period)). NES: the $400E period index 0..15. HuC6280: the R7
    /// noise frequency field 0..31 (clock clock/(64*(31-field))).
    #[serde(default)]
    pub period: Option<u8>,
    /// Game Boy only: NR43 divisor code 0..7 (0 counts as 0.5).
    #[serde(default)]
    pub divisor: Option<u8>,
    /// Game Boy only: NR43 clock shift 0..13.
    #[serde(default)]
    pub shift: Option<u8>,
    /// WonderSwan only: the $8E LFSR tap mode 0..7 (sequence lengths 32767,
    /// 1953, 254, 217, 73, 63, 42, 28); the LFSR advances at the channel 4
    /// sample rate set by pitch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taps: Option<u8>,
}

/// A note turned into register writes at its start and a release at its end.
/// SN76489: tone or noise control with attenuation, then attenuation 15.
/// AY-3-8910/YM2149: tone and/or noise with a fixed volume, then volume 0 and
/// the channel's mixer bits closed. Game Boy: a triggered pulse, wave or noise
/// note, then its DAC turned off.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PsgNote {
    /// SN76489: 0..2 tone channels, 3 noise. AY: 0..2 (A, B, C). Game Boy:
    /// 0 = CH1 pulse, 1 = CH2 pulse, 2 = CH3 wave, 3 = CH4 noise. NES: 0 =
    /// pulse 1, 1 = pulse 2, 2 = triangle, 3 = noise, 4 = DMC. HuC6280: 0..5
    /// (noise on 4 and 5). WonderSwan: 0..3 (noise on 3).
    pub channel: u8,
    pub start_seconds: f64,
    pub duration_seconds: f64,
    /// Tone channels. The period is the chip's register value: SN 10-bit
    /// (2..1023), AY 12-bit (1..4095), Game Boy 11-bit (0..2047), NES 11-bit
    /// timer t (pulse 8..2047, frequency clock/(16(t+1)); triangle 0..2047,
    /// clock/(32(t+1))), HuC6280 12-bit divider (1..4095, clock/(32F)),
    /// WonderSwan 11-bit divisor (0..2047, clock/(32(2048-d)); for noise the
    /// LFSR rate clock/(2048-d)).
    #[serde(default)]
    pub pitch: Option<PsgPitch>,
    /// SN76489 noise channel, AY noise on a channel (with or without pitch),
    /// Game Boy CH4, NES noise.
    #[serde(default)]
    pub noise: Option<PsgNoise>,
    /// SN76489 only: attenuation 0 (loudest) .. 14 in 2 dB steps.
    #[serde(default)]
    pub attenuation: Option<u8>,
    /// AY: fixed amplitude 1..15. Game Boy pulse and noise: initial volume
    /// 0..15. NES pulse and noise: constant volume 0..15. HuC6280: AL 0..31
    /// (1.5 dB steps). WonderSwan: 0..15 on both sides.
    #[serde(default)]
    pub volume: Option<u8>,
    /// Game Boy and NES pulse only: duty 0..3 (12.5%, 25%, 50%, 75%); default 2.
    #[serde(default)]
    pub duty: Option<u8>,
    /// Game Boy pulse and noise only: volume envelope; default none.
    #[serde(default)]
    pub envelope: Option<crate::gb_apu::GbEnvelope>,
    /// Game Boy wave channel only: wave RAM samples and output level.
    #[serde(default)]
    pub wave: Option<crate::gb_apu::GbWave>,
    /// NES DMC (channel 4) only: the DPCM sample, rate and loop.
    #[serde(default)]
    pub dpcm: Option<crate::nes_apu::NesDpcm>,
    /// HuC6280 and WonderSwan tone notes: the channel's 32-word waveform in
    /// playback order, loaded at the note start (HuC6280 words 0..31,
    /// WonderSwan samples 0..15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wavetable: Option<Vec<u8>>,
}

/// An explicit byte for the chip.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PsgWriteInput {
    /// Time in seconds, rounded to the nearest tick; or use `at_native_sample` or `at_tick`.
    #[serde(default)]
    pub at_seconds: Option<f64>,
    /// Exact native sample from power-on: input clock / 16 on the SN76489,
    /// clock / 8 on the AY, clock / 32 on the Game Boy, clock / 16 on the NES,
    /// clock / 32 on the HuC6280 and clock / 128 on the WonderSwan (its first
    /// tick).
    #[serde(default)]
    pub at_native_sample: Option<u64>,
    /// Exact model tick from power-on: equal to the native sample except on
    /// the Game Boy, whose ticks are clock / 2 (16 per native sample), the
    /// NES, whose ticks are CPU cycles (16 per native sample), and the HuC6280
    /// and WonderSwan, whose ticks are input clock cycles (32 and 128 per
    /// native sample).
    #[serde(default)]
    pub at_tick: Option<u64>,
    /// SN76489: the PSG port or the Game Gear stereo port. AY, Game Boy and
    /// NES writes use `register`.
    #[serde(default)]
    pub port: PsgPort,
    /// AY: register 0..15. Game Boy: offset from 0xFF10 (0x00..0x16 sound
    /// registers, 0x20..0x2F wave RAM). NES: offset from $4000 (0x00..0x1F).
    /// HuC6280: PSG register R0..R9 ($0800..$0809). WonderSwan: I/O port
    /// minus 0x80 (0x00..0x1F for ports $80..$9F). Both accept 0x00..0x7F as
    /// VGM does and report other offsets as ignored. Not used on the SN76489.
    #[serde(default)]
    pub register: Option<u8>,
    /// WonderSwan only, instead of `register`: an internal RAM address
    /// (0x0000..0xFFFF) receiving `data`, as VGM command 0xC6 writes it; the
    /// wavetables are read from RAM at ($8F << 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<u16>,
    pub data: u8,
}

fn native_at(clock_hz: u32, seconds: f64) -> Result<u64, String> {
    if !(seconds.is_finite() && (0.0..=MAX_RENDER_SECONDS as f64).contains(&seconds)) {
        return Err(format!("times must be 0..{MAX_RENDER_SECONDS} seconds"));
    }
    Ok((seconds * f64::from(clock_hz) / f64::from(DIVIDER)).round() as u64)
}

/// Explicit writes in time order, each with its resolved native sample.
pub fn explicit_writes(
    clock_hz: u32,
    writes: &[PsgWriteInput],
) -> Result<Vec<TimedWrite>, (usize, String)> {
    let mut out = Vec::with_capacity(writes.len());
    for (index, write) in writes.iter().enumerate() {
        if write.register.is_some() {
            return Err((
                index,
                "the SN76489 has no register addresses; write latch and data bytes to the psg port"
                    .into(),
            ));
        }
        let native_sample = match (write.at_seconds, write.at_native_sample, write.at_tick) {
            (Some(seconds), None, None) => native_at(clock_hz, seconds).map_err(|e| (index, e))?,
            (None, Some(sample), None) | (None, None, Some(sample)) => sample,
            _ => {
                return Err((
                    index,
                    "give exactly one of at_seconds, at_native_sample or at_tick".into(),
                ));
            }
        };
        if out
            .last()
            .is_some_and(|w: &TimedWrite| w.native_sample > native_sample)
        {
            return Err((index, "writes must be in time order".into()));
        }
        out.push(TimedWrite {
            native_sample,
            port: write.port,
            data: write.data,
        });
    }
    Ok(out)
}

/// Register writes for notes, with a per-note report. Notes on one channel
/// must not overlap; writes at one sample keep note order.
pub fn note_writes(
    clock_hz: u32,
    notes: &[PsgNote],
) -> Result<(Vec<TimedWrite>, Vec<Value>), (usize, String)> {
    let mut writes = Vec::new();
    let mut reports = Vec::new();
    let mut spans: Vec<(u8, u64, u64)> = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        let fail = |message: String| (index, message);
        if note.channel > 3 {
            return Err(fail("channel must be 0..3".into()));
        }
        if note.volume.is_some()
            || note.duty.is_some()
            || note.envelope.is_some()
            || note.wave.is_some()
        {
            return Err(fail(
                "SN76489 notes take attenuation; volume, duty, envelope and wave belong to other chips".into(),
            ));
        }
        let Some(attenuation) = note.attenuation.filter(|a| *a <= 14) else {
            return Err(fail(
                "attenuation must be 0..14; 15 is silence and ends the note".into(),
            ));
        };
        if !(note.duration_seconds.is_finite() && note.duration_seconds > 0.0) {
            return Err(fail("duration_seconds must be positive".into()));
        }
        let start = native_at(clock_hz, note.start_seconds).map_err(fail)?;
        let end = native_at(clock_hz, note.start_seconds + note.duration_seconds).map_err(fail)?;
        if end <= start {
            return Err(fail("the note is shorter than one native sample".into()));
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
        let channel = note.channel << 5;
        let mut report = json!({"index":index,"channel":note.channel,"start_native_sample":start,
            "end_native_sample":end,"attenuation":attenuation,"attenuation_db":-2.0*f64::from(attenuation)});
        let mut at_start = Vec::new();
        match (note.channel, &note.pitch, note.noise) {
            (0..=2, Some(pitch), None) => {
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
                        let period = period_for(clock_hz, target);
                        (
                            period,
                            json!({"source":"midi_note","note":note,"a4_hz":a4_hz,"target_hz":target,
                            "cents_error":if (2.0..=1023.0).contains(&period) {1200.0*(tone_hz(clock_hz, period as u16)/target).log2()} else {0.0}}),
                        )
                    }
                };
                if !(2.0..=1023.0).contains(&period) {
                    return Err(fail(format!(
                        "tone period {period} is outside 2..1023 at this clock"
                    )));
                }
                let period = period as u16;
                report["period"] = json!(period);
                report["frequency_hz"] = json!(tone_hz(clock_hz, period));
                report["pitch"] = pitch_report;
                at_start.push(0x80 | channel | (period & 0x0f) as u8);
                at_start.push(((period >> 4) & 0x3f) as u8);
            }
            (3, None, Some(noise)) => {
                let (Some(mode), Some(rate), None, None, None) = (
                    noise.mode,
                    noise.rate,
                    noise.period,
                    noise.divisor,
                    noise.shift,
                ) else {
                    return Err(fail("SN76489 noise takes mode and rate only".into()));
                };
                if rate > 3 {
                    return Err(fail("noise rate must be 0..3".into()));
                }
                let white = u8::from(mode == NoiseMode::White) << 2;
                report["noise"] = json!({"mode":mode,"rate":rate,
                    "shift_rate":SHIFT_RATES[usize::from(rate)]});
                at_start.push(0xe0 | white | rate);
            }
            (3, _, _) => {
                return Err(fail(
                    "the noise channel takes noise (mode and rate) and no pitch".into(),
                ));
            }
            _ => {
                return Err(fail(
                    "tone channels take pitch and no noise settings".into(),
                ));
            }
        }
        at_start.push(0x90 | channel | attenuation);
        report["writes"] = json!(
            at_start
                .iter()
                .map(|b| format!("0x{b:02x}"))
                .collect::<Vec<_>>()
        );
        report["release_write"] = json!(format!("0x{:02x}", 0x9f | channel));
        for data in at_start {
            writes.push(TimedWrite {
                native_sample: start,
                port: PsgPort::Psg,
                data,
            });
        }
        writes.push(TimedWrite {
            native_sample: end,
            port: PsgPort::Psg,
            data: 0x9f | channel,
        });
        reports.push(report);
    }
    writes.sort_by_key(|w| w.native_sample);
    Ok((writes, reports))
}

pub fn model_scope() -> Value {
    json!({
        "renderer": "Tunemend SN76489-family PSG model; not original hardware audio or driver playback",
        "sources": ["SMS Power, Maxim: SN76489 notes", "VGM 1.71 specification (header PSG fields)"],
        "modelled": ["latch/data register protocol, including data bytes written to a latched attenuation or noise register",
            "three 10-bit tone down-counters at clock/16, reload at zero and output toggle; frequency clock/(32*N)",
            "tone period 1 (and 0 unless the variant counts 0 as 0x400) holds the output high",
            "noise counter reload 0x10/0x20/0x40 or tone 2's current period; LFSR shifts on the counter output's 0->1 change",
            "white (parity of tapped bits) and periodic (bit 0) feedback into the top bit; any noise register write resets the LFSR to its top bit",
            "4-bit attenuation in 2 dB steps, 15 silent", "Game Gear stereo port 0x06 per-channel left/right enables",
            "VGM output-negate flag"],
        "declared_choices": ["power-on: tones and noise 0, attenuation 15, latch on tone 0, counters 0, flip-flops high, LFSR at its top bit, stereo 0xFF",
            "outputs are bipolar (+level when high, -level when low); the real chip's unipolar output and DC drift are not modelled",
            "the noise output is the bit shifted out of the LFSR, held between shifts",
            "writes take effect before the tick of their native sample, in order"],
        "not_modelled": ["output DC leakage and analog filtering", "the Master System 2's clipped top three levels",
            "the /2 clock divider of SN94624-style parts", "XNOR noise feedback (NCR8496)", "T6W28 (Neo Geo Pocket) split channels",
            "a second chip in dual-chip VGM files", "write timing inside one clock/16 tick"],
        "equality": "Equal register writes and clock do not establish that a game, driver or hardware unit sounds the same."
    })
}

/// Kaiser-windowed sinc resampling from the native rate clock/16 to `rate`.
pub const RESAMPLE_ZERO_CROSSINGS: f64 = 32.0;
pub const RESAMPLE_KAISER_BETA: f64 = 9.0;
pub const RESAMPLE_CUTOFF: f64 = 0.9;
pub const RESAMPLE_PHASES: usize = 1024;

/// Output sample `m` sits at the exact source position `m * clock / (16 * rate)`.
/// Taps come from 1024 tabulated kernel phases (each normalized to unit DC
/// gain) interpolated linearly at the exact fractional position; outside the
/// rendered range the signal is silent.
pub fn resample(frames: &[[i16; 2]], clock_hz: u32, rate: u32) -> (Vec<[i16; 2]>, Value) {
    resample_divided(frames, clock_hz, DIVIDER, rate)
}

/// [`resample`] for a native rate of `clock / divider`.
pub fn resample_divided(
    frames: &[[i16; 2]],
    clock_hz: u32,
    divider: u32,
    rate: u32,
) -> (Vec<[i16; 2]>, Value) {
    let (exact, mut report) = resample_exact(frames, clock_hz, divider, rate);
    let (out, clipped) = to_pcm16(&exact);
    report["clipped_samples"] = json!(clipped);
    (out, report)
}

/// Round half away from zero to PCM16, counting clipped samples.
pub fn to_pcm16(samples: &[[f64; 2]]) -> (Vec<[i16; 2]>, u64) {
    let mut clipped = 0u64;
    let out = samples
        .iter()
        .map(|frame| {
            frame.map(|v| {
                let r = v.round();
                if !(-32768.0..=32767.0).contains(&r) {
                    clipped += 1;
                }
                r.clamp(-32768.0, 32767.0) as i16
            })
        })
        .collect();
    (out, clipped)
}

/// The resampled signal before rounding, for mixing several chips.
pub fn resample_exact(
    frames: &[[i16; 2]],
    clock_hz: u32,
    divider: u32,
    rate: u32,
) -> (Vec<[f64; 2]>, Value) {
    let native = f64::from(clock_hz) / f64::from(divider);
    let fc = RESAMPLE_CUTOFF * (f64::from(rate) / native).min(1.0);
    let half = RESAMPLE_ZERO_CROSSINGS / fc;
    let reach = half.ceil() as i64;
    let i0 = bessel_i0(RESAMPLE_KAISER_BETA);
    let weight = |x: f64| {
        if x.abs() >= half {
            return 0.0;
        }
        let u = x / half;
        let sinc = if x == 0.0 {
            1.0
        } else {
            sin_pi(fc * x) / (std::f64::consts::PI * fc * x)
        };
        fc * sinc * bessel_i0(RESAMPLE_KAISER_BETA * (1.0 - u * u).sqrt()) / i0
    };
    let table: Vec<Vec<f64>> = (0..=RESAMPLE_PHASES)
        .map(|j| {
            let frac = j as f64 / RESAMPLE_PHASES as f64;
            let taps: Vec<f64> = (1 - reach..=reach)
                .map(|k| weight(k as f64 - frac))
                .collect();
            let sum: f64 = taps.iter().sum();
            taps.into_iter().map(|w| w / sum).collect()
        })
        .collect();
    let (step, den) = (u128::from(clock_hz), u128::from(divider) * u128::from(rate));
    let count = (frames.len() as u128 * den).div_ceil(step) as u64;
    let mut out = Vec::with_capacity(count as usize);
    for m in 0..u128::from(count) {
        let position = m * step;
        let (base, rem) = ((position / den) as i64, position % den);
        let scaled = rem * RESAMPLE_PHASES as u128;
        let (index, frac) = ((scaled / den) as usize, (scaled % den) as f64 / den as f64);
        let (a, b) = (&table[index], &table[index + 1]);
        let mut sum = [0f64; 2];
        for (tap, k) in (1 - reach..=reach).enumerate() {
            let n = base + k;
            if n < 0 || n as usize >= frames.len() {
                continue;
            }
            let w = a[tap] + (b[tap] - a[tap]) * frac;
            let frame = frames[n as usize];
            sum[0] += w * f64::from(frame[0]);
            sum[1] += w * f64::from(frame[1]);
        }
        out.push(sum);
    }
    let report = json!({"method":"kaiser_windowed_sinc_polyphase","source_rate_exact":format!("{clock_hz}/{divider}"),
        "source_rate_hz":native,"target_rate":rate,"source_samples":frames.len(),"target_samples":out.len(),
        "position":format!("output m at exact source position m*clock/({divider}*rate)"),"zero_crossings_per_side":RESAMPLE_ZERO_CROSSINGS,
        "kaiser_beta":RESAMPLE_KAISER_BETA,"cutoff_of_lower_nyquist":RESAMPLE_CUTOFF,
        "phases":format!("{RESAMPLE_PHASES} tabulated phases, each normalized to unit DC gain, interpolated linearly"),
        "rounding":"half away from zero to PCM16","clipped_samples":0,"boundary":"silence outside the rendered range"});
    (out, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sega() -> PsgConfig {
        PsgVariant::Sega.config()
    }

    fn run(psg: &mut Psg, ticks: usize) -> Vec<[i32; 2]> {
        (0..ticks).map(|_| psg.tick()).collect()
    }

    /// DFT magnitude of the left channel at `hz`.
    fn magnitude(signal: &[[i32; 2]], rate: f64, hz: f64) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, frame) in signal.iter().enumerate() {
            let phase = 2.0 * std::f64::consts::PI * hz * n as f64 / rate;
            re += f64::from(frame[0]) * phase.cos();
            im += f64::from(frame[0]) * phase.sin();
        }
        (re * re + im * im).sqrt()
    }

    fn rms(signal: &[[i32; 2]]) -> f64 {
        (signal.iter().map(|f| f64::from(f[0]).powi(2)).sum::<f64>() / signal.len() as f64).sqrt()
    }

    fn tone(psg: &mut Psg, channel: u8, period: u16, attenuation: u8) {
        psg.write(0x80 | channel << 5 | (period & 0xf) as u8);
        psg.write((period >> 4) as u8 & 0x3f);
        psg.write(0x90 | channel << 5 | attenuation);
    }

    #[test]
    fn tone_frequency_is_clock_over_32_n_within_one_dft_bin() {
        for (clock, period) in [
            (NTSC_CLOCK_HZ, 0x0fe),
            (PAL_CLOCK_HZ, 0x11d),
            (NTSC_CLOCK_HZ, 0x3ff),
        ] {
            let mut psg = Psg::new(sega()).unwrap();
            tone(&mut psg, 1, period, 0);
            let (rate, _) = native_rate(clock);
            let signal = run(&mut psg, rate as usize);
            // One second of signal: bins are 1 Hz apart.
            let bin_width = rate / signal.len() as f64;
            let expected = tone_hz(clock, period);
            let centre = (expected / bin_width).round() as i64;
            let best = (centre - 5..=centre + 5)
                .max_by(|&a, &b| {
                    magnitude(&signal, rate, a as f64 * bin_width).total_cmp(&magnitude(
                        &signal,
                        rate,
                        b as f64 * bin_width,
                    ))
                })
                .unwrap();
            assert!(
                (best as f64 * bin_width - expected).abs() <= bin_width,
                "{period}"
            );
            // The flip-flop toggles exactly every N ticks.
            let flips: Vec<usize> = signal
                .windows(2)
                .enumerate()
                .filter(|(_, w)| w[0] != w[1])
                .map(|(n, _)| n)
                .collect();
            assert!(flips.windows(2).all(|w| w[1] - w[0] == usize::from(period)));
        }
    }

    #[test]
    fn attenuation_steps_are_2_db() {
        for k in 0..14 {
            let db = 20.0 * (f64::from(VOLUME[k]) / f64::from(VOLUME[k + 1])).log10();
            assert!((db - 2.0).abs() < 0.05, "step {k}: {db}");
        }
        assert_eq!(VOLUME[15], 0);
        let level = |attenuation| {
            let mut psg = Psg::new(sega()).unwrap();
            tone(&mut psg, 0, 0x100, attenuation);
            rms(&run(&mut psg, 8192))
        };
        let (a, b) = (level(3), level(4));
        assert!((20.0 * (a / b).log10() - 2.0).abs() < 0.05);
        assert_eq!(level(15), 0.0);
    }

    fn lfsr_outputs(config: PsgConfig, white: bool, count: usize) -> String {
        let mut psg = Psg::new(config).unwrap();
        psg.write(if white { 0xe4 } else { 0xe0 });
        (0..count)
            .map(|_| {
                psg.shift();
                if psg.noise_bit() { '1' } else { '0' }
            })
            .collect()
    }

    #[test]
    fn lfsr_sequences_match_hand_traces_for_both_variants() {
        // Sega 16-bit, taps bits 0 and 3, seed 0x8000. The seed bit walks down
        // for 15 shifts (all outputs 0); when it reaches bit 3 (after 12
        // shifts) the parity feeds a 1 into bit 15 (0x8004), so output 16 is 1
        // and the register becomes 0x9000, and so on.
        assert_eq!(
            lfsr_outputs(sega(), true, 48),
            "000000000000000100000000000010010000000001000001"
        );
        // Discrete 15-bit, taps bits 0 and 1, seed 0x4000: the 1 reaches bit 1
        // after 13 shifts and feeds back immediately.
        let discrete = PsgVariant::Sn76489.config();
        assert_eq!(
            lfsr_outputs(discrete, true, 48),
            "000000000000001000000000000011000000000000101000"
        );
        // Periodic noise is the seed bit circulating: one 1 per 16 (or 15) shifts.
        assert_eq!(
            lfsr_outputs(sega(), false, 32),
            "0000000000000001".repeat(2)
        );
        assert_eq!(
            lfsr_outputs(discrete, false, 30),
            "000000000000001".repeat(2)
        );
        // White sequence lengths: 57337 (Sega taps 0x0009 do not give a
        // maximal sequence) and the maximal 32767 for the 15-bit taps.
        for (config, period) in [(sega(), 57337), (discrete, 32767)] {
            let mut psg = Psg::new(config).unwrap();
            psg.write(0xe4);
            let seed = psg.lfsr;
            let mut n = 0;
            loop {
                psg.shift();
                n += 1;
                if psg.lfsr == seed {
                    break;
                }
            }
            assert_eq!(n, period);
        }
    }

    #[test]
    fn periodic_noise_repeats_every_16_shifts_and_white_does_not() {
        let render = |control: u8| {
            let mut psg = Psg::new(sega()).unwrap();
            psg.write(control);
            psg.write(0xf0);
            run(&mut psg, 512 * 8)
        };
        // Rate 0 reloads 0x10: a shift every 32 ticks, 16 shifts = 512 ticks.
        let periodic = render(0xe0);
        assert!((512..512 * 7).all(|n| periodic[n] == periodic[n + 512]));
        let highs = periodic[512..1024].iter().filter(|f| f[0] > 0).count();
        assert_eq!(highs, 32, "one high output per period, held 32 ticks");
        let white = render(0xe4);
        assert!((512..512 * 7).any(|n| white[n] != white[n + 512]));
    }

    #[test]
    fn tone_two_rate_follows_tone_two_register() {
        let mut psg = Psg::new(sega()).unwrap();
        tone(&mut psg, 2, 40, 15);
        psg.write(0xe3);
        psg.write(0xf0);
        let mut shifts = Vec::new();
        let mut last = psg.lfsr;
        for n in 0..2000 {
            psg.tick();
            if psg.lfsr != last {
                shifts.push(n);
                last = psg.lfsr;
            }
        }
        // Shifts every two reloads of the tone-2 period.
        assert!(shifts.windows(2).all(|w| w[1] - w[0] == 80), "{shifts:?}");
        // Changing tone 2 changes the noise rate at the next reload.
        psg.write(0xc0 | 10);
        psg.write(0);
        let start = shifts.len();
        for n in 2000..3000 {
            psg.tick();
            if psg.lfsr != last {
                shifts.push(n);
                last = psg.lfsr;
            }
        }
        assert!(shifts[start + 1..].windows(2).all(|w| w[1] - w[0] == 20));
    }

    #[test]
    fn latch_protocol_follows_documented_cases() {
        let mut psg = Psg::new(sega()).unwrap();
        // Latch tone 0 low nibble, then data sets the high six bits; the low
        // nibble applies immediately.
        let e = psg.write(0x8e);
        assert_eq!((e.latch, e.register, e.after), (true, 0, 0x00e));
        let e = psg.write(0x0f);
        assert_eq!((e.latch, e.register, e.after), (false, 0, 0x0fe));
        // The latch persists: another data byte rewrites the high bits only.
        assert_eq!(psg.write(0x01).after, 0x01e);
        // Data after a volume latch sets the attenuation (low 4 bits).
        psg.write(0x9f);
        let e = psg.write(0x30);
        assert_eq!((e.register, e.before, e.after), (1, 15, 0));
        // Data after a noise latch rewrites the noise register and resets the LFSR.
        psg.write(0xe4);
        for _ in 0..5 {
            psg.shift();
        }
        assert_ne!(psg.lfsr, 0x8000);
        let e = psg.write(0x02);
        assert_eq!((e.register, e.after, e.lfsr_reset), (6, 2, true));
        assert_eq!(psg.lfsr, 0x8000);
        // A latch for the high noise bit discards bit 3.
        assert_eq!(psg.write(0xef).after, 7);
        // Tone 1 and 2 latches address their own registers.
        psg.write(0xa5);
        psg.write(0x3f);
        assert_eq!(psg.tone, [0x01e, 0x3f5, 0]);
    }

    #[test]
    fn game_gear_stereo_masks_each_side_per_channel() {
        let mut psg = Psg::new(sega()).unwrap();
        tone(&mut psg, 0, 1, 0);
        tone(&mut psg, 1, 1, 2);
        // Periods 1 hold the output high: constant levels per channel.
        assert_eq!(psg.tick(), [8191 + 5168, 8191 + 5168]);
        psg.write_stereo(0xf0);
        assert_eq!(psg.tick(), [8191 + 5168, 0]);
        psg.write_stereo(0x0f);
        assert_eq!(psg.tick(), [0, 8191 + 5168]);
        // Channel 0 left only, channel 1 right only.
        psg.write_stereo(0x12);
        assert_eq!(psg.tick(), [8191, 5168]);
        // Without the stereo port the mask has no effect.
        let mut sms = Psg::new(PsgConfig {
            stereo_port: false,
            ..sega()
        })
        .unwrap();
        tone(&mut sms, 0, 1, 0);
        sms.write_stereo(0x00);
        assert_eq!(sms.tick(), [8191, 8191]);
    }

    #[test]
    fn period_zero_holds_high_on_sega_and_counts_1024_on_discrete_parts() {
        let mut sega_chip = Psg::new(sega()).unwrap();
        sega_chip.write(0x90);
        assert!(run(&mut sega_chip, 4096).iter().all(|f| *f == [8191, 8191]));
        let mut discrete = Psg::new(PsgVariant::Sn76489.config()).unwrap();
        discrete.write(0x90);
        let signal = run(&mut discrete, 4096);
        let flips: Vec<usize> = signal
            .windows(2)
            .enumerate()
            .filter(|(_, w)| w[0] != w[1])
            .map(|(n, _)| n)
            .collect();
        assert!(flips.windows(2).all(|w| w[1] - w[0] == 1024), "{flips:?}");
        // Period 1 holds high on both.
        discrete.write(0x81);
        assert!(run(&mut discrete, 64).iter().all(|f| *f == [8191, 8191]));
    }

    #[test]
    fn note_writes_follow_the_protocol_and_refuse_overlaps() {
        let notes = vec![PsgNote {
            channel: 1,
            start_seconds: 0.0,
            duration_seconds: 0.1,
            pitch: Some(PsgPitch::MidiNote {
                note: 69,
                a4_hz: 440.0,
            }),
            noise: None,
            attenuation: Some(2),
            volume: None,
            duty: None,
            envelope: None,
            wave: None,
            dpcm: None,
            wavetable: None,
        }];
        let (writes, reports) = note_writes(NTSC_CLOCK_HZ, &notes).unwrap();
        assert_eq!(reports[0]["period"], 254);
        let bytes: Vec<u8> = writes.iter().map(|w| w.data).collect();
        assert_eq!(bytes, [0xae, 0x0f, 0xb2, 0xbf]);
        let mut overlapping = notes.clone();
        overlapping.push(PsgNote {
            start_seconds: 0.05,
            ..notes[0].clone()
        });
        assert_eq!(note_writes(NTSC_CLOCK_HZ, &overlapping).unwrap_err().0, 1);
    }

    #[test]
    fn resampling_keeps_level_and_is_deterministic() {
        let mut psg = Psg::new(sega()).unwrap();
        tone(&mut psg, 0, 0x0fe, 0);
        let frames: Vec<[i16; 2]> = run(&mut psg, 22372)
            .into_iter()
            .map(|f| f.map(|v| v as i16))
            .collect();
        let (out, report) = resample(&frames, NTSC_CLOCK_HZ, 44100);
        assert_eq!(
            out.len() as u64,
            (22372u64 * 16 * 44100).div_ceil(3_579_545)
        );
        assert_eq!(report["clipped_samples"], 0);
        let level = |s: &[[i16; 2]]| {
            (s.iter().map(|f| f64::from(f[0]).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
        };
        // A 440 Hz square keeps its in-band harmonics: RMS within 1%.
        let native = level(&frames[2000..20000]);
        let resampled = level(&out[400..3900]);
        assert!(
            (resampled / native - 1.0).abs() < 0.01,
            "{resampled} {native}"
        );
        assert_eq!(resample(&frames, NTSC_CLOCK_HZ, 44100).0, out);
    }
}
