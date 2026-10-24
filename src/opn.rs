//! OPN-family FM channel model (YM2203, YM2608 and YM2612 FM part) for
//! auditioning static register patches with declared playback context.
//!
//! References: Yamaha's OPN/OPNA application
//! manuals (register map, F-number formula, key code and detune tables, LFO
//! rates and AM/PM depths) and published YM2612 hardware analysis (log-sin
//! and exponent tables, envelope rate and increment tables, phase, detune,
//! modulation and feedback arithmetic). The model consumes register writes at
//! native sample boundaries. It does not model DAC quantization or the YM2612
//! ladder effect, SSG-EG, CSM or channel-3 special mode, timers, the SSG,
//! ADPCM and rhythm parts, write busy timing, or bit-exact hardware PM.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::OnceLock;

use crate::fm::{FmPatch, OPERATOR_MASKS, OpnChip};

/// Detune magnitude in phase-increment units by key code and DT bits 0..1.
/// DT bit 2 negates it.
const DETUNE: [[u8; 4]; 32] = [
    [0, 0, 1, 2],
    [0, 0, 1, 2],
    [0, 0, 1, 2],
    [0, 0, 1, 2],
    [0, 1, 2, 2],
    [0, 1, 2, 3],
    [0, 1, 2, 3],
    [0, 1, 2, 3],
    [0, 1, 2, 4],
    [0, 1, 3, 4],
    [0, 1, 3, 4],
    [0, 1, 3, 5],
    [0, 2, 4, 5],
    [0, 2, 4, 6],
    [0, 2, 4, 6],
    [0, 2, 5, 7],
    [0, 2, 5, 8],
    [0, 3, 6, 8],
    [0, 3, 6, 9],
    [0, 3, 7, 10],
    [0, 4, 8, 11],
    [0, 4, 8, 12],
    [0, 4, 9, 13],
    [0, 5, 10, 14],
    [0, 5, 11, 16],
    [0, 6, 12, 17],
    [0, 6, 13, 19],
    [0, 7, 14, 20],
    [0, 8, 16, 22],
    [0, 8, 16, 22],
    [0, 8, 16, 22],
    [0, 8, 16, 22],
];

/// Native samples per LFO counter step for register 22h frequencies 0..7.
///
/// A declared choice between two sources that differ by one sample. These are
/// the periods Game_Music_Emu's YM2612 core steps with when run as a black
/// box; Yamaha's rounded datasheet rates correspond to one sample more per
/// step. A hardware capture of frequency 6 or 7, where the gap is 12–20%,
/// would settle it.
const LFO_PERIODS: [u32; 8] = [108, 77, 71, 67, 62, 44, 8, 5];
/// Nominal LFO rates at an 8 MHz-class clock, for reporting only.
const LFO_NOMINAL_HZ: [f64; 8] = [3.98, 5.56, 6.02, 6.37, 6.88, 9.63, 48.1, 72.2];
/// Documented PM depth in cents for PMS 0..7.
const PM_DEPTH_CENTS: [f64; 8] = [0.0, 3.4, 6.7, 10.0, 14.0, 20.0, 40.0, 80.0];
/// Right shift of the 0..126 AM triangle for AMS 0..3 (0, 1.4, 5.9, 11.8 dB).
const AM_SHIFT: [u32; 4] = [8, 3, 1, 0];
/// Envelope increments for the eight counter phases of a rate below 48.
const EG_LOW: [[u32; 8]; 4] = [
    [0, 1, 0, 1, 0, 1, 0, 1],
    [0, 1, 0, 1, 1, 1, 0, 1],
    [0, 1, 1, 1, 0, 1, 1, 1],
    [0, 1, 1, 1, 1, 1, 1, 1],
];
/// Envelope increments for rates 48..59, scaled by 1, 2 or 4.
const EG_HIGH: [[u32; 8]; 4] = [
    [1, 1, 1, 1, 1, 1, 1, 1],
    [1, 1, 1, 2, 1, 1, 1, 2],
    [1, 2, 1, 2, 1, 2, 1, 2],
    [1, 2, 2, 2, 1, 2, 2, 2],
];
/// Named operators 1..4 occupy register slots 0, 2, 1, 3 (offsets 0, 8, 4, 12).
const NAMED_SLOT: [usize; 4] = [0, 2, 1, 3];
/// Named operator held by each register slot.
const SLOT_NAMED: [u8; 4] = [1, 3, 2, 4];
/// Maximum attenuation of the 10-bit envelope (about 96 dB).
const MAX_ATTENUATION: u32 = 0x3ff;
pub const MAX_NOTE_SECONDS: f64 = 30.0;
pub const MAX_TAIL_SECONDS: f64 = 30.0;
pub const MAX_NOTES: usize = 256;

/// Modulation inputs per algorithm: (target, source, uses previous sample),
/// in named operator indices 0..3. Inputs to operator 3 and outputs of
/// operator 2 take the previous sample, as the hardware evaluates operators
/// in register order 1, 3, 2, 4 with one-sample delayed paths.
const CONNECTIONS: [&[(usize, usize, bool)]; 8] = [
    &[(1, 0, false), (2, 1, true), (3, 2, false)],
    &[(2, 0, true), (2, 1, true), (3, 2, false)],
    &[(2, 1, true), (3, 0, false), (3, 2, false)],
    &[(1, 0, false), (3, 1, true), (3, 2, false)],
    &[(1, 0, false), (3, 2, false)],
    &[(1, 0, false), (2, 0, true), (3, 0, false)],
    &[(1, 0, false)],
    &[],
];
/// Carrier operators (named indices 0..3) per algorithm.
const CARRIERS: [&[usize]; 8] = [
    &[3],
    &[3],
    &[3],
    &[3],
    &[1, 3],
    &[1, 2, 3],
    &[1, 2, 3],
    &[0, 1, 2, 3],
];

/// Quarter-wave log-sin table in 4.8 fixed-point log2 attenuation.
fn logsin() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|i| {
            let x = ((i as f64 + 0.5) * std::f64::consts::PI / 512.0).sin();
            (-x.log2() * 256.0).round() as u32
        })
    })
}

/// Exponent table: 1024 * (2^((255 - i) / 256) - 1).
fn exponent() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|i| {
            (((255 - i) as f64 / 256.0).exp2() * 1024.0 - 1024.0).round() as u32
        })
    })
}

/// Key code from block and the top four F-number bits.
pub fn keycode(fnum: u16, block: u8) -> u8 {
    let bit = |n: u16| ((fnum >> n) & 1) as u8;
    let (f11, f10, f9, f8) = (bit(10), bit(9), bit(8), bit(7));
    let n3 = (f11 & (f10 | f9 | f8)) | ((f11 ^ 1) & f10 & f9 & f8);
    (block << 2) | (f11 << 1) | n3
}

/// 20-bit phase increment per native sample for one operator.
pub fn phase_increment(fnum: u16, block: u8, detune: u8, multiple: u8) -> u32 {
    let kc = keycode(fnum, block) as usize;
    let base = (u32::from(fnum) << block) >> 1;
    let magnitude = i32::from(DETUNE[kc][usize::from(detune & 3)]);
    let delta = if detune & 4 != 0 {
        -magnitude
    } else {
        magnitude
    };
    let detuned = (base as i32 + delta) as u32 & 0x1ffff;
    let multiplied = if multiple == 0 {
        detuned >> 1
    } else {
        detuned * u32::from(multiple)
    };
    multiplied & 0xfffff
}

/// Envelope increment applied at this envelope counter value.
pub fn envelope_increment(rate: u32, counter: u32) -> u32 {
    if rate < 2 {
        return 0;
    }
    let shift = 11u32.saturating_sub(rate >> 2);
    if counter & ((1 << shift) - 1) != 0 {
        return 0;
    }
    let step = ((counter >> shift) & 7) as usize;
    match rate {
        2..=5 => EG_LOW[0][step],
        6 | 7 => EG_LOW[2][step],
        8..=47 => EG_LOW[(rate & 3) as usize][step],
        48..=59 => EG_HIGH[(rate & 3) as usize][step] << ((rate - 48) / 4),
        _ => 8,
    }
}

/// Native samples a key-off release can take, at most, before every carrier
/// of `patch` reaches maximum attenuation: from attenuation 0 at the release
/// rate 4 × RR + 2 without key scaling (which only shortens it), starting
/// just after an envelope update, with one envelope clock per 3 samples.
pub fn release_bound_samples(patch: &crate::fm::FmPatch) -> u64 {
    let carriers = CARRIERS[usize::from(patch.algorithm_feedback & 7)];
    carriers
        .iter()
        .map(|&named| {
            let regs = &patch.operators[NAMED_SLOT[named]];
            let rate = (u32::from(regs[5] & 15) * 4 + 2).min(63);
            let shift = 11u32.saturating_sub(rate >> 2);
            (0..8u32)
                .map(|phase| {
                    let mut attenuation = 0;
                    let mut updates = 0u64;
                    while attenuation < MAX_ATTENUATION {
                        let step = (phase + updates as u32) & 7;
                        attenuation += envelope_increment(rate, step << shift);
                        updates += 1;
                    }
                    (updates + 1) * (1u64 << shift) * 3
                })
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Envelope {
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Debug, Clone)]
struct Operator {
    regs: [u8; 7],
    phase: u32,
    attenuation: u32,
    envelope: Envelope,
    keyed: bool,
}

impl Operator {
    fn new() -> Self {
        Self {
            regs: [0; 7],
            phase: 0,
            attenuation: MAX_ATTENUATION,
            envelope: Envelope::Release,
            keyed: false,
        }
    }
    fn rate(&self, kc: u8) -> u32 {
        let key_scale = u32::from(kc >> (3 - (self.regs[2] >> 6)));
        let field = match self.envelope {
            Envelope::Attack => u32::from(self.regs[2] & 31) * 2,
            Envelope::Decay => u32::from(self.regs[3] & 31) * 2,
            Envelope::Sustain => u32::from(self.regs[4] & 31) * 2,
            Envelope::Release => u32::from(self.regs[5] & 15) * 4 + 2,
        };
        if field == 0 {
            0
        } else {
            (field + key_scale).min(63)
        }
    }
    fn sustain_level(&self) -> u32 {
        let sl = u32::from(self.regs[5] >> 4);
        (if sl == 15 { 31 } else { sl }) << 5
    }
    fn key_on(&mut self, kc: u8) {
        if self.keyed {
            return;
        }
        self.keyed = true;
        self.phase = 0;
        self.envelope = Envelope::Attack;
        if self.rate(kc) >= 62 {
            self.attenuation = 0;
            self.envelope = Envelope::Decay;
        }
    }
    fn key_off(&mut self) {
        if self.keyed {
            self.keyed = false;
            self.envelope = Envelope::Release;
        }
    }
    fn clock_envelope(&mut self, kc: u8, counter: u32) {
        let increment = envelope_increment(self.rate(kc), counter);
        match self.envelope {
            Envelope::Attack => {
                if increment > 0 {
                    let a = self.attenuation as i32;
                    self.attenuation = (a + ((!a * increment as i32) >> 4)).max(0) as u32;
                }
                if self.attenuation == 0 {
                    self.envelope = Envelope::Decay;
                }
            }
            Envelope::Decay => {
                self.attenuation = (self.attenuation + increment).min(MAX_ATTENUATION);
                if self.attenuation >= self.sustain_level() {
                    self.envelope = Envelope::Sustain;
                }
            }
            Envelope::Sustain | Envelope::Release => {
                self.attenuation = (self.attenuation + increment).min(MAX_ATTENUATION);
            }
        }
    }
    fn level(&self, am: u32, ams: u8) -> u32 {
        let tremolo = if self.regs[3] & 0x80 != 0 {
            am >> AM_SHIFT[usize::from(ams)]
        } else {
            0
        };
        (self.attenuation + (u32::from(self.regs[1] & 0x7f) << 3) + tremolo).min(MAX_ATTENUATION)
    }
    /// Signed 14-bit output for a 10-bit phase modulation.
    fn output(&self, modulation: i32, level: u32) -> i32 {
        let index = (((self.phase >> 10) as i32 + modulation) & 0x3ff) as u32;
        let quarter = if index & 0x100 != 0 {
            !index & 0xff
        } else {
            index & 0xff
        };
        let attenuation = logsin()[quarter as usize] + (level << 2);
        let shift = attenuation >> 8;
        let magnitude = if shift > 13 {
            0
        } else {
            ((exponent()[(attenuation & 0xff) as usize] | 0x400) << 2) >> shift
        } as i32;
        if index & 0x200 != 0 {
            -magnitude
        } else {
            magnitude
        }
    }
}

#[derive(Debug, Clone)]
struct Channel {
    /// Register slot order: named operators 1, 3, 2, 4.
    slots: [Operator; 4],
    fnum: u16,
    block: u8,
    algorithm_feedback: u8,
    output_control: u8,
    feedback_history: [i32; 2],
    previous: [i32; 4],
}

impl Channel {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| Operator::new()),
            fnum: 0,
            block: 0,
            algorithm_feedback: 0,
            output_control: 0,
            feedback_history: [0; 2],
            previous: [0; 4],
        }
    }
    fn keycode(&self) -> u8 {
        keycode(self.fnum, self.block)
    }
    fn carriers_silent(&self) -> bool {
        CARRIERS[usize::from(self.algorithm_feedback & 7)]
            .iter()
            .all(|&n| self.slots[NAMED_SLOT[n]].level(0, 0) >= MAX_ATTENUATION)
    }
    fn render(&mut self, am: u32, pm_ratio: f64) -> i32 {
        let algorithm = usize::from(self.algorithm_feedback & 7);
        let feedback = u32::from((self.algorithm_feedback >> 3) & 7);
        let ams = (self.output_control >> 4) & 3;
        let mut current = [0i32; 4];
        for named in [0, 2, 1, 3] {
            let modulation = if named == 0 {
                if feedback == 0 {
                    0
                } else {
                    (self.feedback_history[0] + self.feedback_history[1]) >> (10 - feedback)
                }
            } else {
                CONNECTIONS[algorithm]
                    .iter()
                    .filter(|c| c.0 == named)
                    .map(|&(_, source, delayed)| {
                        if delayed {
                            self.previous[source]
                        } else {
                            current[source]
                        }
                    })
                    .sum::<i32>()
                    >> 1
            };
            let op = &self.slots[NAMED_SLOT[named]];
            current[named] = op.output(modulation, op.level(am, ams));
            if named == 0 {
                self.feedback_history = [current[0], self.feedback_history[0]];
            }
        }
        for op in &mut self.slots {
            let mut increment =
                phase_increment(self.fnum, self.block, op.regs[0] >> 4, op.regs[0] & 15);
            if pm_ratio != 1.0 {
                increment = ((f64::from(increment) * pm_ratio).round() as u32) & 0xfffff;
            }
            op.phase = (op.phase + increment) & 0xfffff;
        }
        self.previous = current;
        CARRIERS[algorithm]
            .iter()
            .map(|&n| current[n])
            .sum::<i32>()
            .clamp(-8192, 8191)
    }
}

/// Register-level OPN FM model; writes take effect at the next sample.
#[derive(Debug, Clone)]
pub struct OpnModel {
    chip: OpnChip,
    channels: Vec<Channel>,
    lfo: u8,
    lfo_counter: u32,
    lfo_timer: u32,
    envelope_counter: u32,
    envelope_timer: u32,
    frequency_latch: u8,
}

pub fn channel_count(chip: OpnChip) -> usize {
    if chip == OpnChip::Ym2203 { 3 } else { 6 }
}

fn label(chip: OpnChip) -> &'static str {
    match chip {
        OpnChip::Opn => "opn",
        OpnChip::Ym2203 => "ym2203",
        OpnChip::Ym2608 => "ym2608",
        OpnChip::Ym2612 => "ym2612",
    }
}

impl OpnModel {
    pub fn new(chip: OpnChip) -> Result<Self, String> {
        if chip == OpnChip::Opn {
            return Err(
                "declare ym2203, ym2608 or ym2612; the register family alone has no playback model"
                    .into(),
            );
        }
        Ok(Self {
            chip,
            channels: (0..channel_count(chip)).map(|_| Channel::new()).collect(),
            lfo: 0,
            lfo_counter: 0,
            lfo_timer: 0,
            envelope_counter: 0,
            envelope_timer: 0,
            frequency_latch: 0,
        })
    }
    fn stereo(&self) -> bool {
        self.chip != OpnChip::Ym2203
    }
    /// Applies one register write. Unmodelled registers are refused.
    pub fn write(&mut self, port: u8, address: u8, data: u8) -> Result<(), String> {
        let ports = if self.stereo() { 2 } else { 1 };
        if port >= ports {
            return Err(format!(
                "port {port} does not exist on {}",
                label(self.chip)
            ));
        }
        let offset = usize::from(address & 3);
        let channel = usize::from(port) * 3 + offset;
        match address {
            0x22 if port == 0 && self.stereo() => self.lfo = data & 0x0f,
            0x28 if port == 0 => {
                let code = usize::from(data & 7);
                let index = match code {
                    0..=2 => code,
                    4..=6 if self.stereo() => code - 1,
                    _ => return Err(format!("key-on channel code {code} is invalid")),
                };
                let ch = &mut self.channels[index];
                let kc = ch.keycode();
                for (named, slot) in NAMED_SLOT.iter().enumerate() {
                    if data & (0x10 << named) != 0 {
                        ch.slots[*slot].key_on(kc);
                    } else {
                        ch.slots[*slot].key_off();
                    }
                }
            }
            0x30..=0x9f if offset != 3 => {
                let group = usize::from((address >> 4) - 3);
                let slot = usize::from((address >> 2) & 3);
                let mut mask = OPERATOR_MASKS[group];
                if group == 3 && !self.stereo() {
                    mask = 0x1f;
                }
                self.channels[channel].slots[slot].regs[group] = data & mask;
            }
            0xa4..=0xa6 => self.frequency_latch = data & 0x3f,
            0xa0..=0xa2 => {
                let ch = &mut self.channels[channel];
                ch.fnum = (u16::from(self.frequency_latch & 7) << 8) | u16::from(data);
                ch.block = self.frequency_latch >> 3;
            }
            0xb0..=0xb2 => self.channels[channel].algorithm_feedback = data & 0x3f,
            0xb4..=0xb6 if self.stereo() => {
                self.channels[channel].output_control = data & 0xf7;
            }
            _ => {
                return Err(format!(
                    "register {address:#04x} on port {port} is not modelled"
                ));
            }
        }
        Ok(())
    }
    /// Advances one native sample and returns the summed left/right outputs.
    pub fn sample(&mut self) -> [i32; 2] {
        let lfo_enabled = self.lfo & 8 != 0;
        if lfo_enabled {
            self.lfo_timer += 1;
            if self.lfo_timer >= LFO_PERIODS[usize::from(self.lfo & 7)] {
                self.lfo_timer = 0;
                self.lfo_counter = (self.lfo_counter + 1) & 127;
            }
        } else {
            self.lfo_timer = 0;
            self.lfo_counter = 0;
        }
        self.envelope_timer += 1;
        if self.envelope_timer == 3 {
            self.envelope_timer = 0;
            self.envelope_counter = self.envelope_counter.wrapping_add(1);
            for ch in &mut self.channels {
                let kc = ch.keycode();
                for op in &mut ch.slots {
                    op.clock_envelope(kc, self.envelope_counter);
                }
            }
        }
        let c = self.lfo_counter;
        let am = (if c & 64 != 0 { 63 - (c & 63) } else { c & 63 }) * 2;
        let step = (c >> 2) as i32;
        let q = step & 7;
        let pm = match step >> 3 {
            0 => q,
            1 => 7 - q,
            2 => -q,
            _ => q - 7,
        };
        let stereo = self.stereo();
        let mut out = [0i32; 2];
        for ch in &mut self.channels {
            let pms = usize::from(ch.output_control & 7);
            let ratio = if pms == 0 || pm == 0 {
                1.0
            } else {
                (PM_DEPTH_CENTS[pms] * f64::from(pm) / 7.0 / 1200.0).exp2()
            };
            let value = ch.render(am, ratio);
            if !stereo || ch.output_control & 0x80 != 0 {
                out[0] += value;
            }
            if !stereo || ch.output_control & 0x40 != 0 {
                out[1] += value;
            }
        }
        out
    }
    pub fn silent(&self) -> bool {
        self.channels.iter().all(Channel::carriers_silent)
    }
}

/// Native FM sample clock: master clock / (prescaler × operator slots).
pub fn native_timing(chip: OpnChip, clock_hz: u32, prescaler: u8) -> Result<(u32, f64), String> {
    let slots = match chip {
        OpnChip::Opn => return Err("declare ym2203, ym2608 or ym2612".into()),
        OpnChip::Ym2203 => 12,
        OpnChip::Ym2608 | OpnChip::Ym2612 => 24,
    };
    let allowed: &[u8] = if chip == OpnChip::Ym2612 {
        &[6]
    } else {
        &[2, 3, 6]
    };
    if !allowed.contains(&prescaler) {
        return Err(format!(
            "prescaler {prescaler} is not available on {}; choose {allowed:?}",
            label(chip)
        ));
    }
    if !(1_000_000..=16_000_000).contains(&clock_hz) {
        return Err("clock_hz must be 1000000..16000000".into());
    }
    let divider = u32::from(prescaler) * slots;
    Ok((divider, f64::from(clock_hz) / f64::from(divider)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpnPan {
    Center,
    Left,
    Right,
    /// Both outputs disabled (B4h bits 7..6 clear).
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SsgEgPolicy {
    /// Refuse patches whose SSG-EG enable bit is set (default).
    #[default]
    Refuse,
    /// Render without SSG-EG; the report marks the audition as differing.
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpnLfo {
    pub enabled: bool,
    /// Register 22h rate 0..7.
    pub frequency: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpnAuditionContext {
    /// Chip model. Required for explicit registers or a family-only `opn`
    /// bank; must equal a bank's declared model.
    #[serde(default)]
    pub chip: Option<OpnChip>,
    /// Master clock into the chip in Hz, declared rather than inferred, such
    /// as 7670454 (YM2612 in an NTSC Mega Drive) or 7987200 (YM2608).
    pub clock_hz: u32,
    /// FM prescaler divisor 6, 3 or 2 (YM2203/YM2608). Default 6, the
    /// power-on setting; YM2612 has only 6.
    #[serde(default)]
    pub prescaler: Option<u8>,
    /// Global LFO (register 22h). Default disabled; YM2203 has none.
    #[serde(default)]
    pub lfo: Option<OpnLfo>,
    /// Channel output (B4h bits 7..6). YM2203 is mono: declare center.
    pub pan: OpnPan,
    /// Channel AM sensitivity 0..3 (B4h). Default 0.
    #[serde(default)]
    pub ams: Option<u8>,
    /// Channel PM sensitivity 0..7 (B4h). Default 0.
    #[serde(default)]
    pub pms: Option<u8>,
    /// Named operators 1..4 keyed on through register 28h. Default all.
    #[serde(default)]
    pub key_on_operators: Option<Vec<u8>>,
    /// SSG-EG is not modelled. Default refuse.
    #[serde(default)]
    pub ssg_eg: Option<SsgEgPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpnPitch {
    /// Equal temperament from an explicit A4 frequency at the native rate.
    /// Without `block`, the smallest block whose rounded F-number fits 11 bits.
    MidiNote {
        note: u8,
        a4_hz: f64,
        #[serde(default)]
        block: Option<u8>,
    },
    /// Exact A0h/A4h values.
    Register { fnum: u16, block: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpnNote {
    /// FM channel 0..2 (YM2203) or 0..5. Default 0.
    #[serde(default)]
    pub channel: u8,
    pub start_seconds: f64,
    /// Time from key-on to key-off.
    pub key_on_seconds: f64,
    pub pitch: OpnPitch,
    /// Explicit velocity policy: added to every carrier's TL in 0.75 dB
    /// steps, clamped at 127. Default 0.
    #[serde(default)]
    pub carrier_tl_offset: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RegisterWrite {
    pub sample: u64,
    pub port: u8,
    pub address: u8,
    pub data: u8,
}

/// A rendered audition: native integer frames plus the exact report.
#[derive(Debug, Clone)]
pub struct Audition {
    pub native_rate: f64,
    pub frames: Vec<[i16; 2]>,
    pub report: Value,
}

/// Resolves a pitch to F-number/block at the native rate.
pub fn resolve_pitch(pitch: &OpnPitch, native_rate: f64) -> Result<(u16, u8, Value), String> {
    let nominal = |fnum: u16, block: u8| {
        f64::from(fnum) * f64::from(1u32 << block) / 2.0 * native_rate / f64::from(1u32 << 20)
    };
    match *pitch {
        OpnPitch::Register { fnum, block } => {
            if fnum > 2047 || block > 7 {
                return Err("fnum must be 0..2047 and block 0..7".into());
            }
            Ok((
                fnum,
                block,
                json!({"source":"register","nominal_hz":nominal(fnum, block)}),
            ))
        }
        OpnPitch::MidiNote { note, a4_hz, block } => {
            if note > 127 || !(a4_hz.is_finite() && (220.0..=880.0).contains(&a4_hz)) {
                return Err("midi note must be 0..127 and a4_hz 220..880".into());
            }
            let target = a4_hz * ((f64::from(note) - 69.0) / 12.0).exp2();
            let fnum_for =
                |b: u8| (target * f64::from(1u32 << (21 - u32::from(b))) / native_rate).round();
            let block = match block {
                Some(b) if b > 7 => return Err("block must be 0..7".into()),
                Some(b) => b,
                None => (0..=7)
                    .find(|&b| fnum_for(b) <= 2047.0)
                    .ok_or("note is above the F-number range at this clock")?,
            };
            let fnum = fnum_for(block);
            if !(1.0..=2047.0).contains(&fnum) {
                return Err(format!(
                    "note {note} needs F-number {fnum} at block {block}; choose another block"
                ));
            }
            let fnum = fnum as u16;
            let actual = nominal(fnum, block);
            Ok((
                fnum,
                block,
                json!({"source":"midi_note","note":note,"a4_hz":a4_hz,
                "block_policy":if matches!(pitch, OpnPitch::MidiNote{block:Some(_),..}) {"declared"} else {"smallest_block_fitting_11_bit_fnum"},
                "target_hz":target,"nominal_hz":actual,"cents_error":1200.0*(actual/target).log2()}),
            ))
        }
    }
}

/// One keyed note at native samples with its resolved pitch.
#[derive(Debug, Clone)]
pub struct NotePlan {
    pub channel: usize,
    pub start: u64,
    /// Native sample of the key-off write; `u64::MAX` keeps the key held.
    pub stop: u64,
    pub fnum: u16,
    pub block: u8,
    /// Carrier TL offset of this note.
    pub offset: u8,
    pub pitch: Value,
}

/// A declared context checked against one patch, with the defaults it applied.
#[derive(Debug, Clone)]
pub struct OpnSetup {
    pub chip: OpnChip,
    pub clock_hz: u32,
    pub prescaler: u8,
    pub divider: u32,
    pub native_rate: f64,
    stereo: bool,
    lfo: OpnLfo,
    pan: OpnPan,
    ams: u8,
    pms: u8,
    key_operators: Vec<u8>,
    key_mask: u8,
    ssg: SsgEgPolicy,
    ssg_used: bool,
    defaults: Vec<&'static str>,
}

/// Checks the declared context and the patch bits for `chip`, applying and
/// recording defaults. Notes are not checked here.
pub fn prepare(
    patch: &FmPatch,
    chip: OpnChip,
    context: &OpnAuditionContext,
) -> Result<OpnSetup, String> {
    let mut defaults = Vec::new();
    let prescaler = context.prescaler.unwrap_or_else(|| {
        defaults.push("prescaler=6");
        6
    });
    let (divider, native_rate) = native_timing(chip, context.clock_hz, prescaler)?;
    let stereo = chip != OpnChip::Ym2203;
    let lfo = context.lfo.unwrap_or_else(|| {
        defaults.push("lfo=disabled");
        OpnLfo {
            enabled: false,
            frequency: 0,
        }
    });
    let ams = context.ams.unwrap_or_else(|| {
        defaults.push("ams=0");
        0
    });
    let pms = context.pms.unwrap_or_else(|| {
        defaults.push("pms=0");
        0
    });
    let key_operators = context.key_on_operators.clone().unwrap_or_else(|| {
        defaults.push("key_on_operators=[1,2,3,4]");
        vec![1, 2, 3, 4]
    });
    let ssg = context.ssg_eg.unwrap_or_else(|| {
        defaults.push("ssg_eg=refuse");
        SsgEgPolicy::Refuse
    });
    if lfo.frequency > 7 || ams > 3 || pms > 7 {
        return Err("lfo frequency must be 0..7, ams 0..3 and pms 0..7".into());
    }
    if !stereo && (lfo.enabled || ams != 0 || pms != 0 || context.pan != OpnPan::Center) {
        return Err("YM2203 has no LFO, AMS/PMS or pan; declare a disabled LFO, zero sensitivities and center".into());
    }
    let mut key_mask = 0u8;
    for &n in &key_operators {
        if !(1..=4).contains(&n) || key_mask & (0x10 << (n - 1)) != 0 {
            return Err("key_on_operators lists distinct named operators 1..4".into());
        }
        key_mask |= 0x10 << (n - 1);
    }
    if key_mask == 0 {
        return Err("key at least one operator".into());
    }
    if patch.algorithm_feedback & !0x3f != 0
        || patch.operators.iter().any(|op| {
            op.iter()
                .zip(OPERATOR_MASKS)
                .enumerate()
                .any(|(group, (&v, mask))| {
                    let mask = if group == 3 && !stereo { 0x1f } else { mask };
                    v & !mask != 0
                })
        })
    {
        return Err(
            "patch sets bits outside the interpreted OPN register fields for this chip".into(),
        );
    }
    let ssg_used = patch.operators.iter().any(|op| op[6] & 8 != 0);
    if ssg_used && ssg == SsgEgPolicy::Refuse {
        return Err("patch enables SSG-EG, which this model does not render; declare ssg_eg=ignore to audition without it".into());
    }
    Ok(OpnSetup {
        chip,
        clock_hz: context.clock_hz,
        prescaler,
        divider,
        native_rate,
        stereo,
        lfo,
        pan: context.pan,
        ams,
        pms,
        key_operators,
        key_mask,
        ssg,
        ssg_used,
        defaults,
    })
}

impl OpnSetup {
    /// The context values and the defaults applied, for reports.
    pub fn context_report(&self) -> Value {
        let lfo = self.lfo;
        json!({
            "lfo": {"enabled": lfo.enabled && self.stereo, "frequency": lfo.frequency,
                "samples_per_step": LFO_PERIODS[usize::from(lfo.frequency)],
                "nominal_hz_at_8mhz_class_clock": LFO_NOMINAL_HZ[usize::from(lfo.frequency)],
                "rate_hz_at_this_clock": self.native_rate / f64::from(LFO_PERIODS[usize::from(lfo.frequency)]) / 128.0},
            "pan": self.pan, "ams": self.ams, "pms": self.pms,
            "key_on_operators": self.key_operators,
            "ssg_eg": self.ssg, "ssg_eg_present": self.ssg_used,
            "defaults_applied": self.defaults,
        })
    }

    pub fn ssg_used(&self) -> bool {
        self.ssg_used
    }
}

/// The register writes that set up the plans' channels and key their notes,
/// in application order: setup at sample 0, then per note any changed
/// carrier TL, A4h/A0h and the 28h key-on at its start, and the 28h key-off at
/// its stop. `order` lists the plans by channel and start. Also returns each
/// note's effective carrier TL values, in `order`.
pub fn register_writes(
    patch: &FmPatch,
    setup: &OpnSetup,
    plans: &[NotePlan],
    order: &[usize],
) -> (Vec<RegisterWrite>, Vec<Vec<Value>>) {
    let stereo = setup.stereo;
    let algorithm = usize::from(patch.algorithm_feedback & 7);
    let carriers: Vec<usize> = CARRIERS[algorithm].iter().map(|&n| NAMED_SLOT[n]).collect();
    let address = |base: u8, ch: usize| (if ch >= 3 { 1u8 } else { 0 }, base + (ch % 3) as u8);
    let mut writes: Vec<(u64, u8, RegisterWrite)> = Vec::new();
    let mut push = |sample: u64, priority: u8, (port, address): (u8, u8), data: u8| {
        writes.push((
            sample,
            priority,
            RegisterWrite {
                sample,
                port,
                address,
                data,
            },
        ));
    };
    if stereo {
        push(
            0,
            0,
            (0, 0x22),
            (u8::from(setup.lfo.enabled) << 3) | setup.lfo.frequency,
        );
    }
    let output_control = match setup.pan {
        OpnPan::Center => 0xc0,
        OpnPan::Left => 0x80,
        OpnPan::Right => 0x40,
        OpnPan::Off => 0,
    } | (setup.ams << 4)
        | setup.pms;
    let mut used: Vec<usize> = plans.iter().map(|p| p.channel).collect();
    used.sort_unstable();
    used.dedup();
    for &ch in &used {
        for group in 0..7u8 {
            for slot in 0..4u8 {
                let data = patch.operators[usize::from(slot)][usize::from(group)];
                push(0, 0, address(0x30 + group * 0x10 + slot * 4, ch), data);
            }
        }
        push(0, 0, address(0xb0, ch), patch.algorithm_feedback);
        if stereo {
            push(0, 0, address(0xb4, ch), output_control);
        }
    }
    let mut tl_report = Vec::new();
    let mut current_tl: std::collections::BTreeMap<(usize, usize), u8> =
        std::collections::BTreeMap::new();
    let code = |ch: usize| if ch >= 3 { ch as u8 + 1 } else { ch as u8 };
    for &i in order {
        let p = &plans[i];
        push(p.stop, 0, (0, 0x28), code(p.channel));
        let mut effective = Vec::new();
        for &slot in &carriers {
            let base = patch.operators[slot][1];
            let tl = (u16::from(base) + u16::from(p.offset)).min(127) as u8;
            effective.push(json!({"named_operator":SLOT_NAMED[slot],"base_tl":base,"tl":tl,"clamped":u16::from(base) + u16::from(p.offset) > 127}));
            if current_tl.get(&(p.channel, slot)).copied().unwrap_or(base) != tl {
                push(p.start, 1, address(0x40 + slot as u8 * 4, p.channel), tl);
            }
            current_tl.insert((p.channel, slot), tl);
        }
        push(
            p.start,
            2,
            address(0xa4, p.channel),
            (p.block << 3) | (p.fnum >> 8) as u8,
        );
        push(p.start, 3, address(0xa0, p.channel), (p.fnum & 0xff) as u8);
        push(p.start, 4, (0, 0x28), setup.key_mask | code(p.channel));
        tl_report.push(effective);
    }
    writes.sort_by_key(|w| (w.0, w.1));
    (writes.into_iter().map(|w| w.2).collect(), tl_report)
}

/// One output frame: the summed 14-bit channel outputs clamped to
/// -8192..8191 and shifted into signed 16-bit PCM. Tracks peak and clipping.
pub fn pcm_frame(out: [i32; 2], peak: &mut i32, clipped: &mut u64) -> [i16; 2] {
    let mut frame = [0i16; 2];
    for (side, value) in out.into_iter().enumerate() {
        *peak = (*peak).max(value.abs());
        if !(-8192..=8191).contains(&value) {
            *clipped += 1;
        }
        frame[side] = (value.clamp(-8192, 8191) << 2) as i16;
    }
    frame
}

/// Render notes for one patch with fully declared context.
pub fn audition(
    patch: &FmPatch,
    chip: OpnChip,
    context: &OpnAuditionContext,
    notes: &[OpnNote],
    tail_seconds: f64,
) -> Result<Audition, String> {
    let setup = prepare(patch, chip, context)?;
    let native_rate = setup.native_rate;
    if notes.is_empty() || notes.len() > MAX_NOTES {
        return Err(format!("declare 1..{MAX_NOTES} notes"));
    }
    if !(tail_seconds.is_finite() && (0.0..=MAX_TAIL_SECONDS).contains(&tail_seconds)) {
        return Err(format!("tail_seconds must be 0..{MAX_TAIL_SECONDS}"));
    }
    let to_samples = |s: f64| (s * native_rate).round() as u64;
    let mut plans = Vec::new();
    for (i, note) in notes.iter().enumerate() {
        let end = note.start_seconds + note.key_on_seconds;
        if !(note.start_seconds.is_finite() && note.start_seconds >= 0.0)
            || !(note.key_on_seconds.is_finite() && note.key_on_seconds > 0.0)
            || end > MAX_NOTE_SECONDS
        {
            return Err(format!(
                "note {i}: start >= 0, key_on_seconds > 0 and key-off by {MAX_NOTE_SECONDS} s"
            ));
        }
        if usize::from(note.channel) >= channel_count(chip) {
            return Err(format!(
                "note {i}: channel {} does not exist on {}",
                note.channel,
                label(chip)
            ));
        }
        let (fnum, block, pitch) =
            resolve_pitch(&note.pitch, native_rate).map_err(|e| format!("note {i}: {e}"))?;
        let (start, stop) = (to_samples(note.start_seconds), to_samples(end));
        if stop <= start {
            return Err(format!(
                "note {i}: key-on is shorter than one native sample"
            ));
        }
        plans.push(NotePlan {
            channel: note.channel.into(),
            start,
            stop,
            fnum,
            block,
            offset: note.carrier_tl_offset,
            pitch,
        });
    }
    let mut order: Vec<usize> = (0..plans.len()).collect();
    order.sort_by_key(|&i| (plans[i].channel, plans[i].start));
    for pair in order.windows(2) {
        let (a, b) = (&plans[pair[0]], &plans[pair[1]]);
        if a.channel == b.channel && b.start < a.stop {
            return Err(format!(
                "notes {} and {} overlap on channel {}; use another channel",
                pair[0], pair[1], a.channel
            ));
        }
    }
    let (writes, tl_report) = register_writes(patch, &setup, &plans, &order);

    let mut model = OpnModel::new(chip)?;
    let last_stop = plans.iter().map(|p| p.stop).max().expect("nonempty");
    let limit = last_stop + to_samples(tail_seconds);
    let mut frames = Vec::with_capacity(limit as usize);
    let (mut next, mut clipped, mut peak) = (0, 0u64, 0i32);
    let mut n = 0u64;
    let mut ended_silent = false;
    while n < limit {
        while next < writes.len() && writes[next].sample == n {
            let w = writes[next];
            model.write(w.port, w.address, w.data)?;
            next += 1;
        }
        if n >= last_stop && model.silent() {
            ended_silent = true;
            break;
        }
        frames.push(pcm_frame(model.sample(), &mut peak, &mut clipped));
        n += 1;
    }
    if !ended_silent && model.silent() {
        ended_silent = true;
    }

    let header_rate = native_rate.round() as u32;
    let note_report: Vec<_> = order
        .iter()
        .zip(&tl_report)
        .map(|(&i, tl)| {
            let p = &plans[i];
            json!({"note":i,"channel":p.channel,"key_on_sample":p.start,"key_off_sample":p.stop,
                "fnum":p.fnum,"block":p.block,"keycode":keycode(p.fnum,p.block),"pitch":p.pitch,
                "carrier_tl_offset":p.offset,"carrier_tl":tl})
        })
        .collect();
    let report = json!({
        "chip": chip,
        "clock_hz": context.clock_hz,
        "prescaler": setup.prescaler,
        "divider": setup.divider,
        "native_rate_hz": native_rate,
        "native_rate_exact": format!("{}/{}", context.clock_hz, setup.divider),
        "wav_header_rate_hz": header_rate,
        "wav_header_pitch_error_cents": 1200.0 * (f64::from(header_rate) / native_rate).log2(),
        "context": setup.context_report(),
        "patch": patch,
        "patch_sha256": patch.digest(),
        "decoded": patch.decoded(),
        "notes": note_report,
        "register_writes": writes,
        "frames": frames.len(),
        "ended_silent": ended_silent,
        "peak_14bit": peak,
        "clipped_samples": clipped,
        "output_scale": "summed 14-bit channel outputs clamped to -8192..8191, shifted left 2 into signed 16-bit PCM",
        "model": model_scope(setup.ssg_used),
    });
    Ok(Audition {
        native_rate,
        frames,
        report,
    })
}

pub fn model_scope(ssg_ignored: bool) -> Value {
    json!({
        "renderer": "Tunemend OPN FM model; not original hardware audio or driver playback",
        "modelled": ["F-number/block phase generator with detune and multiple", "eight algorithms with operator-1 feedback and one-sample delayed paths",
            "attack/decay/sustain/release envelope with key scaling, hardware rate counter and increment tables, sustain level and total level",
            "log-sin and exponent table operator output", "LFO AM triangle with AMS depths", "B4h left/right output"],
        "approximated": ["LFO PM: documented PMS depth in cents applied to the phase increment, not the hardware F-number bit table"],
        "not_modelled": ["SSG-EG", "DAC quantization and YM2612 ladder effect", "output filtering", "CSM and channel-3 special mode", "timers", "register write busy timing (writes apply at native sample boundaries)", "SSG, ADPCM, rhythm and DAC parts", "driver macros and later score commands"],
        "ssg_eg_ignored": ssg_ignored,
        "equality": "Equal registers and context do not establish that a whole original instrument or song sounds identical."
    })
}

/// 16-bit stereo PCM WAV.
pub fn wav16(sample_rate: u32, frames: &[[i16; 2]]) -> Vec<u8> {
    let data_length = frames.len() as u32 * 4;
    let mut out = Vec::with_capacity(44 + data_length as usize);
    out.extend(b"RIFF");
    out.extend((36 + data_length).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(sample_rate.to_le_bytes());
    out.extend((sample_rate * 4).to_le_bytes());
    out.extend(4u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_length.to_le_bytes());
    for frame in frames {
        for sample in frame {
            out.extend(sample.to_le_bytes());
        }
    }
    out
}

pub const RESAMPLER_HALF_WIDTH: usize = 32;

/// Band-limited resampling with a Blackman-windowed sinc whose cutoff is
/// 0.45 of the lower rate. Returns frames and the clipped sample count.
pub fn resample(frames: &[[i16; 2]], input_rate: f64, output_rate: u32) -> (Vec<[i16; 2]>, u64) {
    let ratio = input_rate / f64::from(output_rate);
    let cutoff = 0.45 * f64::from(output_rate).min(input_rate) / input_rate * 2.0;
    let length = (frames.len() as f64 / ratio).floor() as usize;
    let half = RESAMPLER_HALF_WIDTH as f64 / cutoff.min(1.0);
    let mut clipped = 0;
    let out = (0..length)
        .map(|n| {
            let t = n as f64 * ratio;
            let first = (t - half).ceil().max(0.0) as usize;
            let last = ((t + half).floor() as usize).min(frames.len().saturating_sub(1));
            let mut sum = [0f64; 2];
            for (k, frame) in frames.iter().enumerate().take(last + 1).skip(first) {
                let x = k as f64 - t;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    let a = std::f64::consts::PI * cutoff * x;
                    a.sin() / a
                };
                let w = 0.5 + x / (2.0 * half);
                let window = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * w).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * w).cos();
                let gain = cutoff * sinc * window;
                sum[0] += f64::from(frame[0]) * gain;
                sum[1] += f64::from(frame[1]) * gain;
            }
            sum.map(|v| {
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

#[cfg(test)]
mod tests {
    use super::*;

    const RATE_CLOCK: u32 = 7_670_454;

    fn rate() -> f64 {
        native_timing(OpnChip::Ym2612, RATE_CLOCK, 6).unwrap().1
    }
    /// Algorithm `algorithm`; only named operator `audible` (1..4) has TL 0.
    fn patch(algorithm: u8, feedback: u8, audible: usize) -> FmPatch {
        let mut operators = [[0x01, 0x7f, 0x1f, 0x00, 0x00, 0x0f, 0x00]; 4];
        operators[NAMED_SLOT[audible - 1]][1] = 0;
        FmPatch {
            algorithm_feedback: (feedback << 3) | algorithm,
            operators,
        }
    }
    fn context(chip: OpnChip) -> OpnAuditionContext {
        OpnAuditionContext {
            chip: Some(chip),
            clock_hz: RATE_CLOCK,
            prescaler: None,
            lfo: None,
            pan: OpnPan::Center,
            ams: None,
            pms: None,
            key_on_operators: None,
            ssg_eg: None,
        }
    }
    fn note(fnum: u16, block: u8, seconds: f64) -> OpnNote {
        OpnNote {
            channel: 0,
            start_seconds: 0.0,
            key_on_seconds: seconds,
            pitch: OpnPitch::Register { fnum, block },
            carrier_tl_offset: 0,
        }
    }
    fn render(patch: &FmPatch, ctx: &OpnAuditionContext, notes: &[OpnNote]) -> Vec<f64> {
        let a = audition(patch, OpnChip::Ym2612, ctx, notes, 0.0).unwrap();
        a.frames.iter().map(|f| f64::from(f[0])).collect()
    }
    fn dft_power(x: &[f64], bin: f64) -> f64 {
        let n = x.len() as f64;
        let (mut re, mut im) = (0.0, 0.0);
        for (k, v) in x.iter().enumerate() {
            let a = 2.0 * std::f64::consts::PI * bin * k as f64 / n;
            re += v * a.cos();
            im -= v * a.sin();
        }
        re * re + im * im
    }
    fn rms(x: &[f64]) -> f64 {
        (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn tables_are_platform_stable_and_match_formulas() {
        // Every entry rounds far from .5, so libm differences cannot flip a value.
        for i in 0..256 {
            let x = ((i as f64 + 0.5) * std::f64::consts::PI / 512.0).sin();
            let v = -x.log2() * 256.0;
            assert!((v - v.floor() - 0.5).abs() > 1e-6, "logsin {i}");
            let e = ((255 - i) as f64 / 256.0).exp2() * 1024.0 - 1024.0;
            assert!((e - e.floor() - 0.5).abs() > 1e-6, "exp {i}");
        }
        assert_eq!(logsin()[0], 2137);
        assert_eq!(logsin()[255], 0);
        assert_eq!(exponent()[0], 1018);
        assert_eq!(exponent()[255], 0);
        assert_eq!(keycode(1082, 4), 18);
        assert_eq!(keycode(0x780, 2), 11);
        assert_eq!(keycode(0x380, 2), 9);
        assert_eq!(keycode(0x300, 2), 8);
    }

    #[test]
    fn carrier_frequency_follows_fnum_block_multiple_and_detune() {
        let n = 8192usize;
        for (mul, dt, fnum, block) in [
            (1u8, 0u8, 1082u16, 4u8),
            (2, 0, 700, 3),
            (0, 0, 1500, 5),
            (1, 3, 1082, 4),
            (1, 7, 1082, 4),
        ] {
            let mut p = patch(7, 0, 4);
            p.operators[3][0] = (dt << 4) | mul;
            let x = render(&p, &context(OpnChip::Ym2612), &[note(fnum, block, 0.5)]);
            let x = &x[2000..2000 + n];
            let increment = phase_increment(fnum, block, dt, mul);
            let expected_hz = f64::from(increment) * rate() / f64::from(1u32 << 20);
            let expected_bin = expected_hz * n as f64 / rate();
            let centre = expected_bin.round() as i64;
            let peak = (centre - 20..=centre + 20)
                .max_by(|&a, &b| dft_power(x, a as f64).total_cmp(&dft_power(x, b as f64)))
                .unwrap() as f64;
            assert!(
                (peak - expected_bin).abs() <= 1.0,
                "mul {mul} dt {dt}: {peak} vs {expected_bin}"
            );
            // Parseval: the bins around the expected frequency hold nearly all energy.
            let total = n as f64 * x.iter().map(|v| v * v).sum::<f64>();
            let near: f64 = (centre - 10..=centre + 10)
                .map(|b| dft_power(x, b as f64))
                .sum::<f64>()
                * 2.0;
            assert!(near / total > 0.95, "{}", near / total);
            if dt == 0 {
                let multiple = if mul == 0 { 0.5 } else { f64::from(mul) };
                let formula = f64::from(fnum) * f64::from(1u32 << block) / 2.0 * rate()
                    / f64::from(1u32 << 20)
                    * multiple;
                assert!((formula - expected_hz).abs() / formula < 2e-3);
            }
        }
        // Detune moves the increment by the table entry for key code 18.
        assert_eq!(
            phase_increment(1082, 4, 3, 1) - phase_increment(1082, 4, 0, 1),
            9
        );
        assert_eq!(
            phase_increment(1082, 4, 0, 1) - phase_increment(1082, 4, 7, 1),
            9
        );
    }

    #[test]
    fn total_level_steps_are_three_quarter_decibels() {
        let level = |tl: u8| {
            let mut p = patch(7, 0, 4);
            p.operators[3][1] = tl;
            rms(&render(&p, &context(OpnChip::Ym2612), &[note(1082, 4, 0.3)])[4000..12000])
        };
        let reference = level(0);
        let one = 20.0 * (level(1) / reference).log10();
        let eight = level(8) / reference;
        let forty = 20.0 * (level(40) / reference).log10();
        assert!((one + 0.75).abs() < 0.05, "{one}");
        assert!((eight - 0.5).abs() < 0.01, "{eight}");
        assert!((forty + 30.1).abs() < 0.3, "{forty}");
        // TL 0 peaks near the 14-bit limit (shifted into 16-bit PCM).
        assert!(reference > 0.97 * 32760.0 / 2f64.sqrt(), "{reference}");
    }

    fn envelope_trace(regs: impl Fn(&mut [u8; 7]), key_on: f64) -> Vec<u32> {
        let mut model = OpnModel::new(OpnChip::Ym2612).unwrap();
        let mut p = patch(7, 0, 4);
        regs(&mut p.operators[3]);
        for group in 0..7u8 {
            for slot in 0..4u8 {
                let data = p.operators[usize::from(slot)][usize::from(group)];
                model.write(0, 0x30 + group * 16 + slot * 4, data).unwrap();
            }
        }
        model.write(0, 0xb0, 7).unwrap();
        model.write(0, 0xb4, 0xc0).unwrap();
        // Block 4, F-number 1082: key code 18, so KS 0 adds 18 >> 3 = 2.
        model.write(0, 0xa4, (4 << 3) | 4).unwrap();
        model.write(0, 0xa0, 0x3a).unwrap();
        model.write(0, 0x28, 0x80).unwrap();
        let stop = (key_on * 53267.0) as usize;
        (0..stop * 2)
            .map(|n| {
                if n == stop {
                    model.write(0, 0x28, 0).unwrap();
                }
                model.sample();
                model.channels[0].slots[3].attenuation
            })
            .collect()
    }
    /// Independent expectation: 1023 steps at the table's mean increment,
    /// one update per 2^shift envelope clocks, one clock per 3 samples.
    fn expected_samples(rate: u32) -> f64 {
        let shift = 11u32.saturating_sub(rate >> 2);
        let per8: u32 = (0..8).map(|s| envelope_increment(rate, s << shift)).sum();
        1023.0 / (f64::from(per8) / 8.0) * f64::from(1u32 << shift) * 3.0
    }

    #[test]
    fn decay_and_release_timing_follow_the_rate_tables() {
        let decay = |dr: u8| {
            let trace = envelope_trace(
                |r| {
                    r[2] = 0x1f;
                    r[3] = dr;
                    r[4] = dr;
                    r[5] = 0xf0;
                },
                4.0,
            );
            trace.iter().position(|&a| a >= 1023).unwrap() as f64
        };
        for dr in [12u8, 13, 14, 16, 18] {
            let rate = u32::from(dr) * 2 + 2;
            let ratio = decay(dr) / expected_samples(rate);
            assert!((ratio - 1.0).abs() < 0.02, "dr {dr}: {ratio}");
        }
        // Four rate steps (two DR steps) halve the time.
        let ratio = decay(12) / decay(14);
        assert!((ratio - 2.0).abs() < 0.05, "{ratio}");

        // Key-off releases at rate 4 * RR + 2 plus key scale.
        let release = envelope_trace(
            |r| {
                r[2] = 0x1f;
                r[5] = 0x08;
            },
            0.6,
        );
        let stop = (0.6 * 53267.0) as usize;
        assert_eq!(release[stop - 1], 0);
        let gone = release[stop..].iter().position(|&a| a >= 1023).unwrap() as f64;
        assert!((gone / expected_samples(36) - 1.0).abs() < 0.03, "{gone}");

        // The release bound covers it: rate 34 without key scaling, plus
        // one update of waiting, for the slowest carrier.
        let mut p = patch(7, 0, 4);
        p.operators[3][5] = 0x08;
        let bound = release_bound_samples(&p) as f64;
        assert!(bound >= gone, "{bound} {gone}");
        assert!(
            bound <= 1.02 * expected_samples(34) + 3.0 * 256.0,
            "{bound}"
        );
        // Only carriers count: under algorithm 0 operator 4 alone sounds.
        p.algorithm_feedback = 0;
        p.operators[3][5] = 0x0f;
        p.operators[0][5] = 0x00;
        assert_eq!(
            release_bound_samples(&p),
            release_bound_samples(&patch(0, 0, 4))
        );
    }

    #[test]
    fn attack_is_exponential_and_sustain_level_holds() {
        // Rate 26: attenuation falls by ~1/16 of itself per update.
        let attack = envelope_trace(|r| r[2] = 0x0c, 2.0);
        let done = attack.iter().position(|&a| a == 0).unwrap() as f64;
        let half = attack.iter().position(|&a| a <= 512).unwrap() as f64;
        assert!(half / done > 0.05 && half / done < 0.2, "{half} {done}");
        assert!(done > 5000.0);
        // Rate 62 or more completes the attack at key-on.
        assert_eq!(envelope_trace(|r| r[2] = 0x1f, 0.1)[0], 0);
        // SL 4 stops decay at 4 * 32 = 128 (12 dB) when the sustain rate is zero.
        let sustain = envelope_trace(
            |r| {
                r[2] = 0x1f;
                r[3] = 20;
                r[5] = 0x4f;
            },
            1.0,
        );
        let held = sustain[(0.9 * 53267.0) as usize];
        assert!((128..=130).contains(&held), "{held}");
    }

    #[test]
    fn feedback_adds_harmonics_monotonically() {
        let n = 8192usize;
        // F-number 1024, block 4: 128 samples per cycle, so harmonics land on bins.
        let f0 = 64.0;
        let mut previous = -1.0;
        for fb in 0..=6u8 {
            let x = render(
                &patch(7, fb, 1),
                &context(OpnChip::Ym2612),
                &[note(1024, 4, 0.5)],
            );
            let x = &x[4000..4000 + n];
            let fundamental = dft_power(x, f0);
            let harmonics: f64 = (2..=5).map(|h| dft_power(x, f0 * h as f64)).sum();
            let ratio = harmonics / fundamental;
            if fb == 0 {
                assert!(ratio < 1e-4, "{ratio}");
            }
            assert!(ratio > previous, "fb {fb}: {ratio} <= {previous}");
            previous = ratio;
        }
    }

    #[test]
    fn modulator_output_changes_the_carrier_spectrum_only_through_connections() {
        // Algorithm 0 with operator 1 audible-level but not a carrier: silent output.
        let mut p = patch(0, 0, 1);
        let silent = render(&p, &context(OpnChip::Ym2612), &[note(1024, 4, 0.2)]);
        assert!(silent.iter().all(|&v| v == 0.0));
        // Operator 3 -> 4 modulation at TL 0 spreads energy beyond the fundamental.
        p.operators[NAMED_SLOT[3]][1] = 0;
        p.operators[NAMED_SLOT[2]][1] = 0;
        let x = render(&p, &context(OpnChip::Ym2612), &[note(1024, 4, 0.5)]);
        let x = &x[4000..4000 + 8192];
        let fundamental = dft_power(x, 64.0);
        let total = 8192.0 * x.iter().map(|v| v * v).sum::<f64>();
        assert!(2.0 * fundamental / total < 0.5);
    }

    #[test]
    fn lfo_am_depths_match_documented_decibels() {
        for (ams, expected) in [(1u8, 1.41), (2, 5.93), (3, 11.85)] {
            let mut p = patch(7, 0, 4);
            p.operators[3][3] = 0x80;
            let mut ctx = context(OpnChip::Ym2612);
            ctx.lfo = Some(OpnLfo {
                enabled: true,
                frequency: 0,
            });
            ctx.ams = Some(ams);
            // 128 samples per carrier cycle: one-cycle RMS windows are phase independent.
            let x = render(&p, &ctx, &[note(1024, 4, 1.0)]);
            let levels: Vec<f64> = x[256..50_000]
                .as_chunks::<128>()
                .0
                .iter()
                .map(|c| rms(c))
                .collect();
            let max = levels.iter().cloned().fold(0.0, f64::max);
            let min = levels.iter().cloned().fold(f64::MAX, f64::min);
            let depth = 20.0 * (max / min).log10();
            assert!((depth - expected).abs() < 0.3, "ams {ams}: {depth}");
        }
    }

    #[test]
    fn lfo_periods_are_one_sample_shorter_than_the_datasheet_rates() {
        // Yamaha's rates at an 8 MHz-class clock (55.5 kHz FM rate) match one
        // sample more per step, within 1%; the model uses the measured periods.
        let native = 8_000_000.0 / 144.0;
        for (period, documented) in LFO_PERIODS.iter().zip(LFO_NOMINAL_HZ) {
            let hz = native / f64::from(*period + 1) / 128.0;
            assert!((hz / documented - 1.0).abs() < 0.01, "{hz} vs {documented}");
        }
        // The rendered AM triangle repeats every 128 steps of 8 samples (rate 6).
        let mut p = patch(7, 0, 4);
        p.operators[3][3] = 0x80;
        let mut ctx = context(OpnChip::Ym2612);
        ctx.lfo = Some(OpnLfo {
            enabled: true,
            frequency: 6,
        });
        ctx.ams = Some(3);
        let x = render(&p, &ctx, &[note(1024, 4, 0.5)]);
        let levels: Vec<f64> = x[..1024 * 12]
            .as_chunks::<128>()
            .0
            .iter()
            .map(|c| rms(c))
            .collect();
        let quietest = |range: std::ops::Range<usize>| {
            range
                .min_by(|&a, &b| levels[a].total_cmp(&levels[b]))
                .unwrap()
        };
        let first = quietest(0..8);
        assert_eq!(quietest(first + 4..first + 12), first + 8);
        assert_eq!(quietest(first + 44..first + 52), first + 48);
    }

    #[test]
    fn context_mismatches_refuse() {
        let p = patch(7, 0, 4);
        let n = [note(1082, 4, 0.1)];
        assert!(audition(&p, OpnChip::Opn, &context(OpnChip::Opn), &n, 0.0).is_err());
        let mut ctx = context(OpnChip::Ym2203);
        ctx.clock_hz = 3_993_600;
        ctx.lfo = Some(OpnLfo {
            enabled: true,
            frequency: 0,
        });
        let refused = audition(&p, OpnChip::Ym2203, &ctx, &n, 0.0).unwrap_err();
        assert!(refused.contains("YM2203"));
        ctx.lfo = None;
        assert!(audition(&p, OpnChip::Ym2203, &ctx, &n, 0.0).is_ok());
        let mut am = p.clone();
        am.operators[0][3] = 0x80;
        assert!(audition(&am, OpnChip::Ym2203, &ctx, &n, 0.0).is_err());
        let mut ssg = p.clone();
        ssg.operators[3][6] = 0x08;
        let ctx = context(OpnChip::Ym2612);
        assert!(
            audition(&ssg, OpnChip::Ym2612, &ctx, &n, 0.0)
                .unwrap_err()
                .contains("SSG-EG")
        );
        let mut ignore = context(OpnChip::Ym2612);
        ignore.ssg_eg = Some(SsgEgPolicy::Ignore);
        let a = audition(&ssg, OpnChip::Ym2612, &ignore, &n, 0.0).unwrap();
        assert_eq!(a.report["model"]["ssg_eg_ignored"], true);
        let mut prescaled = context(OpnChip::Ym2612);
        prescaled.prescaler = Some(3);
        assert!(audition(&p, OpnChip::Ym2612, &prescaled, &n, 0.0).is_err());
        let overlap = [
            note(1082, 4, 0.2),
            OpnNote {
                start_seconds: 0.1,
                ..note(900, 4, 0.2)
            },
        ];
        let refused = audition(&p, OpnChip::Ym2612, &ctx, &overlap, 0.0).unwrap_err();
        assert!(refused.contains("overlap"));
    }

    #[test]
    fn midi_pitch_uses_declared_tuning_and_reports_quantization() {
        let a4 = |block| OpnPitch::MidiNote {
            note: 69,
            a4_hz: 440.0,
            block,
        };
        let (fnum, block, report) = resolve_pitch(&a4(None), rate()).unwrap();
        assert_eq!((fnum, block), (1083, 4));
        assert!(report["cents_error"].as_f64().unwrap().abs() < 1.0);
        assert_eq!(resolve_pitch(&a4(Some(5)), rate()).unwrap().0, 541);
        assert!(resolve_pitch(&a4(Some(0)), rate()).is_err());
        let (_, opna) = native_timing(OpnChip::Ym2608, 7_987_200, 6).unwrap();
        let (_, opn) = native_timing(OpnChip::Ym2203, 3_993_600, 6).unwrap();
        assert_eq!(opna, opn);
        assert!((opna - 55_466.666).abs() < 0.01);
    }

    #[test]
    fn rendering_is_deterministic_and_resampling_is_separate() {
        let mut p = patch(4, 5, 4);
        p.operators[NAMED_SLOT[0]][1] = 30;
        let run = || {
            audition(
                &p,
                OpnChip::Ym2612,
                &context(OpnChip::Ym2612),
                &[note(1082, 4, 0.1)],
                0.2,
            )
            .unwrap()
        };
        let (a, b) = (run(), run());
        assert_eq!(a.frames, b.frames);
        assert_eq!(a.report, b.report);
        assert_eq!(a.report["ended_silent"], true);
        let (resampled, clipped) = resample(&a.frames, a.native_rate, 44_100);
        assert_eq!(clipped, 0);
        let expected = (a.frames.len() as f64 * 44_100.0 / a.native_rate).floor() as usize;
        assert_eq!(resampled.len(), expected);
        let tone =
            |frames: &[[i16; 2]]| rms(&frames.iter().map(|f| f64::from(f[0])).collect::<Vec<_>>());
        let scale = 44_100.0 / a.native_rate;
        let before = tone(&a.frames[2000..4000]);
        let after = tone(&resampled[(2000.0 * scale) as usize..(4000.0 * scale) as usize]);
        assert!((after / before - 1.0).abs() < 0.02, "{after} {before}");
    }
}
