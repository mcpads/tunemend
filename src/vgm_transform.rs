//! Derives a new VGM file from a retained one with declared changes.
//!
//! Two operations, both writing a complete new file and never touching the
//! original:
//!
//! - A same-family copy changes one declared chip family (SN76489, AY-3-8910
//!   family, Game Boy DMG or NES APU): transposition by period rewrite,
//!   channel muting, a clock retarget with or without pitch preservation, and
//!   a variant or region change. Every other command is copied byte for byte.
//! - An explicit mapping moves the SN76489 tone and noise channels to an
//!   AY-3-8910 or YM2149: periods by clock ratio, attenuation to the nearest
//!   AY level, the noise onto a declared AY channel.
//!
//! Waits are never changed, so timing, totals and the loop length stay
//! exact. Every rewritten, inserted and dropped command is listed, header
//! fields are compared one by one, and each change the target cannot
//! represent is a named loss that the caller must accept. The output is
//! parsed again and its register state replayed against the plan.
use crate::ay::AyVariant;
use crate::vgm::{FIELDS, Kind, Vgm, hex};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Changed commands copied into the tool response; the report file has all.
pub const RESPONSE_CHANGES: usize = 32;

/// The chip family a same-family copy changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransformFamily {
    Sn76489,
    Ay8910,
    GbDmg,
    NesApu,
}

/// A variant or region the copied family should declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransformVariant {
    /// SN76489 in Sega VDPs: 16-bit LFSR taps 0x0009, Game Gear stereo port, period 0 held high.
    Sega,
    /// Discrete SN76489: 15-bit LFSR taps 0x0003, no stereo port, period 0 as 0x400.
    Sn76489,
    /// AY-3-8910 (chip type 0x00).
    Ay8910,
    /// YM2149 (chip type 0x10).
    Ym2149,
    /// NES APU at the NTSC 2A03 clock 1789773.
    NesApu,
    /// NES APU at the PAL 2A07 clock 1662607.
    NesApuPal,
}

/// Declared changes of a same-family copy. Give at least one.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyChanges {
    /// The family whose commands change; the header must declare it. Every
    /// other command is copied byte for byte.
    pub family: TransformFamily,
    /// Transpose tone channels by this many semitones (-48..48) by rewriting
    /// their period registers to the nearest period.
    #[serde(default)]
    pub transpose_semitones: Option<i32>,
    /// Tone channels to transpose, default all: SN76489 0..2, AY 0..2 (A, B,
    /// C), Game Boy 0..2 (pulse 1, pulse 2, wave), NES 0..2 (pulse 1, pulse
    /// 2, triangle). Noise, envelopes and DMC are not transposed.
    #[serde(default)]
    pub transpose_channels: Option<Vec<u8>>,
    /// Channels to silence: SN76489 0..3 (attenuation set to 15), AY 0..2
    /// (amplitude 0), Game Boy 0..3 (NR51 routing bits cleared), NES 0..4
    /// ($4015 enable bits cleared).
    #[serde(default)]
    pub mute_channels: Option<Vec<u8>>,
    /// New header clock for the family (the dual-chip and other flag bits are kept).
    #[serde(default)]
    pub clock_hz: Option<u32>,
    /// With a clock change: rewrite periods so pitches stay (default true;
    /// AY noise and envelope periods too), or false to keep every register
    /// and let pitch and chip timing move with the clock.
    #[serde(default)]
    pub keep_pitch: Option<bool>,
    /// New variant: sega or sn76489 (SN76489 LFSR, period-0 and stereo
    /// fields, needs VGM 1.51), ay8910 or ym2149 (AY chip type), nes_apu or
    /// nes_apu_pal (sets the NTSC or PAL clock).
    #[serde(default)]
    pub variant: Option<TransformVariant>,
}

/// The explicit SN76489 to AY-3-8910/YM2149 mapping.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnToAyMapping {
    /// Target part: ay8910 or ym2149 (fixed levels are the same 4-bit steps).
    pub variant: AyVariant,
    /// Target AY clock, such as 1789773 (MSX) or 2000000 (Atari ST).
    pub clock_hz: u32,
    /// AY channel (0 A, 1 B, 2 C) for SN76489 tone channels 0, 1 and 2 in
    /// order, or null to drop that tone; channels must differ. Default [0, 1, 2].
    #[serde(default)]
    pub tone_channels: Option<Vec<Option<u8>>>,
    /// AY channel that carries the SN76489 noise channel through the AY
    /// noise generator, default 2 (C). The AY combines tone and noise on one
    /// channel with a logical AND and one amplitude.
    #[serde(default)]
    pub noise_channel: Option<u8>,
    /// Drop the SN76489 noise channel instead of mapping it. Default false.
    #[serde(default)]
    pub drop_noise: Option<bool>,
}

/// A change the target cannot represent exactly. A transformation that
/// incurs a loss refuses unless the caller lists its code in `accept_losses`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LossCode {
    /// A transposed or converted period is outside the register range and is clamped to the nearest bound.
    PeriodOutOfRange,
    /// NES: new high period bits need a $4003/$4007/$400B write, which restarts the channel; the original has none there, so the high bits stay as written.
    PeriodNeedsRetrigger,
    /// A clock change keeps pitch, but divider- or table-based rates (SN76489 noise rates 0..2, Game Boy noise, NES noise and DMC) cannot be rescaled and move with the clock.
    RateScalesWithClock,
    /// A clock change makes the Game Boy frame sequencer or NES frame counter (length, envelope, sweep timing) run at another rate.
    FrameTimingScalesWithClock,
    /// SN76489 noise rate 3 follows tone 2, which is transposed, so the noise pitch moves too.
    NoiseFollowsTone2,
    /// The second SN76489 of a dual-chip file is copied unchanged.
    SecondChipNotTransformed,
    /// The new SN76489 variant has no Game Gear stereo port; the stereo writes stay but do nothing.
    StereoPortAbsent,
    /// The new SN76489 variant gives tone period 0 another meaning (held high or 0x400).
    ZeroPeriodMeaningChanges,
    /// The rewritten loop needs other bytes on a later pass, because the state when the loop re-enters differs from the first entry; the output follows the first pass.
    LoopEntryStateDiffers,
    /// SN76489 attenuation (2 dB steps) is rounded to the nearest AY fixed level (3 dB steps).
    VolumeRounded,
    /// The AY 17-bit noise sequence replaces the SN76489 15/16-bit sequence, and noise register writes no longer reset it.
    NoiseSequenceDiffers,
    /// SN76489 periodic noise has no AY equivalent and plays as white noise.
    PeriodicNoise,
    /// SN76489 noise shares an AY channel with a sounding tone: the channel plays tone AND noise at the louder of the two levels.
    NoiseSharesToneChannel,
    /// A sounding SN76489 tone channel is dropped by tone_channels.
    ToneChannelDropped,
    /// Sounding SN76489 noise is dropped (drop_noise).
    NoiseChannelDropped,
    /// Game Gear stereo writes are dropped; every AY channel stays centered.
    GameGearStereoDropped,
    /// Commands of the second SN76489 of a dual-chip file are dropped.
    SecondChipDropped,
    /// A DAC stream aimed at the SN76489 is copied but has no SN76489 to play on.
    PsgDacStreamOrphaned,
}

impl LossCode {
    pub fn name(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

pub enum Request<'a> {
    Copy(&'a CopyChanges),
    Map(&'a SnToAyMapping),
}

#[derive(Debug)]
pub struct Refusal {
    pub location: String,
    pub message: String,
}

fn refuse<T>(location: &str, message: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal {
        location: location.into(),
        message: message.into(),
    })
}

pub struct Outcome {
    pub bytes: Vec<u8>,
    /// The complete report; `changes` lists every changed command.
    pub report: Value,
    pub losses: Vec<LossCode>,
}

#[derive(Default)]
struct LossRow {
    occurrences: usize,
    first_index: usize,
    first_offset: usize,
    detail: String,
}

#[derive(Default)]
struct Losses(BTreeMap<LossCode, LossRow>);

impl Losses {
    fn add(&mut self, code: LossCode, index: usize, offset: usize, detail: impl Into<String>) {
        let row = self.0.entry(code).or_insert_with(|| LossRow {
            occurrences: 0,
            first_index: index,
            first_offset: offset,
            detail: detail.into(),
        });
        row.occurrences += 1;
    }
    fn json(&self) -> Vec<Value> {
        self.0
            .iter()
            .map(|(code, row)| {
                json!({"code":code,"occurrences":row.occurrences,"first_command_index":row.first_index,
                    "first_offset":row.first_offset,"detail":row.detail})
            })
            .collect()
    }
}

/// A rewrite of a run of consecutive family commands.
struct GroupResult {
    commands: Vec<Vec<u8>>,
    reasons: Vec<String>,
    /// Register image of the output after the group, for readback.
    image: Vec<u16>,
}

/// One original command, as the processors see it.
#[derive(Clone, Copy)]
struct Cmd<'a> {
    index: usize,
    offset: usize,
    raw: &'a [u8],
}

trait Processor: Clone {
    /// Whether the command belongs to the transformed family's write stream.
    fn member(&self, kind: Kind, raw: &[u8]) -> bool;
    fn group(&mut self, commands: &[Cmd], losses: &mut Losses) -> GroupResult;
    /// Replacement for a command outside the groups, if it changes.
    fn other(&mut self, _command: Cmd, _kind: Kind, _losses: &mut Losses) -> Option<Vec<Vec<u8>>> {
        None
    }
    /// The register image after each output command of the family, replayed
    /// on a fresh chip, for readback.
    fn replay(&self, output: &Vgm) -> Vec<(usize, Vec<u16>)>;
    /// The register image before any write.
    fn initial(&self) -> Vec<u16>;
    /// Applies output commands to the output state; returns its image.
    fn apply_output(&mut self, commands: &[Vec<u8>]) -> Vec<u16>;
    /// Kinds whose commands the transformation may change in the output.
    fn output_member(&self, kind: Kind, raw: &[u8]) -> bool;
}

/// Period scaling with rounding and range; `None` keeps a special value.
fn scale(divisor: f64, k: f64, range: (f64, f64)) -> (f64, f64, bool) {
    let exact = divisor * k;
    let rounded = exact.round();
    let clamped = rounded.clamp(range.0, range.1);
    let cents = 1200.0 * (clamped / exact).log2();
    (clamped, cents, clamped != rounded)
}

fn cents_text(cents: f64) -> String {
    format!("{cents:+.1} cents from the exact period")
}

// --- SN76489 -------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
struct SnState {
    latch: u8,
    tone: [u16; 3],
    att: [u8; 4],
    noise: u8,
}

impl SnState {
    fn new() -> Self {
        Self {
            latch: 0,
            tone: [0; 3],
            att: [15; 4],
            noise: 0,
        }
    }
    /// The latch protocol; returns the register reached (0..7).
    fn write(&mut self, byte: u8) -> u8 {
        if byte & 0x80 != 0 {
            self.latch = (byte >> 4) & 7;
        }
        let register = self.latch;
        match register {
            0 | 2 | 4 => {
                let tone = &mut self.tone[usize::from(register >> 1)];
                *tone = if byte & 0x80 != 0 {
                    (*tone & 0x3f0) | u16::from(byte & 0x0f)
                } else {
                    (*tone & 0x00f) | (u16::from(byte & 0x3f) << 4)
                };
            }
            6 => self.noise = byte & 7,
            _ => self.att[usize::from(register >> 1)] = byte & 0x0f,
        }
        register
    }
    fn registers(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.tone.to_vec();
        v.extend(self.att.iter().map(|&a| u16::from(a)));
        v.push(u16::from(self.noise));
        v
    }
}

#[derive(Clone)]
struct SnCopy {
    input: SnState,
    output: SnState,
    /// Period factor per tone channel.
    k: [f64; 3],
    clock_k: f64,
    keep_pitch_clock: bool,
    mute: [bool; 4],
    zero_is_1024: bool,
    target_zero_is_1024: bool,
    target_stereo: bool,
}

impl SnCopy {
    fn period(
        &self,
        channel: usize,
        raw: u16,
        at: Cmd,
        losses: &mut Losses,
    ) -> (u16, Option<String>) {
        let k = self.k[channel];
        if k == 1.0 || raw == 1 || (raw == 0 && !self.zero_is_1024) {
            return (raw, None);
        }
        let divisor = if raw == 0 { 1024.0 } else { f64::from(raw) };
        let (value, cents, clamped) = scale(divisor, k, (2.0, 1023.0));
        if clamped {
            losses.add(
                LossCode::PeriodOutOfRange,
                at.index,
                at.offset,
                format!(
                    "SN76489 tone {channel} period {raw} needs {:.1}, outside 2..1023",
                    divisor * k
                ),
            );
        }
        let value = value as u16;
        (
            value,
            Some(format!(
                "tone {channel} period {raw} -> {value} ({})",
                cents_text(cents)
            )),
        )
    }
}

fn sn_latch(register: u8, low: u8) -> u8 {
    0x80 | (register << 4) | (low & 0x0f)
}

impl Processor for SnCopy {
    fn member(&self, kind: Kind, _raw: &[u8]) -> bool {
        kind == Kind::PsgWrite
    }
    fn output_member(&self, kind: Kind, _raw: &[u8]) -> bool {
        kind == Kind::PsgWrite
    }
    fn group(&mut self, commands: &[Cmd], losses: &mut Losses) -> GroupResult {
        let first = commands[0];
        // Registers (0..7: tone 0, attenuation 0, ... noise, attenuation 3) the group reaches.
        let mut touched = [false; 8];
        for c in commands {
            touched[usize::from(self.input.write(c.raw[1]))] = true;
        }
        let mut target = self.input.clone();
        let mut reasons = Vec::new();
        for channel in 0..3 {
            let (value, reason) = self.period(channel, self.input.tone[channel], first, losses);
            if let Some(r) = reason.filter(|_| value != self.input.tone[channel]) {
                reasons.push(r);
            }
            target.tone[channel] = value;
        }
        for channel in 0..4 {
            if self.mute[channel] && target.att[channel] != 15 {
                reasons.push(format!(
                    "attenuation {channel} {} -> 15 (muted)",
                    target.att[channel]
                ));
                target.att[channel] = 15;
            }
        }
        let noise_audible = target.att[3] < 15;
        // Rate 3 follows tone 2's period: only a period that actually changes
        // (not a held or unchanged one) moves the noise.
        if noise_audible
            && self.input.noise & 3 == 3
            && self.k[2] != self.clock_k
            && target.tone[2] != self.input.tone[2]
        {
            losses.add(
                LossCode::NoiseFollowsTone2,
                first.index,
                first.offset,
                "noise rate 3 follows transposed tone 2",
            );
        }
        if noise_audible && self.input.noise & 3 != 3 && self.keep_pitch_clock {
            losses.add(
                LossCode::RateScalesWithClock,
                first.index,
                first.offset,
                format!(
                    "SN76489 noise rate {} is clock/{} and moves with the clock",
                    self.input.noise & 3,
                    512 << (self.input.noise & 3)
                ),
            );
        }
        if self.zero_is_1024 != self.target_zero_is_1024
            && (0..3).any(|c| target.tone[c] == 0 && target.att[c] < 15)
        {
            losses.add(
                LossCode::ZeroPeriodMeaningChanges,
                first.index,
                first.offset,
                "a sounding tone has period 0",
            );
        }
        // The original bytes, if they reach the target from the output state.
        let mut trial = self.output.clone();
        for c in commands {
            trial.write(c.raw[1]);
        }
        let original: Vec<Vec<u8>> = commands.iter().map(|c| c.raw.to_vec()).collect();
        if trial.registers() == target.registers() {
            self.output = trial;
            return GroupResult {
                commands: original,
                reasons: vec![],
                image: self.output.registers(),
            };
        }
        // Write every register the group reached, and any other that differs,
        // in register order; noise writes also reset the LFSR, so they stay.
        let mut bytes = Vec::new();
        for register in 0..8u8 {
            let r = usize::from(register);
            match register {
                0 | 2 | 4 => {
                    let (now, want) = (self.output.tone[r / 2], target.tone[r / 2]);
                    if now != want || touched[r] {
                        bytes.push(sn_latch(register, (want & 15) as u8));
                        if now >> 4 != want >> 4 || touched[r] {
                            bytes.push(((want >> 4) & 0x3f) as u8);
                        }
                    }
                }
                6 => {
                    if self.output.noise != target.noise || touched[r] {
                        bytes.push(sn_latch(6, target.noise));
                    }
                }
                _ => {
                    if self.output.att[r / 2] != target.att[r / 2] || touched[r] {
                        bytes.push(sn_latch(register, target.att[r / 2]));
                    }
                }
            }
        }
        for &b in &bytes {
            self.output.write(b);
        }
        GroupResult {
            commands: bytes.iter().map(|&b| vec![0x50, b]).collect(),
            reasons,
            image: self.output.registers(),
        }
    }
    fn other(&mut self, command: Cmd, kind: Kind, losses: &mut Losses) -> Option<Vec<Vec<u8>>> {
        match kind {
            Kind::SecondPsgWrite | Kind::SecondPsgStereo
                if self.k.iter().any(|&k| k != 1.0) || self.mute.iter().any(|&m| m) =>
            {
                losses.add(
                    LossCode::SecondChipNotTransformed,
                    command.index,
                    command.offset,
                    "second-chip SN76489 commands (0x30, 0x3F) are copied unchanged",
                );
                None
            }
            Kind::PsgStereo if !self.target_stereo => {
                losses.add(
                    LossCode::StereoPortAbsent,
                    command.index,
                    command.offset,
                    "Game Gear stereo writes stay in the file, but the new variant has no stereo port",
                );
                None
            }
            _ => None,
        }
    }
    fn initial(&self) -> Vec<u16> {
        SnState::new().registers()
    }
    fn apply_output(&mut self, commands: &[Vec<u8>]) -> Vec<u16> {
        for c in commands {
            self.output.write(c[1]);
        }
        self.output.registers()
    }
    fn replay(&self, output: &Vgm) -> Vec<(usize, Vec<u16>)> {
        let mut state = SnState::new();
        output
            .commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind == Kind::PsgWrite)
            .map(|(i, c)| {
                state.write(output.bytes[c.offset + 1]);
                (i, state.registers())
            })
            .collect()
    }
}

// --- Register-addressed families (AY, Game Boy, NES) ---------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RegFamily {
    Ay,
    Gb,
    Nes,
}

/// A period split over a low register and an optional high register.
#[derive(Clone, Copy, Debug)]
struct Pair {
    /// Tone channel, or None for AY noise and envelope periods.
    channel: Option<usize>,
    name: &'static str,
    lo: u8,
    hi: Option<u8>,
    /// Bits of the low register and of the high register that hold the period.
    lo_mask: u8,
    hi_mask: u8,
}

impl RegFamily {
    fn opcode(self) -> u8 {
        match self {
            RegFamily::Ay => 0xa0,
            RegFamily::Gb => 0xb3,
            RegFamily::Nes => 0xb4,
        }
    }
    fn kind(self) -> Kind {
        match self {
            RegFamily::Ay => Kind::AyWrite,
            RegFamily::Gb => Kind::GbWrite,
            RegFamily::Nes => Kind::NesWrite,
        }
    }
    fn registers(self) -> usize {
        match self {
            RegFamily::Ay => 16,
            RegFamily::Gb => 0x30,
            RegFamily::Nes => 0x20,
        }
    }
    fn pairs(self) -> Vec<Pair> {
        let tone = |channel, name, lo, hi, hi_mask| Pair {
            channel: Some(channel),
            name,
            lo,
            hi: Some(hi),
            lo_mask: 0xff,
            hi_mask,
        };
        match self {
            RegFamily::Ay => vec![
                tone(0, "tone A", 0, 1, 0x0f),
                tone(1, "tone B", 2, 3, 0x0f),
                tone(2, "tone C", 4, 5, 0x0f),
                Pair {
                    channel: None,
                    name: "noise",
                    lo: 6,
                    hi: None,
                    lo_mask: 0x1f,
                    hi_mask: 0,
                },
                Pair {
                    channel: None,
                    name: "envelope",
                    lo: 11,
                    hi: Some(12),
                    lo_mask: 0xff,
                    hi_mask: 0xff,
                },
            ],
            RegFamily::Gb => vec![
                tone(0, "pulse 1", 0x03, 0x04, 0x07),
                tone(1, "pulse 2", 0x08, 0x09, 0x07),
                tone(2, "wave", 0x0d, 0x0e, 0x07),
            ],
            RegFamily::Nes => vec![
                tone(0, "pulse 1", 0x02, 0x03, 0x07),
                tone(1, "pulse 2", 0x06, 0x07, 0x07),
                tone(2, "triangle", 0x0a, 0x0b, 0x07),
            ],
        }
    }
    /// Raw period to the divisor the frequency is inversely proportional to.
    fn divisor(self, pair: &Pair, raw: u32) -> f64 {
        let _ = pair;
        match self {
            RegFamily::Ay => f64::from(raw.max(1)),
            RegFamily::Gb => f64::from(2048 - raw),
            RegFamily::Nes => f64::from(raw + 1),
        }
    }
    fn raw(self, divisor: f64) -> u32 {
        match self {
            RegFamily::Ay => divisor as u32,
            RegFamily::Gb => 2048 - divisor as u32,
            RegFamily::Nes => divisor as u32 - 1,
        }
    }
    /// Divisor range of a pair.
    fn range(self, pair: &Pair) -> (f64, f64) {
        match (self, pair.name) {
            (RegFamily::Ay, "noise") => (1.0, 31.0),
            (RegFamily::Ay, "envelope") => (1.0, 65535.0),
            (RegFamily::Ay, _) => (1.0, 4095.0),
            (RegFamily::Gb, _) => (1.0, 2048.0),
            (RegFamily::Nes, "triangle") => (1.0, 2048.0),
            (RegFamily::Nes, _) => (9.0, 2048.0),
        }
    }
    fn label(self) -> &'static str {
        match self {
            RegFamily::Ay => "AY",
            RegFamily::Gb => "Game Boy",
            RegFamily::Nes => "NES",
        }
    }
}

#[derive(Clone)]
struct RegCopy {
    family: RegFamily,
    pairs: Vec<Pair>,
    input: [Vec<u8>; 2],
    output: [Vec<u8>; 2],
    /// Period factor per pair.
    k: Vec<f64>,
    mute: Vec<bool>,
    keep_pitch_clock: bool,
    clock_changed: bool,
}

impl RegCopy {
    fn new(
        family: RegFamily,
        k_tone: [f64; 3],
        clock_k: f64,
        keep_pitch_clock: bool,
        clock_changed: bool,
        mute: Vec<bool>,
    ) -> Self {
        let pairs = family.pairs();
        let k = pairs
            .iter()
            .map(|p| match p.channel {
                Some(c) => k_tone[c],
                None => clock_k,
            })
            .collect();
        let n = family.registers();
        Self {
            family,
            pairs,
            input: [vec![0; n], vec![0; n]],
            output: [vec![0; n], vec![0; n]],
            k,
            mute,
            keep_pitch_clock,
            clock_changed,
        }
    }
    fn modelled(&self, register: u8) -> bool {
        usize::from(register) < self.family.registers()
    }
    fn raw_period(&self, regs: &[u8], pair: &Pair) -> u32 {
        let lo = u32::from(regs[usize::from(pair.lo)] & pair.lo_mask);
        let hi = pair
            .hi
            .map_or(0, |h| u32::from(regs[usize::from(h)] & pair.hi_mask));
        lo | (hi << 8)
    }
    /// Value a mute register takes.
    fn muted(&self, register: u8, data: u8) -> Option<u8> {
        match self.family {
            RegFamily::Ay if (8..=10).contains(&register) => {
                self.mute[usize::from(register - 8)].then_some(0)
            }
            RegFamily::Gb if register == 0x15 => {
                let mask = (0..4)
                    .filter(|&c| self.mute[c])
                    .fold(0u8, |m, c| m | (0x11 << c));
                (mask != 0).then_some(data & !mask)
            }
            RegFamily::Nes if register == 0x15 => {
                let mask = (0..5)
                    .filter(|&c| self.mute[c])
                    .fold(0u8, |m, c| m | (1 << c));
                (mask != 0).then_some(data & !mask)
            }
            _ => None,
        }
    }
    fn image(&self) -> Vec<u16> {
        self.output
            .iter()
            .flatten()
            .map(|&r| u16::from(r))
            .collect()
    }
}

impl Processor for RegCopy {
    fn member(&self, kind: Kind, raw: &[u8]) -> bool {
        kind == self.family.kind() && self.modelled(raw[1] & 0x7f)
    }
    fn output_member(&self, kind: Kind, raw: &[u8]) -> bool {
        self.member(kind, raw)
    }
    fn group(&mut self, commands: &[Cmd], losses: &mut Losses) -> GroupResult {
        let first = commands[0];
        let writes: Vec<(usize, u8, u8)> = commands
            .iter()
            .map(|c| (usize::from(c.raw[1] >> 7), c.raw[1] & 0x7f, c.raw[2]))
            .collect();
        for &(chip, register, data) in &writes {
            self.input[chip][usize::from(register)] = data;
        }
        let touched =
            |chip: usize, register: u8| writes.iter().any(|w| w.0 == chip && w.1 == register);
        // Target period bits per (chip, pair).
        let mut targets: BTreeMap<(usize, usize), (u8, u8)> = BTreeMap::new();
        let mut reasons = Vec::new();
        for chip in 0..2 {
            for (p, pair) in self.pairs.iter().enumerate() {
                let written = touched(chip, pair.lo) || pair.hi.is_some_and(|h| touched(chip, h));
                if !written || self.k[p] == 1.0 {
                    continue;
                }
                let raw = self.raw_period(&self.input[chip], pair);
                if self.family == RegFamily::Nes && pair.name.starts_with("pulse") && raw < 8 {
                    continue;
                }
                let divisor = self.family.divisor(pair, raw);
                let (value, cents, clamped) = scale(divisor, self.k[p], self.family.range(pair));
                let new = self.family.raw(value);
                if clamped {
                    losses.add(
                        LossCode::PeriodOutOfRange,
                        first.index,
                        first.offset,
                        format!(
                            "{} {} period {raw} needs divisor {:.1}, outside {:?}",
                            self.family.label(),
                            pair.name,
                            divisor * self.k[p],
                            self.family.range(pair)
                        ),
                    );
                }
                if new != raw {
                    reasons.push(format!(
                        "{}{} period {raw} -> {new} ({})",
                        if chip == 1 { "second chip " } else { "" },
                        pair.name,
                        cents_text(cents)
                    ));
                }
                targets.insert(
                    (chip, p),
                    (
                        (new & 0xff) as u8 & pair.lo_mask,
                        (new >> 8) as u8 & pair.hi_mask,
                    ),
                );
            }
        }
        let mut out: Vec<(usize, u8, u8)> = Vec::new();
        let emit = |out: &mut Vec<(usize, u8, u8)>,
                    output: &mut [Vec<u8>; 2],
                    chip: usize,
                    register: u8,
                    data: u8| {
            out.push((chip, register, data));
            output[chip][usize::from(register)] = data;
        };
        let mut output = self.output.clone();
        for &(chip, register, data) in &writes {
            let mut data_out = data;
            for (p, pair) in self.pairs.iter().enumerate() {
                let Some(&(lo, hi)) = targets.get(&(chip, p)) else {
                    continue;
                };
                if register == pair.lo {
                    data_out = (data & !pair.lo_mask) | lo;
                } else if Some(register) == pair.hi {
                    // The low part must be in place before a high write that may trigger.
                    if !touched(chip, pair.lo)
                        && output[chip][usize::from(pair.lo)] & pair.lo_mask != lo
                    {
                        let value = (output[chip][usize::from(pair.lo)] & !pair.lo_mask) | lo;
                        emit(&mut out, &mut output, chip, pair.lo, value);
                    }
                    data_out = (data & !pair.hi_mask) | hi;
                }
            }
            if let Some(value) = self.muted(register, data) {
                if value != data {
                    reasons.push(format!(
                        "{}register 0x{register:02x} 0x{data:02x} -> 0x{value:02x} (muted)",
                        if chip == 1 { "second chip " } else { "" }
                    ));
                }
                data_out = value;
            }
            if self.keep_pitch_clock {
                let rate = match (self.family, register) {
                    (RegFamily::Gb, 0x12) => Some("Game Boy noise (NR43 divisor and shift)"),
                    (RegFamily::Nes, 0x0e) => Some("NES noise period index"),
                    (RegFamily::Nes, 0x10) => Some("NES DMC rate index"),
                    _ => None,
                };
                if let Some(what) = rate {
                    losses.add(
                        LossCode::RateScalesWithClock,
                        first.index,
                        first.offset,
                        format!("{what} is a divider or table rate and moves with the clock"),
                    );
                }
            }
            emit(&mut out, &mut output, chip, register, data_out);
        }
        // High parts that a group without a high write still needs.
        for (&(chip, p), &(_, hi)) in &targets {
            let pair = self.pairs[p];
            let Some(h) = pair.hi else { continue };
            if touched(chip, h) || output[chip][usize::from(h)] & pair.hi_mask == hi {
                continue;
            }
            if self.family == RegFamily::Nes {
                losses.add(
                    LossCode::PeriodNeedsRetrigger,
                    first.index,
                    first.offset,
                    format!("{} needs high bits {hi} but only its low register is written here; a high write would restart the channel", pair.name),
                );
                continue;
            }
            let keep = match self.family {
                RegFamily::Gb => output[chip][usize::from(h)] & 0x40,
                _ => output[chip][usize::from(h)] & !pair.hi_mask,
            };
            emit(&mut out, &mut output, chip, h, keep | hi);
        }
        if self.clock_changed && self.keep_pitch_clock && self.family != RegFamily::Ay {
            losses.add(
                LossCode::FrameTimingScalesWithClock,
                first.index,
                first.offset,
                format!(
                    "the {} frame {} runs from the chip clock",
                    self.family.label(),
                    if self.family == RegFamily::Gb {
                        "sequencer"
                    } else {
                        "counter"
                    }
                ),
            );
        }
        self.output = output;
        let opcode = self.family.opcode();
        GroupResult {
            commands: out
                .iter()
                .map(|&(chip, register, data)| vec![opcode, register | ((chip as u8) << 7), data])
                .collect(),
            reasons,
            image: self.image(),
        }
    }
    fn initial(&self) -> Vec<u16> {
        vec![0; 2 * self.family.registers()]
    }
    fn apply_output(&mut self, commands: &[Vec<u8>]) -> Vec<u16> {
        for c in commands {
            self.output[usize::from(c[1] >> 7)][usize::from(c[1] & 0x7f)] = c[2];
        }
        self.image()
    }
    fn replay(&self, output: &Vgm) -> Vec<(usize, Vec<u16>)> {
        let n = self.family.registers();
        let mut regs = [vec![0u8; n], vec![0u8; n]];
        output
            .commands
            .iter()
            .enumerate()
            .filter(|(_, c)| self.member(c.kind, &output.bytes[c.offset..c.offset + c.length]))
            .map(|(i, c)| {
                let raw = &output.bytes[c.offset..c.offset + c.length];
                regs[usize::from(raw[1] >> 7)][usize::from(raw[1] & 0x7f)] = raw[2];
                (i, regs.iter().flatten().map(|&r| u16::from(r)).collect())
            })
            .collect()
    }
}

// --- SN76489 to AY mapping ----------------------------------------------

#[derive(Clone)]
struct SnToAy {
    input: SnState,
    output: [u8; 16],
    /// AY clock / SN76489 clock.
    ratio: f64,
    tone_map: [Option<usize>; 3],
    noise_channel: Option<usize>,
    zero_is_1024: bool,
    max_cents: f64,
    max_level_error_db: f64,
}

/// SN76489 attenuation (2 dB steps, 15 silent) to the nearest AY fixed level
/// (3 dB steps, 0 silent), and the error in dB.
pub fn ay_level(attenuation: u8) -> (u8, f64) {
    if attenuation >= 15 {
        return (0, 0.0);
    }
    let sn_db = -2.0 * f64::from(attenuation);
    let step = 20.0 * 2f64.sqrt().log10();
    let level = (15.0 + sn_db / step).round().clamp(1.0, 15.0) as u8;
    let ay_db = -step * f64::from(15 - level);
    (level, ay_db - sn_db)
}

impl SnToAy {
    fn desired(&mut self, at: Cmd, losses: &mut Losses) -> ([u8; 16], Vec<String>) {
        let mut d = self.output;
        let mut reasons = Vec::new();
        let s = self.input.clone();
        let audible = |a: u8| a < 15;
        let noise_audible = audible(s.att[3]);
        if noise_audible && self.noise_channel.is_none() {
            losses.add(
                LossCode::NoiseChannelDropped,
                at.index,
                at.offset,
                "sounding SN76489 noise is dropped",
            );
        }
        let mut mixer = 0x3fu8;
        for tone in 0..3 {
            if audible(s.att[tone]) && self.tone_map[tone].is_none() {
                losses.add(
                    LossCode::ToneChannelDropped,
                    at.index,
                    at.offset,
                    format!("sounding SN76489 tone {tone} is dropped"),
                );
            }
        }
        for channel in 0..3usize {
            let tone = (0..3).find(|&t| self.tone_map[t] == Some(channel));
            let noise_mapped = self.noise_channel == Some(channel);
            let noise_here = noise_audible && noise_mapped;
            let mut level_source: Option<u8> = None;
            // The amplitude silences a channel, so mixer bits change only
            // where tone and noise share it and one of them falls silent.
            let tone_sounds = tone.is_some_and(|t| audible(s.att[t]));
            if let Some(t) = tone {
                let n = s.tone[t];
                let dc = n == 1 || (n == 0 && !self.zero_is_1024);
                if tone_sounds {
                    level_source = Some(s.att[t]);
                }
                if !dc && (tone_sounds || !noise_mapped) {
                    mixer &= !(1 << channel);
                }
                if !dc {
                    let divisor = if n == 0 { 1024.0 } else { f64::from(n) };
                    let (p, cents, clamped) = scale(divisor, 2.0 * self.ratio, (1.0, 4095.0));
                    if clamped {
                        losses.add(LossCode::PeriodOutOfRange, at.index, at.offset, format!("SN76489 tone {t} period {n} needs AY period {:.1}, outside 1..4095", divisor * 2.0 * self.ratio));
                    }
                    if audible(s.att[t]) {
                        self.max_cents = self.max_cents.max(cents.abs());
                    }
                    let p = p as u16;
                    d[2 * channel] = (p & 0xff) as u8;
                    d[2 * channel + 1] = (p >> 8) as u8;
                }
            }
            if noise_mapped && (noise_audible || !tone_sounds) {
                mixer &= !(8 << channel);
            }
            if noise_here {
                if level_source.is_some() {
                    losses.add(
                        LossCode::NoiseSharesToneChannel,
                        at.index,
                        at.offset,
                        format!(
                            "AY channel {channel} plays SN76489 tone {} AND noise at one level",
                            tone.unwrap_or(0)
                        ),
                    );
                }
                level_source = Some(level_source.map_or(s.att[3], |a| a.min(s.att[3])));
            }
            let (level, error) = level_source.map_or((0, 0.0), ay_level);
            if error.abs() > 0.1 {
                self.max_level_error_db = self.max_level_error_db.max(error.abs());
                losses.add(
                    LossCode::VolumeRounded,
                    at.index,
                    at.offset,
                    "SN76489 2 dB attenuation steps become AY 3 dB level steps",
                );
            }
            d[8 + channel] = level;
        }
        d[7] = mixer;
        if noise_audible && self.noise_channel.is_some() {
            losses.add(LossCode::NoiseSequenceDiffers, at.index, at.offset, "the AY 17-bit noise LFSR replaces the SN76489 sequence; noise writes no longer reset it");
            if s.noise & 4 == 0 {
                losses.add(
                    LossCode::PeriodicNoise,
                    at.index,
                    at.offset,
                    "SN76489 periodic noise plays as AY white noise",
                );
            }
            let rate = s.noise & 3;
            let reload = if rate == 3 {
                f64::from(match s.tone[2] {
                    0 if self.zero_is_1024 => 1024,
                    n => n.max(1),
                })
            } else {
                f64::from(16u16 << rate)
            };
            let (np, _, clamped) = scale(reload, 2.0 * self.ratio, (1.0, 31.0));
            if clamped {
                losses.add(
                    LossCode::PeriodOutOfRange,
                    at.index,
                    at.offset,
                    format!(
                        "SN76489 noise rate {rate} needs AY noise period {:.1}, outside 1..31",
                        reload * 2.0 * self.ratio
                    ),
                );
            }
            d[6] = np as u8;
        }
        for (r, (want, now)) in d.iter().zip(&self.output).enumerate().take(11) {
            if want != now {
                reasons.push(format!("AY register {r} 0x{now:02x} -> 0x{want:02x}"));
            }
        }
        (d, reasons)
    }
}

impl Processor for SnToAy {
    fn member(&self, kind: Kind, _raw: &[u8]) -> bool {
        kind == Kind::PsgWrite
    }
    fn output_member(&self, kind: Kind, _raw: &[u8]) -> bool {
        kind == Kind::AyWrite
    }
    fn group(&mut self, commands: &[Cmd], losses: &mut Losses) -> GroupResult {
        // The AY registers that carry each SN76489 register the group writes
        // are written even when unchanged, so the group sets the same
        // absolute values whatever state precedes it (such as a loop return).
        let mut forced = [false; 16];
        for c in commands {
            let register = self.input.write(c.raw[1]);
            let channel = match register {
                0..=5 => self.tone_map[usize::from(register / 2)],
                _ => self.noise_channel,
            };
            if let Some(ch) = channel {
                forced[7] = true;
                match register {
                    0 | 2 | 4 => {
                        forced[2 * ch] = true;
                        forced[2 * ch + 1] = true;
                    }
                    6 => forced[6] = true,
                    _ => forced[8 + ch] = true,
                }
            }
        }
        let (desired, reasons) = self.desired(commands[0], losses);
        let mut out = Vec::new();
        for r in 0..11usize {
            if desired[r] != self.output[r] || forced[r] {
                out.push(vec![0xa0, r as u8, desired[r]]);
            }
        }
        self.output = desired;
        GroupResult {
            commands: out,
            reasons: if reasons.is_empty() {
                vec!["no AY register changes".into()]
            } else {
                reasons
            },
            image: self.output.iter().map(|&r| u16::from(r)).collect(),
        }
    }
    fn other(&mut self, command: Cmd, kind: Kind, losses: &mut Losses) -> Option<Vec<Vec<u8>>> {
        match kind {
            Kind::PsgStereo => {
                losses.add(
                    LossCode::GameGearStereoDropped,
                    command.index,
                    command.offset,
                    "Game Gear stereo writes are dropped; AY channels stay centered",
                );
                Some(vec![])
            }
            Kind::SecondPsgWrite | Kind::SecondPsgStereo => {
                losses.add(
                    LossCode::SecondChipDropped,
                    command.index,
                    command.offset,
                    "second SN76489 commands are dropped",
                );
                Some(vec![])
            }
            Kind::DacStream if command.raw[0] == 0x90 && command.raw[2] & 0x7f == 0 => {
                losses.add(
                    LossCode::PsgDacStreamOrphaned,
                    command.index,
                    command.offset,
                    "a DAC stream set up for the SN76489 has no SN76489 in the new file",
                );
                None
            }
            _ => None,
        }
    }
    fn initial(&self) -> Vec<u16> {
        vec![0; 16]
    }
    fn apply_output(&mut self, commands: &[Vec<u8>]) -> Vec<u16> {
        for c in commands {
            if c[1] < 16 {
                self.output[usize::from(c[1])] = c[2];
            }
        }
        self.output.iter().map(|&r| u16::from(r)).collect()
    }
    fn replay(&self, output: &Vgm) -> Vec<(usize, Vec<u16>)> {
        let mut regs = [0u8; 16];
        output
            .commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind == Kind::AyWrite)
            .map(|(i, c)| {
                let raw = &output.bytes[c.offset..c.offset + 3];
                if raw[1] < 16 {
                    regs[usize::from(raw[1])] = raw[2];
                }
                (i, regs.iter().map(|&r| u16::from(r)).collect())
            })
            .collect()
    }
}

// --- Stream rewriting ----------------------------------------------------

struct Rewritten {
    /// Commands emitted for each original command.
    emitted: Vec<Vec<Vec<u8>>>,
    changes: Vec<Value>,
    /// (original index of the group's last command, output image).
    checkpoints: Vec<(usize, Vec<u16>)>,
    loop_consistent: Option<bool>,
}

fn groups(vgm: &Vgm, member: impl Fn(Kind, &[u8]) -> bool) -> Vec<(usize, usize)> {
    let loop_index = vgm
        .loop_index
        .as_ref()
        .and_then(|l| l.as_ref().ok().copied());
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (i, c) in vgm.commands.iter().enumerate() {
        if !member(c.kind, &vgm.bytes[c.offset..c.offset + c.length]) {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 + 1 == i && Some(i) != loop_index => last.1 = i,
            _ => out.push((i, i)),
        }
    }
    out
}

fn rewrite<P: Processor>(vgm: &Vgm, processor: &mut P, losses: &mut Losses) -> Rewritten {
    let cmd = |i: usize| {
        let c = vgm.commands[i];
        Cmd {
            index: i,
            offset: c.offset,
            raw: &vgm.bytes[c.offset..c.offset + c.length],
        }
    };
    let runs = groups(vgm, |k, r| processor.member(k, r));
    let mut emitted: Vec<Vec<Vec<u8>>> = (0..vgm.commands.len())
        .map(|i| vec![cmd(i).raw.to_vec()])
        .collect();
    let mut changes = Vec::new();
    let mut checkpoints = Vec::new();
    let mut first_pass = Vec::new();
    let mut run = runs.iter().peekable();
    let mut i = 0;
    while i < vgm.commands.len() {
        if let Some(&&(start, end)) = run.peek().filter(|r| r.0 == i) {
            run.next();
            let commands: Vec<Cmd> = (start..=end).map(cmd).collect();
            let result = processor.group(&commands, losses);
            let original: Vec<Vec<u8>> = commands.iter().map(|c| c.raw.to_vec()).collect();
            if result.commands != original {
                changes.push(json!({"commands":[start, end],"offset":commands[0].offset,
                    "before":original.iter().map(|b| hex(b)).collect::<Vec<_>>(),
                    "after":result.commands.iter().map(|b| hex(b)).collect::<Vec<_>>(),"reasons":result.reasons}));
                emitted[start] = result.commands.clone();
                for slot in &mut emitted[start + 1..=end] {
                    slot.clear();
                }
            }
            first_pass.push((start, end, result.commands));
            checkpoints.push((end, result.image));
            i = end + 1;
            continue;
        }
        let command = cmd(i);
        if let Some(replacement) = processor.other(command, vgm.commands[i].kind, losses) {
            changes.push(json!({"commands":[i, i],"offset":command.offset,"before":[hex(command.raw)],
                "after":replacement.iter().map(|b| hex(b)).collect::<Vec<_>>(),"reasons":[format!("{} dropped", vgm.commands[i].kind.name())]}));
            emitted[i] = replacement;
        }
        i += 1;
    }
    // A later pass re-enters the loop with the state at the stream end. The
    // fixed bytes are right if, replayed from that state, they reach the
    // register image the transformation wants for the second pass.
    let loop_consistent = vgm.loop_index.as_ref().and_then(|l| l.as_ref().ok().copied()).map(|l| {
        let mut wanted = processor.clone();
        let mut actual = processor.clone();
        let mut scratch = Losses::default();
        let differing = first_pass.iter().filter(|(start, ..)| *start >= l).find(|(start, end, commands)| {
            let input: Vec<Cmd> = (*start..=*end).map(cmd).collect();
            let image = actual.apply_output(commands);
            wanted.group(&input, &mut scratch).image != image
        });
        if let Some((start, ..)) = differing {
            losses.add(
                LossCode::LoopEntryStateDiffers,
                *start,
                vgm.commands[*start].offset,
                "this rewritten group comes out differently when the loop re-enters with the state at the stream end",
            );
        }
        differing.is_none()
    });
    Rewritten {
        emitted,
        changes,
        checkpoints,
        loop_consistent,
    }
}

// --- Header edits and assembly -----------------------------------------

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// The original header (up to the data start) and declared field edits.
struct Header {
    bytes: Vec<u8>,
    declared: Vec<(&'static str, String)>,
}

impl Header {
    fn set32(&mut self, offset: usize, value: u32, why: impl Into<String>) {
        put32(&mut self.bytes, offset, value);
        self.declared.push((field_name(offset), why.into()));
    }
    fn set8(&mut self, offset: usize, value: u8, why: impl Into<String>) {
        self.bytes[offset] = value;
        self.declared.push((field_name(offset), why.into()));
    }
}

fn field_name(offset: usize) -> &'static str {
    FIELDS
        .iter()
        .find(|f| f.0 == offset)
        .map_or("unnamed", |f| f.2)
}

/// Per original command: index of its first output command and how many.
type Placement = Vec<(usize, usize)>;

fn assemble(
    vgm: &Vgm,
    header: &Header,
    emitted: &[Vec<Vec<u8>>],
) -> Result<(Vec<u8>, Placement), Refusal> {
    let mut bytes = header.bytes.clone();
    let grown = bytes.len() - vgm.data_start;
    let mut placement = Vec::with_capacity(emitted.len());
    let mut count = 0usize;
    for commands in emitted {
        placement.push((count, commands.len()));
        for c in commands {
            bytes.extend_from_slice(c);
        }
        count += commands.len();
    }
    let new_stream_end = bytes.len();
    bytes.extend_from_slice(&vgm.bytes[vgm.stream_end..]);
    let delta = new_stream_end as i64 - vgm.stream_end as i64;
    let shift = |value: usize| (value as i64 + delta) as usize;
    let eof = le32(vgm.bytes, 4) as usize + 4;
    put32(&mut bytes, 4, (shift(eof).max(4) - 4) as u32);
    let gd3 = le32(vgm.bytes, 0x14) as usize;
    if gd3 != 0 {
        let at = 0x14 + gd3;
        if at < vgm.stream_end {
            return refuse(
                "/source",
                format!(
                    "the GD3 offset points at byte {at}, inside the header or command stream; such a file is not rewritten"
                ),
            );
        }
        put32(&mut bytes, 0x14, (shift(at) - 0x14) as u32);
    }
    if let Some(l) = vgm.loop_index.as_ref() {
        let l = *l.as_ref().map_err(|e| Refusal {
            location: "/source".into(),
            message: format!(
                "the loop offset is broken ({}); such a file is not rewritten",
                e.message
            ),
        })?;
        // The first output command at or after the loop command.
        let target = (l..emitted.len())
            .find(|&i| !emitted[i].is_empty())
            .expect("the end command is kept");
        let mut offset = vgm.data_start + grown;
        for (i, commands) in emitted.iter().enumerate() {
            if i == target {
                break;
            }
            offset += commands.iter().map(Vec::len).sum::<usize>();
        }
        put32(&mut bytes, 0x1c, (offset - 0x1c) as u32);
    }
    if grown > 0 {
        put32(&mut bytes, 0x34, (vgm.data_start + grown - 0x34) as u32);
    }
    Ok((bytes, placement))
}

/// Every header field whose raw bytes differ, with the reason.
fn header_changes(before: &Vgm, after: &[u8], declared: &[(&'static str, String)]) -> Vec<Value> {
    let limit_before = before.data_start.min(0x100);
    let after_start = Vgm::parse(after).map_or(0x40, |v| v.data_start).min(0x100);
    FIELDS
        .iter()
        .filter_map(|&(offset, size, name, _)| {
            let old = (offset + size <= limit_before).then(|| &before.bytes[offset..offset + size]);
            let new = (offset + size <= after_start).then(|| &after[offset..offset + size]);
            let zero = |b: Option<&[u8]>| b.is_none_or(|b| b.iter().all(|&x| x == 0));
            if old == new || (zero(old) && zero(new)) {
                return None;
            }
            let reason = declared
                .iter()
                .find(|d| d.0 == name)
                .map(|d| d.1.clone())
                .unwrap_or_else(|| match name {
                    "eof_offset" | "gd3_offset" | "loop_offset" | "vgm_data_offset" => "position bookkeeping: the command stream changed length".into(),
                    _ => "unexpected".into(),
                });
            Some(json!({"offset":offset,"field":name,"before":old.map(hex),"after":new.map(hex),"reason":reason}))
        })
        .collect()
}

/// Replays the output and compares it with the plan and the original.
fn readback<P: Processor>(
    input: &Vgm,
    output_bytes: &[u8],
    processor: &P,
    rewritten: &Rewritten,
    placement: &[(usize, usize)],
    input_member: impl Fn(Kind, &[u8]) -> bool,
    header: &[Value],
) -> Result<Value, String> {
    let output = Vgm::parse(output_bytes).map_err(|e| {
        format!(
            "the new file does not parse at byte {}: {}",
            e.offset, e.message
        )
    })?;
    let total_equal = output.total_samples() == input.total_samples();
    let loop_at = |v: &Vgm| {
        v.loop_index
            .as_ref()
            .and_then(|l| l.as_ref().ok())
            .map(|&i| v.commands[i].at)
    };
    let loop_equal = loop_at(&output) == loop_at(input);
    // Commands outside the transformed stream: same bytes, same order, same time.
    let others = |v: &Vgm, member: &dyn Fn(Kind, &[u8]) -> bool| -> Vec<(u64, Vec<u8>)> {
        v.commands
            .iter()
            .filter(|c| !member(c.kind, &v.bytes[c.offset..c.offset + c.length]))
            .map(|c| (c.at, v.bytes[c.offset..c.offset + c.length].to_vec()))
            .collect()
    };
    let input_others = others(input, &input_member);
    let output_others = others(&output, &|k, r| processor.output_member(k, r));
    let others_identical = input_others == output_others;
    // Register images after each rewritten group, replayed from the new bytes.
    let replayed: BTreeMap<usize, Vec<u16>> = processor.replay(&output).into_iter().collect();
    let initial = processor.initial();
    let mut checked = 0usize;
    let mut mismatched = Vec::new();
    for (end, image) in &rewritten.checkpoints {
        // Output commands emitted up to and including this group.
        let emitted: usize = placement[..=*end].iter().map(|p| p.1).sum();
        let found = match emitted {
            0 => &initial,
            n => replayed.range(..n).next_back().map_or(&initial, |(_, v)| v),
        };
        if found == image {
            checked += 1;
        } else {
            mismatched.push(*end);
        }
    }
    let gd3_identical = match (&input.gd3, &output.gd3) {
        (Some(Ok(a)), Some(Ok(b))) => input.bytes[a.offset..a.end] == output.bytes[b.offset..b.end],
        (None, None) => true,
        _ => false,
    };
    let tail_identical = input.bytes[input.stream_end..] == output.bytes[output.stream_end..];
    let unexpected_header: Vec<&Value> = header
        .iter()
        .filter(|h| h["reason"] == "unexpected")
        .collect();
    let report = json!({"parsed":true,"version":output.version_text(),"commands":output.commands.len(),
        "total_samples_equal":total_equal,"total_samples":output.total_samples(),"loop_at_same_sample":loop_equal,
        "loop_consistent_on_reentry":rewritten.loop_consistent,
        "other_commands_identical":others_identical,"other_commands":input_others.len(),
        "register_checkpoints_matched":checked,"register_checkpoints_mismatched":mismatched,
        "gd3_identical":gd3_identical,"bytes_after_end_identical":tail_identical,
        "only_declared_header_fields_changed":unexpected_header.is_empty()});
    if !(total_equal
        && loop_equal
        && others_identical
        && mismatched.is_empty()
        && gd3_identical
        && tail_identical
        && unexpected_header.is_empty())
    {
        return Err(format!("readback of the new file failed: {report}"));
    }
    Ok(report)
}

// --- Entry point ---------------------------------------------------------

fn family_clock_offset(family: TransformFamily) -> usize {
    match family {
        TransformFamily::Sn76489 => 0x0c,
        TransformFamily::Ay8910 => 0x74,
        TransformFamily::GbDmg => 0x80,
        TransformFamily::NesApu => 0x84,
    }
}

fn family_name(family: TransformFamily) -> &'static str {
    match family {
        TransformFamily::Sn76489 => "sn76489",
        TransformFamily::Ay8910 => "ay8910",
        TransformFamily::GbDmg => "gb_dmg",
        TransformFamily::NesApu => "nes_apu",
    }
}

fn check_channels(
    list: &Option<Vec<u8>>,
    count: u8,
    location: &str,
    what: &str,
) -> Result<Vec<bool>, Refusal> {
    let mut set = vec![false; usize::from(count)];
    for (i, &c) in list.iter().flatten().enumerate() {
        if c >= count {
            return refuse(
                &format!("{location}/{i}"),
                format!("{what} are 0..{}", count - 1),
            );
        }
        set[usize::from(c)] = true;
    }
    Ok(set)
}

fn copy(vgm: &Vgm, changes: &CopyChanges) -> Result<Outcome, Refusal> {
    let family = changes.family;
    let clock_offset = family_clock_offset(family);
    let raw_clock = vgm.u32_field(&format!(
        "{}_clock",
        if family == TransformFamily::GbDmg {
            "gameboy_dmg"
        } else {
            family_name(family)
        }
    ));
    let clock = raw_clock & 0x3fff_ffff;
    if clock == 0 {
        return refuse(
            "/copy/family",
            format!("the header declares no {} clock", family_name(family)),
        );
    }
    let semitones = changes.transpose_semitones.unwrap_or(0);
    if !(-48..=48).contains(&semitones) {
        return refuse(
            "/copy/transpose_semitones",
            "transpose_semitones must be -48..48",
        );
    }
    if changes.transpose_channels.is_some() && semitones == 0 {
        return refuse(
            "/copy/transpose_channels",
            "transpose_channels needs a nonzero transpose_semitones",
        );
    }
    let mute_count = match family {
        TransformFamily::Sn76489 | TransformFamily::GbDmg => 4,
        TransformFamily::Ay8910 => 3,
        TransformFamily::NesApu => 5,
    };
    let transposed = match &changes.transpose_channels {
        None => vec![true; 3],
        Some(_) => check_channels(
            &changes.transpose_channels,
            3,
            "/copy/transpose_channels",
            "tone channels",
        )?,
    };
    let mute = check_channels(
        &changes.mute_channels,
        mute_count,
        "/copy/mute_channels",
        "channels",
    )?;
    // Variant and clock.
    let mut header = Header {
        bytes: vgm.bytes[..vgm.data_start].to_vec(),
        declared: Vec::new(),
    };
    let mut context = Vec::new();
    let mut new_clock = changes.clock_hz;
    let mut sn_target = None;
    if let Some(variant) = changes.variant {
        match (family, variant) {
            (TransformFamily::Sn76489, TransformVariant::Sega | TransformVariant::Sn76489) => {
                if vgm.version < 0x151 || vgm.data_start <= 0x2b {
                    return refuse(
                        "/copy/variant",
                        "the SN76489 variant lives in the feedback, width and flags fields, which need VGM 1.51 and a header reaching 0x2C",
                    );
                }
                let preset = if variant == TransformVariant::Sega {
                    crate::psg::PsgVariant::Sega.config()
                } else {
                    crate::psg::PsgVariant::Sn76489.config()
                };
                header.bytes[0x28..0x2a].copy_from_slice(&preset.taps.to_le_bytes());
                header
                    .declared
                    .push(("sn76489_feedback", format!("variant {variant:?}")));
                header.set8(0x2a, preset.lfsr_width, format!("variant {variant:?}"));
                let flags = vgm.bytes[0x2b];
                let flags = (flags & !0x05)
                    | u8::from(preset.zero_period_is_1024)
                    | if preset.stereo_port { 0 } else { 4 };
                header.set8(
                    0x2b,
                    flags,
                    format!("variant {variant:?}: period-0 bit 0 and stereo-off bit 2"),
                );
                sn_target = Some(preset);
                context.push(format!(
                    "SN76489 noise LFSR becomes {} bits with taps 0x{:04x}",
                    preset.lfsr_width, preset.taps
                ));
            }
            (TransformFamily::Ay8910, TransformVariant::Ay8910 | TransformVariant::Ym2149) => {
                if vgm.version < 0x151 || vgm.data_start <= 0x79 {
                    return refuse(
                        "/copy/variant",
                        "the AY chip type field needs VGM 1.51 and a header reaching 0x7A",
                    );
                }
                if variant == TransformVariant::Ay8910 && vgm.bytes[0x79] & 0x10 != 0 {
                    return refuse(
                        "/copy/variant",
                        "the header halves the clock through YM2149 pin 26 (flag bit 4), which an AY-3-8910 does not have",
                    );
                }
                header.set8(
                    0x78,
                    if variant == TransformVariant::Ym2149 {
                        0x10
                    } else {
                        0x00
                    },
                    format!("variant {variant:?}"),
                );
                context.push(if variant == TransformVariant::Ym2149 {
                    "YM2149: 32-step envelope and 1.5 dB DAC steps; fixed levels keep their 3 dB steps".to_owned()
                } else {
                    "AY-3-8910: 16-step envelope and 3 dB DAC steps".to_owned()
                });
            }
            (TransformFamily::NesApu, TransformVariant::NesApu | TransformVariant::NesApuPal) => {
                let region_clock = if variant == TransformVariant::NesApu {
                    crate::nes_apu::NTSC_CLOCK_HZ
                } else {
                    crate::nes_apu::PAL_CLOCK_HZ
                };
                if new_clock.is_some_and(|c| c != region_clock) {
                    return refuse(
                        "/copy/clock_hz",
                        format!(
                            "variant {variant:?} sets the clock {region_clock}; omit clock_hz or give that value"
                        ),
                    );
                }
                new_clock = Some(region_clock);
                context.push(format!(
                    "NES {} tables (noise periods, DMC rates, frame counter steps)",
                    if variant == TransformVariant::NesApu {
                        "NTSC 2A03"
                    } else {
                        "PAL 2A07"
                    }
                ));
            }
            _ => {
                return refuse(
                    "/copy/variant",
                    format!(
                        "variant {variant:?} does not belong to family {}",
                        family_name(family)
                    ),
                );
            }
        }
    }
    let keep_pitch = changes.keep_pitch.unwrap_or(true);
    if changes.keep_pitch.is_some() && new_clock.is_none() {
        return refuse("/copy/keep_pitch", "keep_pitch applies to a clock change");
    }
    let clock_k = match new_clock {
        Some(c) => {
            let check = match family {
                TransformFamily::Sn76489 => crate::psg::check_clock(c),
                TransformFamily::Ay8910 => crate::ay::check_clock(c),
                TransformFamily::GbDmg => crate::gb_apu::check_clock(c),
                TransformFamily::NesApu => crate::nes_apu::check_clock(c),
            };
            check.map_err(|m| Refusal {
                location: "/copy/clock_hz".into(),
                message: m,
            })?;
            if c != clock {
                header.set32(
                    clock_offset,
                    (raw_clock & 0xc000_0000) | c,
                    format!("clock {clock} -> {c}"),
                );
                let cents = 1200.0 * (f64::from(c) / f64::from(clock)).log2();
                context.push(if keep_pitch {
                    format!("clock {clock} -> {c} Hz with periods rewritten to keep pitch")
                } else {
                    format!("clock {clock} -> {c} Hz with registers kept: every pitch and chip rate moves {cents:+.1} cents")
                });
            }
            if keep_pitch {
                f64::from(c) / f64::from(clock)
            } else {
                1.0
            }
        }
        None => 1.0,
    };
    let clock_changed = new_clock.is_some_and(|c| c != clock);
    let transpose_k = 2f64.powf(-f64::from(semitones) / 12.0);
    let k_tone: [f64; 3] =
        std::array::from_fn(|c| clock_k * if transposed[c] { transpose_k } else { 1.0 });
    if semitones == 0 && !mute.contains(&true) && header.declared.is_empty() {
        return refuse(
            "/copy",
            "declare at least one change: transpose_semitones, mute_channels, a different clock_hz or variant",
        );
    }
    let mut losses = Losses::default();
    let keep_pitch_clock = clock_changed && keep_pitch;
    let (bytes, rewritten, readback_report) = match family {
        TransformFamily::Sn76489 => {
            let (config, _) = vgm.psg_config().map_err(|m| Refusal {
                location: "/source".into(),
                message: m,
            })?;
            let target = sn_target.unwrap_or(config);
            let mut processor = SnCopy {
                input: SnState::new(),
                output: SnState::new(),
                k: k_tone,
                clock_k,
                keep_pitch_clock,
                mute: std::array::from_fn(|c| mute[c]),
                zero_is_1024: config.zero_period_is_1024,
                target_zero_is_1024: target.zero_period_is_1024,
                target_stereo: target.stereo_port,
            };
            finish(vgm, &mut processor, &mut losses, &header)?
        }
        _ => {
            let reg = match family {
                TransformFamily::Ay8910 => RegFamily::Ay,
                TransformFamily::GbDmg => RegFamily::Gb,
                _ => RegFamily::Nes,
            };
            if reg == RegFamily::Ay && vgm.ay_config().is_some_and(|r| r.is_err()) {
                return refuse(
                    "/source",
                    "the header declares an AY variant that is not modelled",
                );
            }
            let mut mute = mute.clone();
            mute.resize(5, false);
            let mut processor =
                RegCopy::new(reg, k_tone, clock_k, keep_pitch_clock, clock_changed, mute);
            finish(vgm, &mut processor, &mut losses, &header)?
        }
    };
    let operation = json!({"kind":"copy","family":family_name(family),"transpose_semitones":semitones,
        "transpose_channels":(semitones != 0).then(|| (0..3).filter(|&c| transposed[c]).collect::<Vec<_>>()),
        "mute_channels":(0..mute.len()).filter(|&c| mute[c]).collect::<Vec<_>>(),
        "clock_hz":{"before":clock,"after":new_clock.unwrap_or(clock)},"keep_pitch":clock_changed.then_some(keep_pitch),
        "variant":changes.variant});
    Ok(outcome(
        vgm,
        bytes,
        rewritten,
        &header,
        losses,
        operation,
        context,
        readback_report,
        None,
    ))
}

type Finished = (Vec<u8>, Rewritten, Value);

fn finish<P: Processor>(
    vgm: &Vgm,
    processor: &mut P,
    losses: &mut Losses,
    header: &Header,
) -> Result<Finished, Refusal> {
    let fresh = processor.clone();
    let rewritten = rewrite(vgm, processor, losses);
    let (bytes, placement) = assemble(vgm, header, &rewritten.emitted)?;
    let changed_header = header_changes(vgm, &bytes, &header.declared);
    let report = readback(
        vgm,
        &bytes,
        &fresh,
        &rewritten,
        &placement,
        |k, r| fresh.member(k, r),
        &changed_header,
    )
    .map_err(|m| Refusal {
        location: "".into(),
        message: m,
    })?;
    Ok((bytes, rewritten, report))
}

#[allow(clippy::too_many_arguments)]
fn outcome(
    vgm: &Vgm,
    bytes: Vec<u8>,
    rewritten: Rewritten,
    header: &Header,
    losses: Losses,
    operation: Value,
    context: Vec<String>,
    readback: Value,
    approximations: Option<Value>,
) -> Outcome {
    let original = vgm.commands.len();
    let output: usize = rewritten.emitted.iter().map(Vec::len).sum();
    let unchanged = (0..original)
        .filter(|&i| {
            rewritten.emitted[i].len() == 1
                && rewritten.emitted[i][0]
                    == vgm.bytes
                        [vgm.commands[i].offset..vgm.commands[i].offset + vgm.commands[i].length]
        })
        .count();
    let replacement: usize = rewritten
        .changes
        .iter()
        .map(|c| c["after"].as_array().map_or(0, Vec::len))
        .sum();
    let changed_header = header_changes(vgm, &bytes, &header.declared);
    let max_cents = rewritten
        .changes
        .iter()
        .flat_map(|c| c["reasons"].as_array().cloned().unwrap_or_default())
        .filter_map(|r| {
            r.as_str()
                .and_then(|s| s.split(" (").nth(1))
                .and_then(|s| s.split(' ').next())
                .and_then(|s| s.parse::<f64>().ok())
        })
        .fold(0f64, |m, c| m.max(c.abs()));
    let codes: Vec<LossCode> = losses.0.keys().copied().collect();
    let mut report = json!({"operation":operation,"header_changes":changed_header,
        "commands":{"original":original,"output":output,"unchanged":unchanged,"replaced":original - unchanged,
            "replacement_commands":replacement,"changed_groups":rewritten.changes.len(),
            "rule":"a changed group of consecutive commands is replaced as a whole; changes lists each group's original and output bytes"},
        "changes":rewritten.changes,"losses":losses.json(),"context_changes":context,
        "approximations":approximations.unwrap_or_else(|| json!({"max_period_rounding_cents":max_cents})),
        "timing":{"waits":"every wait is copied, so command times, total_samples and the loop length are unchanged","total_samples":vgm.total_samples()},
        "readback":readback,"bytes":bytes.len()});
    if rewritten.changes.is_empty() {
        report["note"] = json!("no command changed");
    }
    Outcome {
        bytes,
        report,
        losses: codes,
    }
}

fn map(vgm: &Vgm, mapping: &SnToAyMapping) -> Result<Outcome, Refusal> {
    let sn_clock = vgm.u32_field("sn76489_clock") & 0x3fff_ffff;
    if sn_clock == 0 {
        return refuse("/map_sn76489_to_ay", "the header declares no SN76489 clock");
    }
    let (config, _) = vgm.psg_config().map_err(|m| Refusal {
        location: "/source".into(),
        message: m,
    })?;
    if vgm.u32_field("ay8910_clock") & 0x3fff_ffff != 0 {
        return refuse(
            "/map_sn76489_to_ay",
            "the header already declares an AY8910; the mapping needs a free AY slot",
        );
    }
    crate::ay::check_clock(mapping.clock_hz).map_err(|m| Refusal {
        location: "/map_sn76489_to_ay/clock_hz".into(),
        message: m,
    })?;
    let tone_map: [Option<usize>; 3] = match &mapping.tone_channels {
        None => [Some(0), Some(1), Some(2)],
        Some(list) if list.len() == 3 => {
            let mut map = [None; 3];
            for (i, c) in list.iter().enumerate() {
                if let Some(c) = c {
                    if *c > 2 {
                        return refuse(
                            &format!("/map_sn76489_to_ay/tone_channels/{i}"),
                            "AY channels are 0..2",
                        );
                    }
                    if map[..i].contains(&Some(usize::from(*c))) {
                        return refuse(
                            &format!("/map_sn76489_to_ay/tone_channels/{i}"),
                            "two SN76489 tones cannot share one AY channel",
                        );
                    }
                    map[i] = Some(usize::from(*c));
                }
            }
            map
        }
        Some(_) => {
            return refuse(
                "/map_sn76489_to_ay/tone_channels",
                "give three entries, one per SN76489 tone channel",
            );
        }
    };
    let drop_noise = mapping.drop_noise.unwrap_or(false);
    if drop_noise && mapping.noise_channel.is_some() {
        return refuse(
            "/map_sn76489_to_ay/noise_channel",
            "drop_noise and noise_channel exclude each other",
        );
    }
    let noise_channel = if drop_noise {
        None
    } else {
        Some(usize::from(mapping.noise_channel.unwrap_or(2)))
    };
    if noise_channel.is_some_and(|c| c > 2) {
        return refuse("/map_sn76489_to_ay/noise_channel", "AY channels are 0..2");
    }
    // Header: drop the SN76489 clock, declare the AY, and reach VGM 1.51.
    let mut header = Header {
        bytes: vgm.bytes[..vgm.data_start].to_vec(),
        declared: Vec::new(),
    };
    if vgm.version < 0x151 {
        let reinterpreted: Vec<&str> = FIELDS
            .iter()
            .filter(|f| f.3 > vgm.version && f.3 <= 0x151 && f.0 + f.1 <= vgm.data_start.min(0x100))
            .filter(|f| {
                !matches!(
                    f.2,
                    "sn76489_flags"
                        | "vgm_data_offset"
                        | "sn76489_feedback"
                        | "sn76489_shift_register_width"
                )
            })
            .filter(|f| vgm.bytes[f.0..f.0 + f.1].iter().any(|&b| b != 0))
            .map(|f| f.2)
            .collect();
        if !reinterpreted.is_empty() {
            return refuse(
                "/source",
                format!(
                    "declaring an AY needs VGM 1.51, which would start applying these nonzero header fields: {}",
                    reinterpreted.join(", ")
                ),
            );
        }
        header.set32(
            8,
            0x151,
            format!(
                "version {} -> 1.51, the first version with the AY8910 fields",
                vgm.version_text()
            ),
        );
    }
    if vgm.data_start < 0x80 {
        header.bytes.resize(0x80, 0);
        header.declared.push((
            "vgm_data_offset",
            "the header grows to 0x80 bytes to hold the AY8910 fields".into(),
        ));
    }
    header.set32(0x0c, 0, "the SN76489 is replaced by the AY");
    header.set32(0x74, mapping.clock_hz, "the AY8910 clock of the mapping");
    header.set8(
        0x78,
        if mapping.variant == AyVariant::Ym2149 {
            0x10
        } else {
            0x00
        },
        format!("chip type for {:?}", mapping.variant),
    );
    header.set8(
        0x79,
        0,
        "AY flags: no pin-26 halving, default channel routing",
    );
    let ratio = f64::from(mapping.clock_hz) / f64::from(sn_clock);
    let mut processor = SnToAy {
        input: SnState::new(),
        output: [0; 16],
        ratio,
        tone_map,
        noise_channel,
        zero_is_1024: config.zero_period_is_1024,
        max_cents: 0.0,
        max_level_error_db: 0.0,
    };
    let mut losses = Losses::default();
    let fresh = processor.clone();
    let rewritten = rewrite(vgm, &mut processor, &mut losses);
    let (bytes, placement) = assemble(vgm, &header, &rewritten.emitted)?;
    let changed_header = header_changes(vgm, &bytes, &header.declared);
    let input_member = |k: Kind, _r: &[u8]| {
        matches!(
            k,
            Kind::PsgWrite | Kind::PsgStereo | Kind::SecondPsgWrite | Kind::SecondPsgStereo
        )
    };
    let readback_report = readback(
        vgm,
        &bytes,
        &fresh,
        &rewritten,
        &placement,
        input_member,
        &changed_header,
    )
    .map_err(|m| Refusal {
        location: "".into(),
        message: m,
    })?;
    let operation = json!({"kind":"map_sn76489_to_ay","from":{"family":"sn76489","clock_hz":sn_clock,"config":config.to_json()},
        "to":{"family":"ay8910","variant":mapping.variant,"clock_hz":mapping.clock_hz},
        "tone_channels":tone_map,"noise_channel":noise_channel,
        "rules":{"tone_period":"AY period = round(2 * SN76489 period * AY clock / SN76489 clock), 1..4095; periods that hold the SN76489 output high (1, and 0 on Sega parts) become a tone-disabled channel at its level",
            "level":"AY fixed level = 15 - round(2 * attenuation / 3.01), attenuation 15 = level 0",
            "noise":"AY noise period = round(2 * SN76489 reload * AY clock / SN76489 clock), 1..31, with reload 16, 32, 64 or tone 2's period; the noise channel's level goes to its AY channel",
            "mixer":"a channel's tone is enabled while its SN76489 tone sounds with a period that is not held high; noise is enabled on the noise channel while the SN76489 noise sounds"}});
    let context = vec![
        "the SN76489 model output is bipolar, the AY model output unipolar with DC".to_owned(),
        format!(
            "AY {:?} at {} Hz replaces the SN76489 at {} Hz",
            mapping.variant, mapping.clock_hz, sn_clock
        ),
    ];
    let approximations = json!({"max_period_rounding_cents":processor.max_cents,"max_level_error_db":processor.max_level_error_db});
    Ok(outcome(
        vgm,
        bytes,
        rewritten,
        &header,
        losses,
        operation,
        context,
        readback_report,
        Some(approximations),
    ))
}

/// Applies a declared transformation to a parsed VGM. The caller decides
/// which losses are acceptable.
pub fn transform(vgm: &Vgm, request: Request) -> Result<Outcome, Refusal> {
    match request {
        Request::Copy(changes) => copy(vgm, changes),
        Request::Map(mapping) => map(vgm, mapping),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attenuation_maps_to_the_nearest_ay_level() {
        assert_eq!(ay_level(0), (15, 0.0));
        assert_eq!(ay_level(15), (0, 0.0));
        // 6 dB is two 3.01 dB AY steps; 2 dB rounds to one step.
        assert_eq!(ay_level(3).0, 13);
        assert_eq!(ay_level(1).0, 14);
        assert!((ay_level(1).1 + 1.01).abs() < 0.01);
        assert_eq!(ay_level(14).0, 6);
    }

    #[test]
    fn periods_round_and_clamp_with_reported_cents() {
        let (value, cents, clamped) = scale(254.0, 2f64.powf(-1.0 / 12.0), (2.0, 1023.0));
        assert_eq!(value, 240.0);
        assert!(cents.abs() < 2.0 && !clamped);
        let (value, _, clamped) = scale(800.0, 2.0, (2.0, 1023.0));
        assert_eq!((value, clamped), (1023.0, true));
    }

    #[test]
    fn sn76489_state_follows_the_latch_protocol() {
        let mut state = SnState::new();
        assert_eq!(state.write(0x8e), 0);
        assert_eq!(state.write(0x0f), 0);
        assert_eq!(state.tone[0], 0xfe);
        assert_eq!(state.write(0x93), 1);
        // A data byte after an attenuation latch writes the attenuation.
        assert_eq!(state.write(0x05), 1);
        assert_eq!(state.att[0], 5);
        assert_eq!(state.write(0xe4), 6);
        assert_eq!(state.noise, 4);
    }
}
