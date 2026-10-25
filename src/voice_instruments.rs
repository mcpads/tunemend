//! Chip instruments assigned to voices. In renders, an assigned voice plays
//! its notes through Tunemend's S-DSP voice model or OPN FM model instead of
//! the reference synthesizer; the chip output is converted to the render rate
//! and enters the mixer like any voice.
//!
//! The assignment is mixer data of the revision: it changes what a render
//! sounds like, never the composition's MIDI events or program numbers.

use std::collections::{BTreeMap, HashSet, VecDeque};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::composition::Composition;
use crate::diagnostic::{Diagnostic, DiagnosticCode as Code, Report};
use crate::edit::EditOperation;
use crate::fm::{FmPatch, OpnChip};
use crate::mix::Tenths;
use crate::opn::{
    NotePlan, OpnAuditionContext, OpnModel, OpnPitch, OpnSetup, pcm_frame, register_writes,
    resolve_pitch,
};
use crate::performance::ChannelMessage;
use crate::snes_dsp::{
    self, ADSR1, ADSR2, AUDITION_DIR, AUDITION_START, DIR, Dsp, DspEnvelope, FLG, GAIN, KOFF, KON,
    MVOLL, MVOLR, NoteRoot, PITCHH, PITCHL, SRCN, StereoVolume, VOLL, VOLR,
};
use crate::timeline::Rational;

/// S-DSP voices one instrument render may use.
pub const SNES_VOICES: u8 = 8;
/// Notes whose chip-voice allocation a render report lists.
pub const FIRST_NOTES_LISTED: usize = 8;
/// Lowest and highest A4 reference for FM note mapping, in Hz.
pub const A4_RANGE_HZ: (f64, f64) = (220.0, 880.0);

/// The chip instrument that plays one voice in renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstrumentAssignment {
    pub voice: String,
    pub instrument: VoiceInstrument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VoiceInstrument {
    /// A BRR sample played by the S-DSP voice model at 32000 Hz.
    SnesBrr(SnesBrrInstrument),
    /// A static OPN patch played by the FM model at its native rate.
    Fm(FmInstrument),
}

/// A saved bank slot (`bank` with `slot`) or a sample plan with a BRR target
/// (`plan`), played as render_instrument plays it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnesBrrInstrument {
    /// Saved instrument bank; use with `slot`. Mutually exclusive with `plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u8>,
    /// Saved sample plan with a BRR target; its bytes and kept loop are played.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub pitch: SnesPitchMap,
    pub envelope: DspEnvelope,
    /// VxVOLL/VxVOLR, signed, before the velocity policy.
    pub volume: StereoVolume,
    pub velocity: SnesVelocity,
    /// MVOLL/MVOLR, signed. Default 127/127.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_volume: Option<StereoVolume>,
    /// Global envelope counter at render sample 0, 0..0x77FF. Default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counter_start: Option<u16>,
    /// S-DSP voices the instrument may use, 1..8. Default 8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_voices: Option<u8>,
    /// Whether the voice's CC7/CC11 (and CC10) lanes shape the chip output.
    /// Default `ignore`.
    #[serde(default, skip_serializing_if = "ChannelControls::is_ignore")]
    pub channel_controls: ChannelControls,
}

/// How a chip instrument hears its voice's channel volume (CC7), expression
/// (CC11) and pan (CC10). The gain and pan curves are the reference
/// synthesizer's SF2 default modulators, applied to the chip's native output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChannelControls {
    /// The lanes are reported as not applied; mirror them with the mixer.
    #[default]
    Ignore,
    /// CC7 × CC11 gain, (value / 127)² each, starting at CC7 100 and CC11 127.
    Level,
    /// The `level` gain and CC10 equal-power pan, starting at CC10 64.
    LevelAndPan,
}

impl ChannelControls {
    pub fn is_ignore(&self) -> bool {
        *self == Self::Ignore
    }
}

/// How a note's MIDI pitch becomes the 14-bit VxPITCH value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SnesPitchMap {
    /// Equal temperament: `root_register` (1..0x3FFF) sounds `root_note`.
    Root { root_note: u8, root_register: u16 },
    /// Equal temperament against the root and declared rate recorded in the
    /// sample plan, as render_instrument's `plan_note`. Needs `plan`.
    PlanRoot,
    /// Every note plays this register (1..0x3FFF), for unpitched sounds.
    Fixed { register: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SnesVelocity {
    /// Every note uses `volume`.
    Ignore,
    /// VxVOL = volume × velocity / 127, truncated toward zero.
    ScaleVolume,
}

/// A saved FM bank record (`bank` with `index`) or explicit `registers`,
/// played as render_fm_patch plays it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FmInstrument {
    /// Saved FM bank; use with `index`. Mutually exclusive with `registers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// Explicit B0h and register slots 0,4,8,12 for groups 30h..90h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registers: Option<FmPatch>,
    /// Chip, clock, pan, LFO and sensitivities, as for render_fm_patch.
    pub context: OpnAuditionContext,
    /// Equal-tempered note mapping from this A4 frequency, 220.0..880.0 Hz.
    pub a4_hz: Tenths,
    /// Fixed block 0..7. Default: the smallest block whose F-number fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<u8>,
    pub velocity: FmVelocity,
    /// FM channels the instrument may use: 1..6, or 1..3 on YM2203. Default all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_voices: Option<u8>,
    /// Whether the voice's CC7/CC11 (and CC10) lanes shape the chip output.
    /// Default `ignore`.
    #[serde(default, skip_serializing_if = "ChannelControls::is_ignore")]
    pub channel_controls: ChannelControls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FmVelocity {
    /// Every note adds this offset to each carrier TL (0.75 dB steps).
    Fixed { carrier_tl_offset: u8 },
    /// Carrier TL offset round((127 − velocity) × range / 126): 0 at
    /// velocity 127, `range` at velocity 1.
    ScaleCarrierTl { range: u8 },
}

impl FmVelocity {
    pub fn offset(self, velocity: u8) -> u8 {
        match self {
            Self::Fixed { carrier_tl_offset } => carrier_tl_offset,
            Self::ScaleCarrierTl { range } => {
                let quiet = u32::from(127 - velocity.clamp(1, 127));
                ((quiet * u32::from(range) * 2 + 126) / 252) as u8
            }
        }
    }
}

impl VoiceInstrument {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SnesBrr(_) => "snes_brr",
            Self::Fm(_) => "fm",
        }
    }

    /// The saved source it refers to, for reports.
    pub fn source(&self) -> Value {
        match self {
            Self::SnesBrr(i) => json!({"bank": i.bank, "slot": i.slot, "plan": i.plan}),
            Self::Fm(i) => {
                json!({"bank": i.bank, "index": i.index, "explicit_registers": i.registers.is_some()})
            }
        }
    }
}

/// Checks voice references and the values that need no saved data.
pub(crate) fn check_instruments(
    composition: &Composition,
    voice_ids: &HashSet<&str>,
    report: &mut Report,
) {
    let mut seen = HashSet::new();
    for (i, assignment) in composition.mix.instruments.iter().enumerate() {
        let mut at = report.nested(format!("mix/instruments/{i}"));
        if !voice_ids.contains(assignment.voice.as_str()) {
            at.push(
                Code::UnknownReference,
                "voice",
                format!("voice `{}` does not exist", assignment.voice),
            );
        }
        if !seen.insert(assignment.voice.as_str()) {
            at.push(
                Code::DuplicateId,
                "voice",
                format!("voice `{}` has two instruments", assignment.voice),
            );
        }
        let mut at = at.nested("instrument");
        match &assignment.instrument {
            VoiceInstrument::SnesBrr(i) => check_snes(i, &mut at),
            VoiceInstrument::Fm(i) => check_fm(i, &mut at),
        }
    }
}

fn check_snes(i: &SnesBrrInstrument, report: &mut Report) {
    match (&i.bank, i.slot, &i.plan) {
        (Some(_), Some(_), None) | (None, None, Some(_)) => {}
        _ => report.push(
            Code::InvalidRange,
            "bank",
            "select bank with slot, or plan, but not both",
        ),
    }
    let register = |value: u16| (1..=0x3fff).contains(&value);
    match i.pitch {
        SnesPitchMap::Root {
            root_note,
            root_register,
        } if root_note > 127 || !register(root_register) => report.push(
            Code::ValueOutOfRange,
            "pitch",
            "root_note is MIDI 0..127 and root_register 1..0x3FFF",
        ),
        SnesPitchMap::PlanRoot if i.plan.is_none() => report.push(
            Code::InvalidRange,
            "pitch",
            "plan_root needs a sample plan; give a bank slot an explicit root",
        ),
        SnesPitchMap::Fixed { register: value } if !register(value) => report.push(
            Code::ValueOutOfRange,
            "pitch/register",
            "the pitch register is 1..0x3FFF",
        ),
        _ => {}
    }
    if matches!(i.envelope, DspEnvelope::BankTable { .. }) && i.bank.is_none() {
        report.push(
            Code::InvalidRange,
            "envelope",
            "bank_table needs a bank slot; give a plan explicit registers",
        );
    }
    if i.counter_start
        .is_some_and(|c| u32::from(c) >= snes_dsp::COUNTER_RANGE)
    {
        report.push(
            Code::ValueOutOfRange,
            "counter_start",
            "counter_start is 0..0x77FF",
        );
    }
    if i.max_voices
        .is_some_and(|v| !(1..=SNES_VOICES).contains(&v))
    {
        report.push(
            Code::ValueOutOfRange,
            "max_voices",
            format!("the S-DSP has 1..{SNES_VOICES} voices"),
        );
    }
}

fn check_fm(i: &FmInstrument, report: &mut Report) {
    match (&i.bank, i.index, &i.registers) {
        (Some(_), Some(_), None) | (None, None, Some(_)) => {}
        _ => report.push(
            Code::InvalidRange,
            "bank",
            "provide bank with index, or explicit registers, exactly one",
        ),
    }
    if !(A4_RANGE_HZ.0..=A4_RANGE_HZ.1).contains(&i.a4_hz.value()) {
        report.push(Code::ValueOutOfRange, "a4_hz", "a4_hz is 220.0..880.0");
    }
    if i.block.is_some_and(|b| b > 7) {
        report.push(Code::ValueOutOfRange, "block", "block is 0..7");
    }
    let channels = if i.context.chip == Some(OpnChip::Ym2203) {
        3
    } else {
        6
    };
    if i.max_voices.is_some_and(|v| !(1..=channels).contains(&v)) {
        report.push(
            Code::ValueOutOfRange,
            "max_voices",
            format!("this chip has 1..{channels} FM channels"),
        );
    }
}

/// Applies `set_instrument` or `clear_instrument`.
pub(crate) fn apply_instrument_operation(
    composition: &mut Composition,
    operation: &EditOperation,
    report: &mut Report,
) {
    match operation {
        EditOperation::SetInstrument { voice, instrument } => {
            if !composition.voices.iter().any(|v| &v.id == voice) {
                report.push(
                    Code::UnknownReference,
                    "voice",
                    format!("voice `{voice}` does not exist"),
                );
                return;
            }
            let instruments = &mut composition.mix.instruments;
            match instruments.iter_mut().find(|a| &a.voice == voice) {
                Some(existing) => existing.instrument = instrument.clone(),
                None => instruments.push(InstrumentAssignment {
                    voice: voice.clone(),
                    instrument: instrument.clone(),
                }),
            }
        }
        EditOperation::ClearInstrument { voice } => {
            let instruments = &mut composition.mix.instruments;
            let before = instruments.len();
            instruments.retain(|a| &a.voice != voice);
            if instruments.len() == before {
                report.push(
                    Code::UnknownReference,
                    "voice",
                    format!("voice `{voice}` has no instrument"),
                );
            }
        }
        _ => unreachable!("only instrument operations are delegated here"),
    }
}

/// Voices whose instrument assignment differs between two mixers.
pub(crate) fn changed_voices<'a>(
    before: &'a [InstrumentAssignment],
    after: &'a [InstrumentAssignment],
) -> Vec<&'a str> {
    let find = |list: &'a [InstrumentAssignment], voice: &str| {
        list.iter()
            .find(|a| a.voice == voice)
            .map(|a| &a.instrument)
    };
    let mut voices: Vec<&str> = before
        .iter()
        .chain(after)
        .map(|a| a.voice.as_str())
        .collect();
    voices.sort_unstable();
    voices.dedup();
    voices
        .into_iter()
        .filter(|v| find(before, v) != find(after, v))
        .collect()
}

/// How a resolved S-DSP instrument chooses pitch registers.
#[derive(Debug, Clone, Copy)]
pub enum SnesPitch {
    Root(NoteRoot),
    Fixed(u16),
}

/// An S-DSP instrument with its saved sample loaded and registers resolved.
#[derive(Debug, Clone)]
pub struct SnesChip {
    pub brr: Vec<u8>,
    pub loop_offset: Option<usize>,
    pub pitch: SnesPitch,
    pub adsr1: u8,
    pub adsr2: u8,
    pub gain: u8,
    pub volume: [i8; 2],
    pub master: [i8; 2],
    pub velocity: SnesVelocity,
    pub counter_start: u32,
}

/// An FM instrument with its patch loaded and context checked.
#[derive(Debug, Clone)]
pub struct FmChip {
    pub patch: FmPatch,
    pub setup: OpnSetup,
    pub a4_hz: f64,
    pub block: Option<u8>,
    pub velocity: FmVelocity,
}

#[derive(Debug, Clone)]
pub enum Chip {
    Snes(Box<SnesChip>),
    Fm(Box<FmChip>),
}

/// A voice's instrument, resolved from saved data for one render.
#[derive(Debug, Clone)]
pub struct ChipInstrument {
    /// Index of the assignment in `mix.instruments`, for diagnostics.
    pub assignment: usize,
    pub chip: Chip,
    /// Chip voices or channels the allocator may use.
    pub max_voices: usize,
    /// Whether CC7, CC11 and CC10 shape the output.
    pub channel_controls: ChannelControls,
    /// Source identities, envelope and context reported with renders.
    pub report: Value,
}

impl ChipInstrument {
    /// The native sample rate as an exact ratio `numerator / denominator` Hz.
    pub fn native_rate(&self) -> (u32, u32) {
        match &self.chip {
            Chip::Snes(_) => (snes_dsp::SAMPLE_RATE, 1),
            Chip::Fm(fm) => (fm.setup.clock_hz, fm.setup.divider),
        }
    }

    pub fn hardware_voices(&self) -> usize {
        match &self.chip {
            Chip::Snes(_) => SNES_VOICES.into(),
            Chip::Fm(fm) => crate::opn::channel_count(fm.setup.chip),
        }
    }
}

/// A chip render converted to the render rate, with its report.
pub struct ChipRender {
    pub signal: [Vec<f32>; 2],
    pub report: Value,
}

/// One note of the voice at native samples.
struct ChipNote {
    key: u8,
    velocity: u8,
    seconds: Rational,
    start: u64,
    /// `u64::MAX` when the note is never released in this render.
    stop: u64,
    voice: usize,
}

fn native_sample(seconds: Rational, (numerator, denominator): (u32, u32)) -> u64 {
    (seconds * Rational::new(numerator.into(), denominator.into()))
        .ceil()
        .to_integer()
        .max(0) as u64
}

fn seconds_value(seconds: Rational) -> f64 {
    *seconds.numer() as f64 / *seconds.denom() as f64
}

fn controller_name(controller: u8) -> String {
    use crate::performance::*;
    let name = match controller {
        CC_MODULATION => "modulation",
        CC_VOLUME => "volume",
        CC_PAN => "pan",
        CC_EXPRESSION => "expression",
        CC_REVERB => "reverb",
        CC_CHORUS => "chorus",
        CC_DATA_ENTRY | CC_DATA_ENTRY_LSB | CC_RPN_LSB | CC_RPN_MSB => "rpn",
        _ => "other",
    };
    format!("cc{controller} {name}")
}

/// Plays one voice's channel messages on its chip instrument. `events` hold
/// exact render times; `release_at` keys off every held note there, as an
/// excerpt's end does. The result has `length` samples at `sample_rate`.
pub fn render_chip(
    voice: &str,
    instrument: &ChipInstrument,
    events: &[(Rational, ChannelMessage)],
    release_at: Option<Rational>,
    length: usize,
    sample_rate: u32,
) -> Result<ChipRender, Diagnostic> {
    let rate = instrument.native_rate();
    let location = format!("/mix/instruments/{}", instrument.assignment);
    let refuse = |code: Code, message: String| Diagnostic {
        code,
        location: location.clone(),
        message,
    };
    // Pair in exact time order, where releases and controls precede notes at
    // one time; native samples follow, so a note shorter than one native
    // sample keeps its own release.
    let mut timed: Vec<(u64, Rational, ChannelMessage)> = events
        .iter()
        .map(|&(at, message)| (native_sample(at, rate), at, message))
        .collect();
    timed.sort_by_key(|(_, at, m)| (*at, matches!(m, ChannelMessage::NoteOn { .. })));
    let mut notes: Vec<ChipNote> = Vec::new();
    let mut held: BTreeMap<u8, VecDeque<usize>> = BTreeMap::new();
    let mut not_applied: BTreeMap<String, u64> = BTreeMap::new();
    let mut unpaired = 0u64;
    let policy = instrument.channel_controls;
    let mut levels = ChannelLevels::default();
    // `(native sample, [left, right] gain)` from that sample on.
    let mut gains: Vec<(u64, [f64; 2])> = vec![(0, levels.gains(policy))];
    let mut controls_applied: BTreeMap<String, u64> = BTreeMap::new();
    for &(n, at, message) in &timed {
        match message {
            ChannelMessage::NoteOn { key, velocity } => {
                held.entry(key).or_default().push_back(notes.len());
                notes.push(ChipNote {
                    key,
                    velocity,
                    seconds: at,
                    start: n,
                    stop: u64::MAX,
                    voice: 0,
                });
            }
            ChannelMessage::NoteOff { key, .. } => {
                match held.get_mut(&key).and_then(VecDeque::pop_front) {
                    Some(index) => notes[index].stop = n,
                    None => unpaired += 1,
                }
            }
            ChannelMessage::Controller { controller, value }
                if levels.hears(policy, controller) =>
            {
                levels.set(controller, value);
                gains.push((n, levels.gains(policy)));
                *controls_applied
                    .entry(controller_name(controller))
                    .or_default() += 1;
            }
            ChannelMessage::Controller { controller, .. } => {
                *not_applied.entry(controller_name(controller)).or_default() += 1;
            }
            ChannelMessage::Program(_) => *not_applied.entry("program".into()).or_default() += 1,
            ChannelMessage::PitchBend(_) => {
                *not_applied.entry("pitch_bend".into()).or_default() += 1
            }
            ChannelMessage::ChannelPressure(_) => {
                *not_applied.entry("channel_pressure".into()).or_default() += 1
            }
        }
    }
    if let Some(at) = release_at {
        let n = native_sample(at, rate);
        for note in notes.iter_mut().filter(|n| n.stop == u64::MAX) {
            note.stop = n.max(note.start);
        }
    }
    // Key timing: the S-DSP polls KON/KOFF on even samples, so writes are made
    // just before the poll; a note keys off at least one poll after key-on.
    let snes = matches!(instrument.chip, Chip::Snes(_));
    for note in &mut notes {
        if snes {
            note.start = note.start.next_multiple_of(2);
            if note.stop != u64::MAX {
                note.stop = note.stop.next_multiple_of(2).max(note.start + 2);
            }
        } else if note.stop != u64::MAX {
            note.stop = note.stop.max(note.start + 1);
        }
    }
    // Lowest-numbered chip voice holding no note; a releasing voice is re-keyed.
    let mut busy_until = vec![0u64; instrument.max_voices];
    let mut order: Vec<usize> = (0..notes.len()).collect();
    order.sort_by_key(|&i| notes[i].start);
    let mut peak_simultaneous = 0;
    for &i in &order {
        let start = notes[i].start;
        let Some(voice) = busy_until.iter().position(|&until| until <= start) else {
            let note = &notes[i];
            return Err(refuse(
                Code::ChipVoicesExceeded,
                format!(
                    "voice `{voice}` needs more than {} chip voices at {:.6} s (key {}); allow more with max_voices up to {}, or thin the chord",
                    instrument.max_voices,
                    seconds_value(note.seconds),
                    note.key,
                    instrument.hardware_voices()
                ),
            ));
        };
        busy_until[voice] = notes[i].stop;
        notes[i].voice = voice;
        peak_simultaneous =
            peak_simultaneous.max(busy_until.iter().filter(|&&u| u > start).count());
    }
    let (numerator, denominator) = rate;
    let resampled = !(denominator == 1 && numerator == sample_rate);
    let (from, to) = (numerator, sample_rate * denominator);
    let native_length = if resampled {
        crate::sample_resample::stream_source_samples(from, to, length)
    } else {
        length
    };
    let (frames, chip_report) =
        match &instrument.chip {
            Chip::Snes(chip) => render_snes(chip, &notes, native_length)
                .map_err(|e| refuse(Code::TargetRefused, e))?,
            Chip::Fm(chip) => render_fm(chip, &notes, native_length)
                .map_err(|e| refuse(Code::TargetRefused, e))?,
        };
    let scale = |v: f64| (v / f64::from(i16::MAX)) as f32;
    let mut sides: [Vec<f64>; 2] =
        [0, 1].map(|side| frames.iter().map(|f| f64::from(f[side])).collect());
    if !policy.is_ignore() {
        apply_gains(&mut sides, &gains);
    }
    let signal: [Vec<f32>; 2] = if resampled {
        let out = crate::sample_resample::stream_values(&[&sides[0], &sides[1]], from, to, length);
        [0, 1].map(|side| out[side].iter().map(|&v| scale(v)).collect())
    } else {
        [0, 1].map(|side| sides[side].iter().take(length).map(|&v| scale(v)).collect())
    };
    let mut per_chip_voice = vec![0u64; busy_until.len()];
    for note in &notes {
        per_chip_voice[note.voice] += 1;
    }
    let allocation: Vec<Value> = notes
        .iter()
        .take(FIRST_NOTES_LISTED)
        .map(|n| json!({"key": n.key, "velocity": n.velocity, "seconds": seconds_value(n.seconds),
            "native_key_on": n.start, "native_key_off": (n.stop != u64::MAX).then_some(n.stop), "chip_voice": n.voice}))
        .collect();
    let mut report = json!({
        "voice": voice,
        "instrument": instrument.report,
        "native_rate": {"exact": format!("{numerator}/{denominator}"), "hz": f64::from(numerator) / f64::from(denominator)},
        "render_rate": sample_rate,
        "conversion": if resampled {
            json!({"resampled": true, "method": crate::sample_resample::METHOD,
                "zero_crossings_per_side": crate::sample_resample::ZERO_CROSSINGS, "kaiser_beta": crate::sample_resample::KAISER_BETA,
                "cutoff_of_lower_nyquist": crate::sample_resample::CUTOFF,
                "phases": "exact per phase up to 4096 reduced phases; otherwise linear interpolation between 4097 exact phases",
                "scale": "PCM16 value / 32767 into the mixer"})
        } else {
            json!({"resampled": false, "reason": "the render rate equals the native rate", "scale": "PCM16 value / 32767 into the mixer"})
        },
        "notes": notes.len(),
        "chip_voices": {"limit": instrument.max_voices, "hardware": instrument.hardware_voices(), "peak_simultaneous": peak_simultaneous,
            "policy": "each note takes the lowest-numbered chip voice holding no note; a voice still in its release is re-keyed; a note that finds none refuses the render",
            "notes_per_chip_voice": per_chip_voice,
            "first_notes": allocation,
            "first_notes_listed": format!("the first {} of {} notes", allocation.len(), notes.len())},
        "applied": ["note key-on at the note start and key-off at its end", "note pitch through the instrument's pitch mapping", "note velocity through the instrument's velocity policy"],
        "not_applied": not_applied,
        "unpaired_note_offs": unpaired,
        "chip": chip_report,
        "state": "the chip starts from reset at render sample 0; loop passes continue its state, and excerpts start it at the excerpt start like the reference synthesizer",
    });
    if !policy.is_ignore() {
        report["applied"]
            .as_array_mut()
            .expect("an array")
            .push(json!(match policy {
                ChannelControls::LevelAndPan => "CC7 volume, CC11 expression and CC10 pan as gain and equal-power pan on the chip output",
                _ => "CC7 volume and CC11 expression as gain on the chip output",
            }));
        let last = gains.last().map(|(_, g)| *g).unwrap_or([1.0, 1.0]);
        report["channel_controls"] = json!({
            "policy": policy,
            "applied": controls_applied,
            "initial": {"cc7": 100, "cc11": 127, "cc10": 64},
            "changes": gains.len() - 1,
            "gain_at_last_change": {"left": last[0], "right": last[1], "note": "linear gain per side after the last controller change, held to the render end"},
            "gain_curve": "the reference synthesizer's SF2 default modulators: attenuation 960 x concave(1 - value/127) centibels each for CC7 and CC11, summed and limited to 1440 cB, so each contributes (value/127)^2 for values 1..127; CC121 resets CC11 to 127",
            "pan_curve": if policy == ChannelControls::LevelAndPan { json!("SF2 pan 500 x (2 x CC10/128 - 1) tenths of a percent, equal-power: left x cos, right x sin of (pan + 500)/1000 x 90 degrees; CC10 64 gives 0.7071 on each side") } else { Value::Null },
            "timing": "a controller takes effect from the first native sample at or after its time (ceil(seconds x native rate)), as a step, before rate conversion and before the voice's effects, fader and sends",
        });
    }
    Ok(ChipRender { signal, report })
}

/// The channel volume, pan and expression a chip instrument hears.
struct ChannelLevels {
    volume: u8,
    pan: u8,
    expression: u8,
}

impl Default for ChannelLevels {
    /// The reference synthesizer's channel defaults.
    fn default() -> Self {
        Self {
            volume: 100,
            pan: 64,
            expression: 127,
        }
    }
}

impl ChannelLevels {
    /// Whether `controller` changes the output under `policy`.
    fn hears(&self, policy: ChannelControls, controller: u8) -> bool {
        use crate::performance::{CC_EXPRESSION, CC_PAN, CC_VOLUME};
        match policy {
            ChannelControls::Ignore => false,
            ChannelControls::Level => {
                matches!(controller, CC_VOLUME | CC_EXPRESSION | CC_RESET_ALL)
            }
            ChannelControls::LevelAndPan => {
                matches!(
                    controller,
                    CC_VOLUME | CC_EXPRESSION | CC_PAN | CC_RESET_ALL
                )
            }
        }
    }

    fn set(&mut self, controller: u8, value: u8) {
        use crate::performance::{CC_EXPRESSION, CC_PAN, CC_VOLUME};
        match controller {
            CC_VOLUME => self.volume = value,
            CC_EXPRESSION => self.expression = value,
            CC_PAN => self.pan = value,
            // RP-015, as the synthesizer: expression resets; volume and pan keep.
            CC_RESET_ALL => self.expression = 127,
            _ => {}
        }
    }

    /// `[left, right]` gains under `policy`, from the synthesizer's curves.
    fn gains(&self, policy: ChannelControls) -> [f64; 2] {
        use crate::synth::{math::centibels_to_gain, modulator::concave, voice::pan_gains};
        let attenuation = |value: u8| 960.0 * concave(1.0 - f64::from(value) / 127.0);
        let gain = centibels_to_gain(
            (attenuation(self.volume) + attenuation(self.expression)).clamp(0.0, 1440.0),
        );
        match policy {
            ChannelControls::Ignore => [1.0, 1.0],
            ChannelControls::Level => [gain, gain],
            ChannelControls::LevelAndPan => {
                let (left, right) = pan_gains(500.0 * (2.0 * f64::from(self.pan) / 128.0 - 1.0));
                [gain * left, gain * right]
            }
        }
    }
}

/// Reset All Controllers.
const CC_RESET_ALL: u8 = 121;

/// Multiplies native samples by the gain in effect at each one.
fn apply_gains(sides: &mut [Vec<f64>; 2], gains: &[(u64, [f64; 2])]) {
    for (i, &(start, gain)) in gains.iter().enumerate() {
        let end = gains.get(i + 1).map_or(u64::MAX, |(next, _)| *next);
        for (side, values) in sides.iter_mut().enumerate() {
            let (start, end) = (
                (start as usize).min(values.len()),
                (end.min(values.len() as u64)) as usize,
            );
            for value in &mut values[start..end] {
                *value *= gain[side];
            }
        }
    }
}

/// The writes before one even DSP sample: voices to key off, and notes to
/// key on as (voice, pitch register, VxVOL).
#[derive(Default)]
struct PollWrites {
    key_off: u8,
    key_on: Vec<(usize, u16, [i8; 2])>,
}

fn render_snes(
    chip: &SnesChip,
    notes: &[ChipNote],
    length: usize,
) -> Result<(Vec<[i16; 2]>, Value), String> {
    snes_dsp::check_sample(&chip.brr, chip.loop_offset)?;
    let mut actions: BTreeMap<u64, PollWrites> = BTreeMap::new();
    let (mut rounding, mut lowest, mut highest) = (0.0f64, u16::MAX, 0u16);
    for note in notes {
        let (register, detail) = match chip.pitch {
            SnesPitch::Fixed(register) => (register, Value::Null),
            SnesPitch::Root(root) => snes_dsp::note_register(note.key, root).map_err(|e| {
                format!(
                    "key {} at {:.6} s: {e}",
                    note.key,
                    seconds_value(note.seconds)
                )
            })?,
        };
        if let Some(cents) = detail.get("rounding_cents").and_then(Value::as_f64) {
            rounding = rounding.max(cents.abs());
        }
        (lowest, highest) = (lowest.min(register), highest.max(register));
        let volume = match chip.velocity {
            SnesVelocity::Ignore => chip.volume,
            SnesVelocity::ScaleVolume => chip
                .volume
                .map(|v| (i32::from(v) * i32::from(note.velocity) / 127) as i8),
        };
        actions
            .entry(note.start)
            .or_default()
            .key_on
            .push((note.voice, register, volume));
        if note.stop != u64::MAX {
            actions.entry(note.stop).or_default().key_off |= 1 << note.voice;
        }
    }
    let mut dsp = Dsp::new(
        snes_dsp::audition_ram(&chip.brr, chip.loop_offset),
        chip.counter_start,
    );
    for (register, value) in [
        (FLG, 0x20),
        (DIR, AUDITION_DIR),
        (MVOLL, chip.master[0] as u8),
        (MVOLR, chip.master[1] as u8),
    ] {
        dsp.write(register, value);
    }
    let mut koff = 0u8;
    let mut frames = Vec::with_capacity(length);
    let mut next = actions.iter().peekable();
    for n in 0..length as u64 {
        if let Some((_, writes)) = next.next_if(|(at, _)| **at == n) {
            koff |= writes.key_off;
            let mut kon = 0u8;
            for &(voice, pitch, volume) in &writes.key_on {
                let row = voice * 16;
                for (register, value) in [
                    (VOLL, volume[0] as u8),
                    (VOLR, volume[1] as u8),
                    (PITCHL, pitch as u8),
                    (PITCHH, (pitch >> 8) as u8),
                    (SRCN, 0),
                    (ADSR1, chip.adsr1),
                    (ADSR2, chip.adsr2),
                    (GAIN, chip.gain),
                ] {
                    dsp.write(row + register, value);
                }
                kon |= 1 << voice;
            }
            koff &= !kon;
            dsp.write(KOFF, koff);
            if kon != 0 {
                dsp.write(KON, kon);
            }
        }
        frames.push(dsp.tick());
    }
    let hex = |value: u8| format!("0x{value:02X}");
    let report = json!({
        "model": "S-DSP voice model (render_instrument), one DSP per instrument render",
        "aram": {"directory_page": hex(AUDITION_DIR), "srcn": 0, "start": format!("0x{AUDITION_START:04X}"),
            "bytes": chip.brr.len(), "loop_offset": chip.loop_offset, "layout": "audition layout, not the native ARAM layout"},
        "registers": {"adsr1": hex(chip.adsr1), "adsr2": hex(chip.adsr2), "gain": hex(chip.gain),
            "volume": {"left": chip.volume[0], "right": chip.volume[1]},
            "master_volume": {"left": chip.master[0], "right": chip.master[1]}, "flg": "0x20"},
        "velocity": chip.velocity,
        "pitch_registers": if notes.is_empty() { Value::Null } else { json!({"lowest": lowest, "highest": highest, "largest_rounding_cents": rounding}) },
        "counter_start": chip.counter_start,
        "timing": "register writes for a note are made just before the even DSP sample at which KON/KOFF are polled; a note keys off at least one poll after its key-on",
        "not_modelled": "echo/FIR, noise, pitch modulation, intra-sample voice cycle order, pre-key-on decode, analog output stage and the console's real sample clock",
    });
    Ok((frames, report))
}

fn render_fm(
    chip: &FmChip,
    notes: &[ChipNote],
    length: usize,
) -> Result<(Vec<[i16; 2]>, Value), String> {
    let mut plans = Vec::with_capacity(notes.len());
    for note in notes {
        let pitch = OpnPitch::MidiNote {
            note: note.key,
            a4_hz: chip.a4_hz,
            block: chip.block,
        };
        let (fnum, block, detail) = resolve_pitch(&pitch, chip.setup.native_rate).map_err(|e| {
            format!(
                "key {} at {:.6} s: {e}",
                note.key,
                seconds_value(note.seconds)
            )
        })?;
        plans.push(NotePlan {
            channel: note.voice,
            start: note.start,
            stop: note.stop,
            fnum,
            block,
            offset: chip.velocity.offset(note.velocity),
            pitch: detail,
        });
    }
    let mut order: Vec<usize> = (0..plans.len()).collect();
    order.sort_by_key(|&i| (plans[i].channel, plans[i].start));
    let (writes, _) = register_writes(&chip.patch, &chip.setup, &plans, &order);
    let mut model = OpnModel::new(chip.setup.chip)?;
    let (mut next, mut clipped, mut peak) = (0, 0u64, 0i32);
    let mut frames = Vec::with_capacity(length);
    for n in 0..length as u64 {
        while next < writes.len() && writes[next].sample == n {
            let w = writes[next];
            model.write(w.port, w.address, w.data)?;
            next += 1;
        }
        frames.push(pcm_frame(model.sample(), &mut peak, &mut clipped));
    }
    let cents: Vec<f64> = plans
        .iter()
        .filter_map(|p| p.pitch.get("cents_error").and_then(Value::as_f64))
        .collect();
    let report = json!({
        "model": "Tunemend OPN FM model (render_fm_patch)",
        "chip": chip.setup.chip,
        "clock_hz": chip.setup.clock_hz,
        "prescaler": chip.setup.prescaler,
        "divider": chip.setup.divider,
        "context": chip.setup.context_report(),
        "patch": chip.patch,
        "patch_sha256": chip.patch.digest(),
        "a4_hz": chip.a4_hz,
        "block_policy": if chip.block.is_some() { "declared" } else { "smallest_block_fitting_11_bit_fnum" },
        "velocity": chip.velocity,
        "largest_pitch_error_cents": cents.iter().fold(0.0f64, |a, c| a.max(c.abs())),
        "register_writes": writes.len(),
        "peak_14bit": peak,
        "clipped_samples": clipped,
        "output_scale": "summed 14-bit channel outputs clamped to -8192..8191, shifted left 2 into signed 16-bit PCM",
        "timing": "register writes take effect at native sample boundaries; key-off follows key-on by at least one native sample",
        "scope": crate::opn::model_scope(chip.setup.ssg_used()),
    });
    Ok((frames, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(max_voices: usize) -> ChipInstrument {
        // Two blocks of equal nibbles, looping from the second.
        let brr = [[0xc0u8; 9], [0xc3u8; 9]].concat();
        ChipInstrument {
            assignment: 0,
            chip: Chip::Snes(Box::new(SnesChip {
                brr,
                loop_offset: Some(9),
                pitch: SnesPitch::Root(NoteRoot::Register {
                    root_note: 60,
                    root_register: 0x1000,
                }),
                adsr1: 0,
                adsr2: 0,
                gain: 0x7f,
                volume: [64, 64],
                master: [127, 127],
                velocity: SnesVelocity::ScaleVolume,
                counter_start: 0,
            })),
            max_voices,
            channel_controls: ChannelControls::Ignore,
            report: Value::Null,
        }
    }

    fn at(samples: i128) -> Rational {
        Rational::new(samples, 32000)
    }

    fn on(key: u8, velocity: u8) -> ChannelMessage {
        ChannelMessage::NoteOn { key, velocity }
    }

    fn off(key: u8) -> ChannelMessage {
        ChannelMessage::NoteOff { key, velocity: 64 }
    }

    fn play(
        max_voices: usize,
        events: &[(Rational, ChannelMessage)],
        release_at: Option<Rational>,
        length: usize,
    ) -> ChipRender {
        render_chip("v", &chip(max_voices), events, release_at, length, 32000).unwrap()
    }

    #[test]
    fn velocity_policies_scale_volume_and_carrier_level() {
        let scaled = FmVelocity::ScaleCarrierTl { range: 24 };
        assert_eq!([127, 64, 1].map(|v| scaled.offset(v)), [0, 12, 24]);
        assert_eq!(
            FmVelocity::Fixed {
                carrier_tl_offset: 5
            }
            .offset(1),
            5
        );
        let loud = play(8, &[(at(0), on(60, 127)), (at(400), off(60))], None, 600);
        let soft = play(8, &[(at(0), on(60, 64)), (at(400), off(60))], None, 600);
        let peak = |r: &ChipRender| r.signal[0].iter().fold(0.0f32, |p, s| p.max(s.abs()));
        let ratio = peak(&soft) / peak(&loud);
        assert!((0.48..0.52).contains(&ratio), "{ratio}");
    }

    #[test]
    fn key_writes_wait_for_the_even_poll_and_short_notes_still_release() {
        let even = play(8, &[(at(10), on(60, 127)), (at(300), off(60))], None, 800);
        let odd = play(8, &[(at(9), on(60, 127)), (at(299), off(60))], None, 800);
        assert_eq!(even.signal, odd.signal);
        assert!(even.signal[0][100..300].iter().any(|&s| s != 0.0));
        // A note shorter than one poll interval keys off at the next poll.
        let release = Rational::new(21, 64000);
        let short = play(8, &[(at(10), on(60, 127)), (release, off(60))], None, 800);
        assert_eq!(
            short.report["chip_voices"]["first_notes"][0]["native_key_off"],
            12
        );
        assert!(short.signal[0][700..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn controls_and_unpaired_releases_are_reported_not_applied() {
        let events = [
            (at(0), ChannelMessage::Program(5)),
            (at(0), ChannelMessage::PitchBend(100)),
            (
                at(0),
                ChannelMessage::Controller {
                    controller: 7,
                    value: 90,
                },
            ),
            (at(0), on(60, 100)),
            (at(100), off(62)),
            (at(200), off(60)),
        ];
        let report = play(1, &events, None, 400).report;
        assert_eq!(report["not_applied"]["program"], 1);
        assert_eq!(report["not_applied"]["pitch_bend"], 1);
        assert_eq!(report["not_applied"]["cc7 volume"], 1);
        assert_eq!(report["unpaired_note_offs"], 1);
        // A note held at an excerpt end is released there.
        let held = play(1, &[(at(0), on(60, 100))], Some(at(100)), 400);
        assert_eq!(
            held.report["chip_voices"]["first_notes"][0]["native_key_off"],
            100
        );
    }

    fn cc(controller: u8, value: u8) -> ChannelMessage {
        ChannelMessage::Controller { controller, value }
    }

    fn play_with(
        policy: ChannelControls,
        events: &[(Rational, ChannelMessage)],
        length: usize,
    ) -> ChipRender {
        let mut instrument = chip(8);
        instrument.channel_controls = policy;
        render_chip("v", &instrument, events, None, length, 32000).unwrap()
    }

    /// The controller gain the synthesizer's default modulators give.
    fn synth_gains(volume: u8, expression: u8, pan: Option<u8>) -> [f64; 2] {
        use crate::synth::math::centibels_to_gain;
        use crate::synth::modulator::{DEFAULT_MODULATORS, SourceInputs};
        let mut controllers = [0u8; 128];
        controllers[7] = volume;
        controllers[11] = expression;
        controllers[10] = pan.unwrap_or(64);
        let inputs = SourceInputs {
            velocity: 127,
            key: 60,
            controllers: &controllers,
            channel_pressure: 0,
            pitch_wheel: 8192,
            pitch_wheel_semitones: 2.0,
        };
        let attenuation =
            DEFAULT_MODULATORS[4].value(&inputs) + DEFAULT_MODULATORS[6].value(&inputs);
        let gain = centibels_to_gain(attenuation.clamp(0.0, 1440.0));
        match pan {
            None => [gain, gain],
            Some(_) => {
                let (left, right) =
                    crate::synth::voice::pan_gains(DEFAULT_MODULATORS[5].value(&inputs));
                [gain * left, gain * right]
            }
        }
    }

    #[test]
    fn channel_controls_use_the_synthesizer_curves() {
        let level = |volume, expression, pan| ChannelLevels {
            volume,
            expression,
            pan,
        };
        for value in 0..=127u8 {
            assert_eq!(
                level(value, 127, 64).gains(ChannelControls::Level),
                synth_gains(value, 127, None)
            );
            assert_eq!(
                level(100, value, 64).gains(ChannelControls::Level),
                synth_gains(100, value, None)
            );
            assert_eq!(
                level(90, 110, value).gains(ChannelControls::LevelAndPan),
                synth_gains(90, 110, Some(value))
            );
            if value > 0 {
                // Within the 1440 cB limit each controller is (value/127)².
                let [gain, _] = level(value, 127, 64).gains(ChannelControls::Level);
                let square = (f64::from(value) / 127.0).powi(2);
                assert!(
                    (gain / square - 1.0).abs() < 1e-12,
                    "{value}: {gain} {square}"
                );
            }
        }
        // The defaults: CC7 100 alone is -4.15 dB, the centre pan -3.01 dB per side.
        let [left, right] = ChannelLevels::default().gains(ChannelControls::LevelAndPan);
        let expected = 40.0 * (100.0f64 / 127.0).log10() - 3.0103;
        assert!((20.0 * left.log10() - expected).abs() < 1e-3, "{left}");
        assert!((left - right).abs() < 1e-15);
        assert_eq!(
            ChannelLevels::default().gains(ChannelControls::Ignore),
            [1.0, 1.0]
        );
    }

    #[test]
    fn level_and_pan_steps_land_on_the_controller_native_sample() {
        let events = [
            (at(0), on(60, 127)),
            (at(200), cc(7, 64)),
            // 299.5 samples: the change lands on sample 300.
            (Rational::new(599, 64000), cc(11, 100)),
            (at(400), cc(121, 0)),
            (at(450), cc(10, 0)),
            (at(500), cc(91, 40)),
            (at(560), off(60)),
        ];
        let plain = play_with(ChannelControls::Ignore, &events, 640);
        let level = play_with(ChannelControls::Level, &events, 640);
        let panned = play_with(ChannelControls::LevelAndPan, &events, 640);
        assert!(plain.signal[0][1..600].iter().any(|&s| s != 0.0));
        // The PCM16 frame behind each plain sample, and what a gain makes of it.
        let frame = |s: f32| (f64::from(s) * 32767.0).round();
        let scaled = |s: f32, gain: f64| (frame(s) * gain / 32767.0) as f32;
        for n in 0..640 {
            let (volume, expression, pan) = match n {
                0..200 => (100, 127, 64),
                200..300 => (64, 127, 64),
                300..400 => (64, 100, 64),
                400..450 => (64, 127, 64),
                _ => (64, 127, 0),
            };
            let flat = synth_gains(volume, expression, None);
            let stereo = synth_gains(volume, expression, Some(pan));
            for side in 0..2 {
                let dry = plain.signal[side][n];
                assert_eq!(
                    level.signal[side][n],
                    scaled(dry, flat[side]),
                    "level {side} {n}"
                );
                assert_eq!(
                    panned.signal[side][n],
                    scaled(dry, stereo[side]),
                    "pan {side} {n}"
                );
            }
        }
        // CC10 0 is hard left: nothing on the right from sample 450.
        assert!(panned.signal[1][450..].iter().all(|&s| s == 0.0));
        assert!(panned.signal[0][450..560].iter().any(|&s| s != 0.0));
        // Applied controllers leave not_applied; the rest stay there.
        assert_eq!(plain.report["not_applied"]["cc7 volume"], 1);
        assert!(plain.report.get("channel_controls").is_none());
        assert!(level.report["not_applied"].get("cc7 volume").is_none());
        assert_eq!(level.report["not_applied"]["cc10 pan"], 1);
        assert_eq!(level.report["not_applied"]["cc91 reverb"], 1);
        assert_eq!(
            level.report["channel_controls"]["applied"]["cc121 other"],
            1
        );
        assert_eq!(panned.report["channel_controls"]["applied"]["cc10 pan"], 1);
        assert_eq!(panned.report["channel_controls"]["policy"], "level_and_pan");
        assert_eq!(panned.report["channel_controls"]["changes"], 4);
    }

    #[test]
    fn ignored_controllers_leave_the_output_unchanged() {
        let notes = [(at(0), on(60, 127)), (at(300), off(60))];
        let mut with_controls = notes.to_vec();
        with_controls.extend([
            (at(100), cc(7, 20)),
            (at(150), cc(10, 0)),
            (at(200), cc(11, 5)),
        ]);
        with_controls.sort_by_key(|(at, _)| *at);
        let plain = play_with(ChannelControls::Ignore, &notes, 400);
        let controlled = play_with(ChannelControls::Ignore, &with_controls, 400);
        assert_eq!(plain.signal, controlled.signal);
        assert_eq!(play(8, &notes, None, 400).signal, plain.signal);
    }
}
