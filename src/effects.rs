//! Offline insert effects of the mixer: parametric EQ, compressor, limiter,
//! delay, algorithmic reverb and an S-DSP style echo.
//!
//! Each processor runs once over a whole rendered stereo signal, in f64, in a
//! fixed order, so a revision renders reproducibly. Look-ahead reads later
//! samples of the same render instead of delaying the output, so processed
//! audio keeps the sample positions that feedback marks refer to.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::composition::Composition;
use crate::diagnostic::{DiagnosticCode as Code, Report};
use crate::edit::EditOperation;
use crate::mix::{MixTarget, Tenths};

/// Most effects in one chain.
pub const MAX_CHAIN_EFFECTS: usize = 16;
/// Most bands of one equalizer.
pub const MAX_EQ_BANDS: usize = 16;
/// Longest delay time, also for note-synced delays at the slowest tempo.
pub const MAX_DELAY_MS: f64 = 2000.0;
/// Averaging window of RMS level detection.
pub const RMS_WINDOW_MS: f64 = 10.0;
/// Band frequencies above this fraction of the render rate are processed at it.
pub const MAX_BAND_FRACTION: f64 = 0.45;

/// A number with at most two digits after the decimal point, stored exactly
/// as an integer count of hundredths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hundredths(pub i32);

impl Hundredths {
    pub fn value(self) -> f64 {
        f64::from(self.0) / 100.0
    }
}

impl Serialize for Hundredths {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.value())
    }
}

impl<'de> Deserialize<'de> for Hundredths {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        let scaled = value * 100.0;
        let rounded = scaled.round();
        if !value.is_finite() || (scaled - rounded).abs() > 1e-6 || rounded.abs() > 1.0e9 {
            return Err(serde::de::Error::custom(format!(
                "{value} is not a decimal with at most two digits after the point, such as 0.71"
            )));
        }
        Ok(Self(rounded as i32))
    }
}

impl JsonSchema for Hundredths {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Hundredths".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "number",
            "description": "A decimal with at most two digits after the point, such as 0.71."
        })
    }
}

/// The insert effects of one voice, bus or the master, processed in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectChain {
    pub target: MixTarget,
    /// 1–16 effects; the first processes the strip's input.
    pub effects: Vec<Effect>,
}

/// One insert effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    /// Unique within the chain; operations address the effect by it.
    pub id: String,
    /// Passes the signal through unchanged while keeping the settings.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypass: bool,
    pub processor: Processor,
}

/// Processor kind and settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Processor {
    /// Parametric equalizer: RBJ cookbook biquads in series.
    #[serde(rename = "eq")]
    Equalizer(Equalizer),
    /// Feed-forward compressor with stereo-linked detection.
    Compressor(Compressor),
    /// Look-ahead brickwall limiter on sample peaks.
    Limiter(Limiter),
    /// Feedback delay, in milliseconds or a note length at the tempo.
    Delay(Delay),
    /// Freeverb-style algorithmic reverb: mono in, stereo out.
    Reverb(Reverb),
    /// S-DSP style echo: 8-tap FIR, EDL delay, EFB feedback, EVOL volume.
    SnesEcho(SnesEcho),
}

impl Processor {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Equalizer(_) => "eq",
            Self::Compressor(_) => "compressor",
            Self::Limiter(_) => "limiter",
            Self::Delay(_) => "delay",
            Self::Reverb(_) => "reverb",
            Self::SnesEcho(_) => "snes_echo",
        }
    }

    /// The voice or bus whose signal drives this processor's detector.
    pub fn sidechain(&self) -> Option<&MixTarget> {
        match self {
            Self::Compressor(c) => c.sidechain.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Equalizer {
    /// 1–16 bands, processed in order.
    pub bands: Vec<EqBand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EqBand {
    pub filter: FilterKind,
    /// Center, corner or shelf midpoint frequency in Hz, 10.0–20000.0.
    /// Above 0.45 × the render rate the band is processed at that limit.
    pub frequency_hz: Tenths,
    /// Gain in dB, -24.0–24.0; peaking and shelf bands only.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub gain_db: Tenths,
    /// Quality factor, 0.10–24.00; 0.71 is close to Butterworth for passes.
    pub q: Hundredths,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterKind {
    Peaking,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    /// Constant 0 dB peak gain at the center.
    BandPass,
    Notch,
}

impl FilterKind {
    fn has_gain(self) -> bool {
        matches!(self, Self::Peaking | Self::LowShelf | Self::HighShelf)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Compressor {
    /// dBFS, -60.0–0.0.
    pub threshold_db: Tenths,
    /// Input dB over the threshold per output dB, 1.0–100.0.
    pub ratio: Tenths,
    /// Soft knee width in dB, 0.0–24.0, centered on the threshold.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub knee_db: Tenths,
    /// Time constant of rising gain reduction in ms, 0.0–500.0.
    pub attack_ms: Tenths,
    /// Time constant of falling gain reduction in ms, 1.0–5000.0.
    pub release_ms: Tenths,
    /// Gain after compression in dB, -24.0–24.0.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub makeup_db: Tenths,
    #[serde(default, skip_serializing_if = "Detection::is_peak")]
    pub detection: Detection,
    /// Voice or bus whose signal (after its own chain, before its fader)
    /// drives the detector instead of this strip's input. A voice chain can
    /// be keyed by another voice only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidechain: Option<MixTarget>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Detection {
    /// The larger absolute sample of both sides.
    #[default]
    Peak,
    /// Root mean square of both sides over the last 10 ms.
    Rms,
}

impl Detection {
    fn is_peak(&self) -> bool {
        *self == Self::Peak
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limiter {
    /// Highest output sample magnitude in dBFS, -24.0–0.0. Sample peaks, not
    /// inter-sample (true) peaks.
    pub ceiling_db: Tenths,
    /// Time constant of gain recovery in ms, 1.0–5000.0.
    pub release_ms: Tenths,
    /// Gain reduction starts this many ms before a peak, 0.0–20.0, without
    /// delaying the output.
    pub lookahead_ms: Tenths,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Delay {
    pub time: DelayTime,
    /// Share of each echo fed back into the line, 0.0–95.0 percent.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub feedback_percent: Tenths,
    /// Output is (100 - mix)% dry plus mix% echoes, 0.0–100.0.
    pub mix_percent: Tenths,
    /// Echoes alternate left and right from a mono sum of the input.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ping_pong: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "unit", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelayTime {
    /// Milliseconds, 1.0–2000.0.
    Ms { ms: Tenths },
    /// `numerator / denominator` of a whole note at the tempo in effect,
    /// such as 3/16 for a dotted eighth. Music timebase only; at most 2000 ms
    /// at the slowest tempo of the map.
    Note { numerator: u16, denominator: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reverb {
    /// Comb feedback from 0.70 (0%) to 0.98 (100%).
    pub room_size_percent: Tenths,
    /// High-frequency damping inside the combs, 0.0–100.0.
    pub damping_percent: Tenths,
    /// Silence before the reverb input in ms, 0.0–250.0.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub pre_delay_ms: Tenths,
    /// Output is (100 - mix)% dry plus mix% reverb, 0.0–100.0.
    pub mix_percent: Tenths,
}

/// Register-style settings of the S-DSP echo unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnesEcho {
    /// FIR coefficients C0–C7 as signed register bytes; C0 weights the
    /// oldest of the 8 echo samples. `[127, 0, 0, 0, 0, 0, 0, 0]` passes the echo.
    pub fir: [i8; 8],
    /// EDL, 0–15: the echo buffer holds `delay` × 16 ms.
    pub delay: u8,
    /// EFB: echo fed back into the buffer, × feedback / 128.
    pub feedback: i8,
    /// EVOL left: echo output × volume / 128.
    pub volume_left: i8,
    /// EVOL right.
    pub volume_right: i8,
    /// Keeps the input in the output; false outputs only the echo, for a
    /// bus that receives sends.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub dry: bool,
}

// ---------------------------------------------------------------------------
// Parameter automation

/// An automatable effect parameter, named like the processor field it moves.
/// Time-structure settings (delay time, pre-delay, look-ahead, attack,
/// release, knee, EDL, FIR, q, filter kind, detection) are not automatable.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EffectParameter {
    /// `eq` band gain, -24.0–24.0 dB; peaking and shelf bands; needs `band`.
    GainDb,
    /// `eq` band frequency, 10.0–20000.0 Hz; needs `band`.
    FrequencyHz,
    /// `compressor` threshold, -60.0–0.0 dBFS.
    ThresholdDb,
    /// `compressor` ratio, 1.0–100.0.
    Ratio,
    /// `compressor` makeup gain, -24.0–24.0 dB.
    MakeupDb,
    /// `limiter` ceiling, -24.0–0.0 dBFS.
    CeilingDb,
    /// `delay` feedback, 0.0–95.0 percent.
    FeedbackPercent,
    /// `delay` or `reverb` wet share, 0.0–100.0 percent.
    MixPercent,
    /// `reverb` room size, 0.0–100.0 percent.
    RoomSizePercent,
    /// `reverb` damping, 0.0–100.0 percent.
    DampingPercent,
    /// `snes_echo` EFB, a whole number -128–127.
    Feedback,
    /// `snes_echo` EVOL left, a whole number -128–127.
    VolumeLeft,
    /// `snes_echo` EVOL right, a whole number -128–127.
    VolumeRight,
}

impl EffectParameter {
    /// Whether the parameter is on a ratio scale, where an `exponential`
    /// ramp (equal ratios per time) is meaningful: always positive and not
    /// already logarithmic.
    pub fn ratio_scale(self) -> bool {
        matches!(self, Self::FrequencyHz | Self::Ratio)
    }

    /// The field name, as written in requests.
    pub fn name(self) -> &'static str {
        match self {
            Self::GainDb => "gain_db",
            Self::FrequencyHz => "frequency_hz",
            Self::ThresholdDb => "threshold_db",
            Self::Ratio => "ratio",
            Self::MakeupDb => "makeup_db",
            Self::CeilingDb => "ceiling_db",
            Self::FeedbackPercent => "feedback_percent",
            Self::MixPercent => "mix_percent",
            Self::RoomSizePercent => "room_size_percent",
            Self::DampingPercent => "damping_percent",
            Self::Feedback => "feedback",
            Self::VolumeLeft => "volume_left",
            Self::VolumeRight => "volume_right",
        }
    }

    /// Processor kinds that have this parameter.
    pub fn kinds(self) -> &'static [&'static str] {
        match self {
            Self::GainDb | Self::FrequencyHz => &["eq"],
            Self::ThresholdDb | Self::Ratio | Self::MakeupDb => &["compressor"],
            Self::CeilingDb => &["limiter"],
            Self::FeedbackPercent => &["delay"],
            Self::MixPercent => &["delay", "reverb"],
            Self::RoomSizePercent | Self::DampingPercent => &["reverb"],
            Self::Feedback | Self::VolumeLeft | Self::VolumeRight => &["snes_echo"],
        }
    }

    /// Whether the parameter belongs to one equalizer band.
    pub fn per_band(self) -> bool {
        matches!(self, Self::GainDb | Self::FrequencyHz)
    }

    /// Register-valued parameters take whole numbers only.
    pub fn whole(self) -> bool {
        matches!(self, Self::Feedback | Self::VolumeLeft | Self::VolumeRight)
    }

    /// Lowest and highest value and the unit suffix for messages.
    pub fn range(self) -> (f64, f64, &'static str) {
        match self {
            Self::GainDb => (-24.0, 24.0, " dB"),
            Self::FrequencyHz => (10.0, 20000.0, " Hz"),
            Self::ThresholdDb => (-60.0, 0.0, " dB"),
            Self::Ratio => (1.0, 100.0, ""),
            Self::MakeupDb => (-24.0, 24.0, " dB"),
            Self::CeilingDb => (-24.0, 0.0, " dB"),
            Self::FeedbackPercent => (0.0, 95.0, "%"),
            Self::MixPercent | Self::RoomSizePercent | Self::DampingPercent => (0.0, 100.0, "%"),
            Self::Feedback | Self::VolumeLeft | Self::VolumeRight => (-128.0, 127.0, ""),
        }
    }

    /// The processor's static value of this parameter, which applies before
    /// the first point; `None` when the processor or band lacks it.
    pub fn static_value(self, processor: &Processor, band: Option<u8>) -> Option<f64> {
        let band = |eq: &Equalizer| band.and_then(|b| eq.bands.get(usize::from(b))).copied();
        Some(match (self, processor) {
            (Self::GainDb, Processor::Equalizer(eq)) => {
                let band = band(eq)?;
                if !band.filter.has_gain() {
                    return None;
                }
                band.gain_db.value()
            }
            (Self::FrequencyHz, Processor::Equalizer(eq)) => band(eq)?.frequency_hz.value(),
            (Self::ThresholdDb, Processor::Compressor(c)) => c.threshold_db.value(),
            (Self::Ratio, Processor::Compressor(c)) => c.ratio.value(),
            (Self::MakeupDb, Processor::Compressor(c)) => c.makeup_db.value(),
            (Self::CeilingDb, Processor::Limiter(l)) => l.ceiling_db.value(),
            (Self::FeedbackPercent, Processor::Delay(d)) => d.feedback_percent.value(),
            (Self::MixPercent, Processor::Delay(d)) => d.mix_percent.value(),
            (Self::MixPercent, Processor::Reverb(r)) => r.mix_percent.value(),
            (Self::RoomSizePercent, Processor::Reverb(r)) => r.room_size_percent.value(),
            (Self::DampingPercent, Processor::Reverb(r)) => r.damping_percent.value(),
            (Self::Feedback, Processor::SnesEcho(e)) => f64::from(e.feedback),
            (Self::VolumeLeft, Processor::SnesEcho(e)) => f64::from(e.volume_left),
            (Self::VolumeRight, Processor::SnesEcho(e)) => f64::from(e.volume_right),
            _ => return None,
        })
    }
}

/// A parameter's value at every rendered sample: constant, or one per sample.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Values {
    Constant(f64),
    Varying(Vec<f64>),
}

impl Values {
    pub(crate) fn get(&self, n: usize) -> f64 {
        match self {
            Self::Constant(value) => *value,
            Self::Varying(values) => values.get(n).or(values.last()).copied().unwrap_or(0.0),
        }
    }
}

/// Per-sample values of an effect's automated parameters. Parameters without
/// a lane keep the processor's static value.
#[derive(Debug, Default)]
pub(crate) struct Automation {
    lanes: Vec<(EffectParameter, Option<u8>, Values)>,
}

impl Automation {
    pub(crate) fn push(&mut self, parameter: EffectParameter, band: Option<u8>, values: Values) {
        self.lanes.push((parameter, band, values));
    }

    fn lane(&self, parameter: EffectParameter, band: Option<u8>) -> Option<&Values> {
        self.lanes
            .iter()
            .find(|(p, b, _)| *p == parameter && *b == band)
            .map(|(_, _, values)| values)
    }

    /// The value at sample `n`, or `fallback` without a lane.
    fn value(&self, parameter: EffectParameter, n: usize, fallback: f64) -> f64 {
        self.lane(parameter, None)
            .map_or(fallback, |values| values.get(n))
    }

    /// A register value at sample `n`: the nearest whole number, -128–127.
    fn register(&self, parameter: EffectParameter, n: usize, fallback: i8) -> i32 {
        self.lane(parameter, None)
            .map_or(i32::from(fallback), |values| {
                values.get(n).round().clamp(-128.0, 127.0) as i32
            })
    }
}

fn yes() -> bool {
    true
}

fn is_yes(value: &bool) -> bool {
    *value
}

// ---------------------------------------------------------------------------
// Checks

fn check_tenths(value: Tenths, low: f64, high: f64, unit: &str, report: &mut Report, at: String) {
    if !(low..=high).contains(&value.value()) {
        report.push(
            Code::ValueOutOfRange,
            at,
            format!("value must be {low}–{high}{unit}"),
        );
    }
}

/// Checks chains, settings, sidechain references and their processing order.
pub(crate) fn check_effects(
    composition: &Composition,
    voice_ids: &HashSet<&str>,
    bus_ids: &HashSet<&str>,
    report: &mut Report,
) {
    let mix = &composition.mix;
    let mut targets = HashSet::new();
    // Edges from a keyed strip to its key source, among voices and among buses.
    let mut keyed: BTreeMap<MixTarget, BTreeSet<MixTarget>> = BTreeMap::new();
    for (i, chain) in mix.effects.iter().enumerate() {
        let at = |rest: &str| format!("mix/effects/{i}{rest}");
        let exists = match &chain.target {
            MixTarget::Voice { id } => voice_ids.contains(id.as_str()),
            MixTarget::Bus { id } => bus_ids.contains(id.as_str()),
            MixTarget::Master => true,
        };
        if !exists {
            report.push(
                Code::UnknownReference,
                at("/target"),
                format!("{} does not exist", chain.target),
            );
        }
        if !targets.insert(&chain.target) {
            report.push(
                Code::DuplicateId,
                at("/target"),
                format!("{} has two effect chains", chain.target),
            );
        }
        if chain.effects.is_empty() || chain.effects.len() > MAX_CHAIN_EFFECTS {
            report.push(
                Code::InvalidRange,
                at("/effects"),
                format!("a chain holds 1–{MAX_CHAIN_EFFECTS} effects; remove an empty chain"),
            );
        }
        let mut ids = HashSet::new();
        for (j, effect) in chain.effects.iter().enumerate() {
            let at = |rest: &str| format!("mix/effects/{i}/effects/{j}{rest}");
            if effect.id.is_empty() {
                report.push(
                    Code::InvalidRange,
                    at("/id"),
                    "an effect id must not be empty",
                );
            } else if !ids.insert(effect.id.as_str()) {
                report.push(
                    Code::DuplicateId,
                    at("/id"),
                    format!("effect `{}` appears twice in the chain", effect.id),
                );
            }
            check_processor(composition, &effect.processor, report, &at("/processor"));
            if let Some(source) = effect.processor.sidechain() {
                let source_exists = match source {
                    MixTarget::Voice { id } => voice_ids.contains(id.as_str()),
                    MixTarget::Bus { id } => bus_ids.contains(id.as_str()),
                    MixTarget::Master => false,
                };
                let at = at("/processor/sidechain");
                if !source_exists {
                    report.push(
                        Code::UnknownReference,
                        at,
                        format!(
                            "{source} cannot key a sidechain: it does not exist or is the master"
                        ),
                    );
                } else if source == &chain.target {
                    report.push(
                        Code::InvalidRange,
                        at,
                        "a strip cannot key its own sidechain; omit sidechain to use the input",
                    );
                } else if matches!(chain.target, MixTarget::Voice { .. })
                    && matches!(source, MixTarget::Bus { .. })
                {
                    report.push(
                        Code::InvalidRange,
                        at,
                        "a voice chain can be keyed by another voice only; buses are processed after every voice",
                    );
                } else if std::mem::discriminant(source) == std::mem::discriminant(&chain.target) {
                    keyed
                        .entry(chain.target.clone())
                        .or_default()
                        .insert(source.clone());
                }
            }
        }
    }
    if let Some(target) = first_cycle(&keyed) {
        report.push(
            Code::InvalidRange,
            "mix/effects",
            format!("sidechain keys starting at {target} form a cycle; the processing order must be acyclic"),
        );
    }
}

/// A strip on a cycle of key dependencies, if any.
fn first_cycle(edges: &BTreeMap<MixTarget, BTreeSet<MixTarget>>) -> Option<MixTarget> {
    fn visit<'a>(
        node: &'a MixTarget,
        edges: &'a BTreeMap<MixTarget, BTreeSet<MixTarget>>,
        state: &mut BTreeMap<&'a MixTarget, bool>,
    ) -> bool {
        match state.get(node) {
            Some(done) => return !done,
            None => {
                state.insert(node, false);
            }
        }
        for next in edges.get(node).into_iter().flatten() {
            if visit(next, edges, state) {
                return true;
            }
        }
        state.insert(node, true);
        false
    }
    let mut state = BTreeMap::new();
    edges
        .keys()
        .find(|node| visit(node, edges, &mut state))
        .cloned()
}

fn check_processor(
    composition: &Composition,
    processor: &Processor,
    report: &mut Report,
    at: &str,
) {
    let at = |field: &str| format!("{at}/{field}");
    match processor {
        Processor::Equalizer(eq) => {
            if eq.bands.is_empty() || eq.bands.len() > MAX_EQ_BANDS {
                report.push(
                    Code::InvalidRange,
                    at("bands"),
                    format!("an equalizer has 1–{MAX_EQ_BANDS} bands"),
                );
            }
            for (k, band) in eq.bands.iter().enumerate() {
                let at = |field: &str| at(&format!("bands/{k}/{field}"));
                check_tenths(
                    band.frequency_hz,
                    10.0,
                    20000.0,
                    " Hz",
                    report,
                    at("frequency_hz"),
                );
                check_tenths(band.gain_db, -24.0, 24.0, " dB", report, at("gain_db"));
                if !band.gain_db.is_zero() && !band.filter.has_gain() {
                    report.push(
                        Code::InvalidRange,
                        at("gain_db"),
                        "gain_db applies to peaking and shelf bands only",
                    );
                }
                if !(10..=2400).contains(&band.q.0) {
                    report.push(Code::ValueOutOfRange, at("q"), "value must be 0.1–24");
                }
            }
        }
        Processor::Compressor(c) => {
            check_tenths(
                c.threshold_db,
                -60.0,
                0.0,
                " dB",
                report,
                at("threshold_db"),
            );
            check_tenths(c.ratio, 1.0, 100.0, "", report, at("ratio"));
            check_tenths(c.knee_db, 0.0, 24.0, " dB", report, at("knee_db"));
            check_tenths(c.attack_ms, 0.0, 500.0, " ms", report, at("attack_ms"));
            check_tenths(c.release_ms, 1.0, 5000.0, " ms", report, at("release_ms"));
            check_tenths(c.makeup_db, -24.0, 24.0, " dB", report, at("makeup_db"));
        }
        Processor::Limiter(l) => {
            check_tenths(l.ceiling_db, -24.0, 0.0, " dB", report, at("ceiling_db"));
            check_tenths(l.release_ms, 1.0, 5000.0, " ms", report, at("release_ms"));
            check_tenths(l.lookahead_ms, 0.0, 20.0, " ms", report, at("lookahead_ms"));
        }
        Processor::Delay(d) => {
            match d.time {
                DelayTime::Ms { ms } => {
                    check_tenths(ms, 1.0, MAX_DELAY_MS, " ms", report, at("time/ms"))
                }
                DelayTime::Note {
                    numerator,
                    denominator,
                } => {
                    if !(1..=64).contains(&numerator) || !(1..=64).contains(&denominator) {
                        report.push(
                            Code::ValueOutOfRange,
                            at("time"),
                            "numerator and denominator must be 1–64",
                        );
                    } else if composition.tempo_map.is_empty() {
                        report.push(
                            Code::MusicalTimingInEffect,
                            at("time"),
                            "a note-synced delay needs a tempo map; use ms in effect projects",
                        );
                    } else if let Some(slowest) = composition
                        .tempo_map
                        .iter()
                        .map(|t| t.microseconds_per_quarter)
                        .max()
                        && note_ms(numerator, denominator, slowest) > MAX_DELAY_MS
                    {
                        report.push(
                            Code::ValueOutOfRange,
                            at("time"),
                            format!(
                                "the note length exceeds {MAX_DELAY_MS} ms at the slowest tempo"
                            ),
                        );
                    }
                }
            }
            check_tenths(
                d.feedback_percent,
                0.0,
                95.0,
                "%",
                report,
                at("feedback_percent"),
            );
            check_tenths(d.mix_percent, 0.0, 100.0, "%", report, at("mix_percent"));
        }
        Processor::Reverb(r) => {
            check_tenths(
                r.room_size_percent,
                0.0,
                100.0,
                "%",
                report,
                at("room_size_percent"),
            );
            check_tenths(
                r.damping_percent,
                0.0,
                100.0,
                "%",
                report,
                at("damping_percent"),
            );
            check_tenths(
                r.pre_delay_ms,
                0.0,
                250.0,
                " ms",
                report,
                at("pre_delay_ms"),
            );
            check_tenths(r.mix_percent, 0.0, 100.0, "%", report, at("mix_percent"));
        }
        Processor::SnesEcho(e) => {
            if e.delay > 15 {
                report.push(Code::ValueOutOfRange, at("delay"), "EDL must be 0–15");
            }
        }
    }
}

fn note_ms(numerator: u16, denominator: u16, microseconds_per_quarter: u32) -> f64 {
    f64::from(numerator) / f64::from(denominator) * 4.0 * f64::from(microseconds_per_quarter)
        / 1000.0
}

// ---------------------------------------------------------------------------
// Edit operations

/// Applies one effect operation of an edit request.
pub(crate) fn apply_effect_operation(
    composition: &mut Composition,
    operation: &EditOperation,
    report: &mut Report,
) {
    let target = match operation {
        EditOperation::SetEffects { target, .. }
        | EditOperation::InsertEffect { target, .. }
        | EditOperation::RemoveEffect { target, .. }
        | EditOperation::MoveEffect { target, .. }
        | EditOperation::SetEffect { target, .. } => target,
        _ => unreachable!("only effect operations are delegated here"),
    };
    let exists = match target {
        MixTarget::Voice { id } => composition.voices.iter().any(|v| &v.id == id),
        MixTarget::Bus { id } => composition.mix.buses.iter().any(|b| &b.id == id),
        MixTarget::Master => true,
    };
    if !exists {
        let hint = match target {
            MixTarget::Bus { .. } => "; add it with add_bus",
            _ => "",
        };
        report.push(
            Code::UnknownReference,
            "target",
            format!("{target} does not exist{hint}"),
        );
        return;
    }
    let chains = &mut composition.mix.effects;
    let position = chains.iter().position(|c| &c.target == target);
    if let EditOperation::SetEffects { effects, .. } = operation {
        match (position, effects.is_empty()) {
            (Some(index), true) => {
                chains.remove(index);
            }
            (Some(index), false) => chains[index].effects = effects.clone(),
            (None, true) => {}
            (None, false) => chains.push(EffectChain {
                target: target.clone(),
                effects: effects.clone(),
            }),
        }
        return;
    }
    if let EditOperation::InsertEffect { effect, index, .. } = operation {
        let chain = match position {
            Some(position) => &mut chains[position],
            None => {
                chains.push(EffectChain {
                    target: target.clone(),
                    effects: Vec::new(),
                });
                chains.last_mut().expect("just pushed")
            }
        };
        if chain.effects.iter().any(|e| e.id == effect.id) {
            report.push(
                Code::DuplicateId,
                "effect/id",
                format!("effect `{}` already exists in the chain", effect.id),
            );
            return;
        }
        let index = index.unwrap_or(chain.effects.len());
        if index > chain.effects.len() {
            report.push(
                Code::InvalidRange,
                "index",
                format!("index must be 0–{}", chain.effects.len()),
            );
            return;
        }
        chain.effects.insert(index, effect.clone());
        return;
    }
    let id = match operation {
        EditOperation::RemoveEffect { id, .. }
        | EditOperation::MoveEffect { id, .. }
        | EditOperation::SetEffect { id, .. } => id,
        _ => unreachable!("handled above"),
    };
    let found = position.and_then(|p| {
        chains[p]
            .effects
            .iter()
            .position(|e| &e.id == id)
            .map(|e| (p, e))
    });
    let Some((chain_index, effect_index)) = found else {
        report.push(
            Code::UnknownReference,
            "id",
            format!("{target} has no effect `{id}`"),
        );
        return;
    };
    let chain = &mut chains[chain_index];
    match operation {
        EditOperation::RemoveEffect { .. } => {
            chain.effects.remove(effect_index);
            if chain.effects.is_empty() {
                chains.remove(chain_index);
            }
        }
        EditOperation::MoveEffect { index, .. } => {
            if *index >= chain.effects.len() {
                report.push(
                    Code::InvalidRange,
                    "index",
                    format!("index must be 0–{}", chain.effects.len() - 1),
                );
                return;
            }
            let effect = chain.effects.remove(effect_index);
            chain.effects.insert(*index, effect);
        }
        EditOperation::SetEffect {
            processor, bypass, ..
        } => {
            if processor.is_none() && bypass.is_none() {
                report.push(Code::EmptyEdit, "", "set processor or bypass");
                return;
            }
            let effect = &mut chain.effects[effect_index];
            if let Some(processor) = processor {
                if processor.kind() != effect.processor.kind() {
                    report.push(
                        Code::InvalidRange,
                        "processor/kind",
                        format!(
                            "effect `{id}` is a {}; use remove_effect and insert_effect to change the kind",
                            effect.processor.kind()
                        ),
                    );
                    return;
                }
                effect.processor = processor.clone();
            }
            effect.bypass = bypass.unwrap_or(effect.bypass);
        }
        _ => unreachable!("handled above"),
    }
}

/// Removes a bus's chain together with the bus.
pub(crate) fn remove_chain(effects: &mut Vec<EffectChain>, target: &MixTarget) {
    effects.retain(|c| &c.target != target);
}

// ---------------------------------------------------------------------------
// Processing

/// Render-wide facts a processor needs.
pub(crate) struct EffectContext<'a> {
    pub sample_rate: u32,
    /// `(first sample, microseconds per quarter)` of each tempo stretch in
    /// render order, empty without a tempo map.
    pub tempo: &'a [(usize, u32)],
}

/// The signal that drives a dynamics processor's detector.
#[derive(Clone, Copy)]
pub(crate) enum Key<'a> {
    /// The processor's own input.
    Own,
    /// A declared sidechain source that is not heard in this render.
    Silent,
    External(&'a [Vec<f32>; 2]),
}

/// Processes a stereo signal in place with static parameters.
#[cfg(test)]
pub(crate) fn process(
    processor: &Processor,
    signal: &mut [Vec<f32>; 2],
    key: Key,
    context: &EffectContext,
) {
    process_automated(processor, signal, key, context, &Automation::default());
}

/// Processes a stereo signal in place, reading automated parameters at
/// every sample. Without lanes this is the static processor exactly.
pub(crate) fn process_automated(
    processor: &Processor,
    signal: &mut [Vec<f32>; 2],
    key: Key,
    context: &EffectContext,
    automation: &Automation,
) {
    let rate = f64::from(context.sample_rate);
    match processor {
        Processor::Equalizer(eq) => {
            let filters: Vec<Biquad> = eq
                .bands
                .iter()
                .map(|b| Biquad::design(b, context.sample_rate))
                .collect();
            for side in signal.iter_mut() {
                let mut x: Vec<f64> = side.iter().map(|s| f64::from(*s)).collect();
                for (k, (filter, band)) in filters.iter().zip(&eq.bands).enumerate() {
                    let band_index = u8::try_from(k).ok();
                    let frequency = automation.lane(EffectParameter::FrequencyHz, band_index);
                    let gain = automation.lane(EffectParameter::GainDb, band_index);
                    if frequency.is_none() && gain.is_none() {
                        filter.run(&mut x);
                    } else {
                        Biquad::run_automated(band, frequency, gain, context.sample_rate, &mut x);
                    }
                }
                write_back(side, &x);
            }
        }
        Processor::Compressor(c) => {
            let gains = compressor_gains(c, signal, key, rate, automation);
            for side in signal.iter_mut() {
                for (s, g) in side.iter_mut().zip(&gains) {
                    *s = (f64::from(*s) * g) as f32;
                }
            }
        }
        Processor::Limiter(l) => {
            limit(l, signal, rate, automation);
        }
        Processor::Delay(d) => delay(d, signal, context, automation),
        Processor::Reverb(r) => reverb(r, signal, rate, automation),
        Processor::SnesEcho(e) => snes_echo(e, signal, rate, automation),
    }
}

/// [`process_automated`] on `signal`, also processing `shares`, parts of
/// `signal` such as stems, so that their sum follows the processed signal.
/// The equalizer, delay and reverb are linear, so each share runs through
/// its own instance. A compressor or limiter applies the gain it computed
/// from `signal` (and its key) to every share, so their sum is the processed
/// signal up to rounding and the limiter's final clamp. The SNES echo
/// quantizes, wraps and clamps, so its shares only approximate the processed
/// signal; the result is `false` then. A share that is the whole signal
/// takes the processed signal and a silent share stays silent, both exactly,
/// so an effect only one stem reaches is never approximated. `signal` is
/// processed exactly as without shares.
pub(crate) fn process_shared(
    processor: &Processor,
    signal: &mut [Vec<f32>; 2],
    key: Key,
    context: &EffectContext,
    automation: &Automation,
    shares: &mut [&mut [Vec<f32>; 2]],
) -> bool {
    let whole: Vec<bool> = shares.iter().map(|share| **share == *signal).collect();
    let silent: Vec<bool> = shares
        .iter()
        .map(|share| share.iter().flatten().all(|x| *x == 0.0))
        .collect();
    let mut split: Vec<&mut &mut [Vec<f32>; 2]> = Vec::new();
    let mut copies: Vec<&mut &mut [Vec<f32>; 2]> = Vec::new();
    for (i, share) in shares.iter_mut().enumerate() {
        if whole[i] {
            copies.push(share);
        } else if !silent[i] {
            split.push(share);
        }
    }
    let exact = split_shared(processor, signal, key, context, automation, &mut split);
    for share in copies {
        **share = signal.clone();
    }
    exact
}

/// [`process_shared`] for shares that are neither the whole signal nor silent.
fn split_shared(
    processor: &Processor,
    signal: &mut [Vec<f32>; 2],
    key: Key,
    context: &EffectContext,
    automation: &Automation,
    shares: &mut [&mut &mut [Vec<f32>; 2]],
) -> bool {
    let rate = f64::from(context.sample_rate);
    let gains = match processor {
        Processor::Compressor(c) => {
            let gains = compressor_gains(c, signal, key, rate, automation);
            for side in signal.iter_mut() {
                for (s, g) in side.iter_mut().zip(&gains) {
                    *s = (f64::from(*s) * g) as f32;
                }
            }
            Some(gains)
        }
        Processor::Limiter(l) => Some(limit(l, signal, rate, automation)),
        _ => None,
    };
    if let Some(gains) = gains {
        for share in shares.iter_mut() {
            for side in share.iter_mut() {
                for (s, g) in side.iter_mut().zip(&gains) {
                    *s = (f64::from(*s) * g) as f32;
                }
            }
        }
        return true;
    }
    process_automated(processor, signal, key, context, automation);
    for share in shares.iter_mut() {
        process_automated(processor, share, Key::Own, context, automation);
    }
    shares.is_empty() || !matches!(processor, Processor::SnesEcho(_))
}

fn write_back(side: &mut [f32], x: &[f64]) {
    for (s, v) in side.iter_mut().zip(x) {
        *s = *v as f32;
    }
}

/// Samples of silence after which the processor's output stays silent, for
/// the tail cut: the longest path through its delay lines.
pub(crate) fn hold_samples(processor: &Processor, context: &EffectContext) -> usize {
    let rate = f64::from(context.sample_rate);
    match processor {
        Processor::Delay(d) => delay_lengths(d.time, context)
            .iter()
            .map(|(_, length)| *length)
            .max()
            .unwrap_or(0),
        Processor::Reverb(r) => {
            let scale = |n: usize| ((n as f64) * rate / FREEVERB_RATE).round() as usize;
            ms_samples(r.pre_delay_ms.value(), rate)
                + scale(COMBS[COMBS.len() - 1] + STEREO_SPREAD)
                + ALLPASSES.iter().map(|n| scale(*n)).sum::<usize>()
        }
        Processor::SnesEcho(e) => echo_length(e.delay, rate) + 8,
        _ => 0,
    }
}

fn ms_samples(ms: f64, rate: f64) -> usize {
    (ms * rate / 1000.0).round() as usize
}

fn time_constant(ms: f64, rate: f64) -> f64 {
    if ms <= 0.0 {
        0.0
    } else {
        (-1.0 / (ms / 1000.0 * rate)).exp()
    }
}

fn db_to_gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

// --- Equalizer --------------------------------------------------------------

/// Biquad coefficients normalized by a0.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
}

impl Biquad {
    /// RBJ Audio EQ Cookbook designs.
    pub(crate) fn design(band: &EqBand, sample_rate: u32) -> Self {
        Self::design_values(
            band.filter,
            band.frequency_hz.value(),
            band.gain_db.value(),
            band.q.value(),
            sample_rate,
        )
    }

    /// The design for explicit frequency, gain and q values.
    fn design_values(
        filter: FilterKind,
        frequency_hz: f64,
        gain_db: f64,
        q: f64,
        sample_rate: u32,
    ) -> Self {
        let rate = f64::from(sample_rate);
        let frequency = frequency_hz.min(MAX_BAND_FRACTION * rate);
        let w0 = 2.0 * std::f64::consts::PI * frequency / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = 10f64.powf(gain_db / 40.0);
        let root = 2.0 * a.sqrt() * alpha;
        let (b, a0, a1, a2) = match filter {
            FilterKind::LowPass => (
                [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            FilterKind::HighPass => (
                [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            FilterKind::BandPass => ([alpha, 0.0, -alpha], 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Notch => ([1.0, -2.0 * cos, 1.0], 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Peaking => (
                [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a],
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            FilterKind::LowShelf => (
                [
                    a * ((a + 1.0) - (a - 1.0) * cos + root),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - root),
                ],
                (a + 1.0) + (a - 1.0) * cos + root,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - root,
            ),
            FilterKind::HighShelf => (
                [
                    a * ((a + 1.0) + (a - 1.0) * cos + root),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - root),
                ],
                (a + 1.0) - (a - 1.0) * cos + root,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - root,
            ),
        };
        Self {
            b: [b[0] / a0, b[1] / a0, b[2] / a0],
            a: [a1 / a0, a2 / a0],
        }
    }

    /// Transposed direct form II, in place.
    fn run(&self, x: &mut [f64]) {
        let (mut z1, mut z2) = (0.0, 0.0);
        for sample in x.iter_mut() {
            (*sample, z1, z2) = self.step(*sample, z1, z2);
        }
    }

    /// One transposed direct form II sample: output and the next state.
    fn step(&self, input: f64, z1: f64, z2: f64) -> (f64, f64, f64) {
        let output = self.b[0] * input + z1;
        (
            output,
            self.b[1] * input - self.a[0] * output + z2,
            self.b[2] * input - self.a[1] * output,
        )
    }

    /// Runs a band whose frequency or gain is automated: the coefficients are
    /// redesigned at every sample whose values differ from the previous
    /// sample's, and the filter state carries over unchanged.
    fn run_automated(
        band: &EqBand,
        frequency: Option<&Values>,
        gain: Option<&Values>,
        sample_rate: u32,
        x: &mut [f64],
    ) {
        let (mut z1, mut z2) = (0.0, 0.0);
        let mut current: Option<((f64, f64), Self)> = None;
        for (n, sample) in x.iter_mut().enumerate() {
            let values = (
                frequency.map_or(band.frequency_hz.value(), |v| v.get(n)),
                gain.map_or(band.gain_db.value(), |v| v.get(n)),
            );
            let filter = match &current {
                Some((held, filter)) if *held == values => *filter,
                _ => {
                    let filter = Self::design_values(
                        band.filter,
                        values.0,
                        values.1,
                        band.q.value(),
                        sample_rate,
                    );
                    current = Some((values, filter));
                    filter
                }
            };
            (*sample, z1, z2) = filter.step(*sample, z1, z2);
        }
    }

    /// |H(e^jw)| at `frequency`, evaluated from the coefficients.
    #[cfg(test)]
    fn magnitude(&self, frequency: f64, sample_rate: u32) -> f64 {
        let w = 2.0 * std::f64::consts::PI * frequency / f64::from(sample_rate);
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num = (
            self.b[0] + self.b[1] * c1 + self.b[2] * c2,
            -(self.b[1] * s1 + self.b[2] * s2),
        );
        let den = (
            1.0 + self.a[0] * c1 + self.a[1] * c2,
            -(self.a[0] * s1 + self.a[1] * s2),
        );
        ((num.0 * num.0 + num.1 * num.1) / (den.0 * den.0 + den.1 * den.1)).sqrt()
    }
}

// --- Compressor -------------------------------------------------------------

/// Output level in dB of the static curve for input level `x` dB, with a
/// quadratic soft knee of width `knee` centered on `threshold`.
pub(crate) fn static_curve(x: f64, threshold: f64, ratio: f64, knee: f64) -> f64 {
    let over = x - threshold;
    if 2.0 * over < -knee {
        x
    } else if knee > 0.0 && 2.0 * over.abs() <= knee {
        x + (1.0 / ratio - 1.0) * (over + knee / 2.0).powi(2) / (2.0 * knee)
    } else {
        threshold + over / ratio
    }
}

/// Linear gain per sample: the static curve's reduction, smoothed in dB with
/// branching attack and release (Giannoulis, Massberg and Reiss 2012), plus makeup.
fn compressor_gains(
    c: &Compressor,
    signal: &[Vec<f32>; 2],
    key: Key,
    rate: f64,
    automation: &Automation,
) -> Vec<f64> {
    let length = signal[0].len();
    let source = match key {
        Key::Own => Some(signal),
        Key::External(key) => Some(key),
        Key::Silent => None,
    };
    let sample = |side: usize, n: usize| -> f64 {
        source
            .and_then(|s| s[side].get(n))
            .map_or(0.0, |v| f64::from(*v))
    };
    let window = ms_samples(RMS_WINDOW_MS, rate).max(1);
    let (attack, release) = (
        time_constant(c.attack_ms.value(), rate),
        time_constant(c.release_ms.value(), rate),
    );
    let knee = c.knee_db.value();
    let mut gains = Vec::with_capacity(length);
    let mut squares = 0.0f64;
    let mut smoothed = 0.0f64;
    for n in 0..length {
        let (threshold, ratio, makeup) = (
            automation.value(EffectParameter::ThresholdDb, n, c.threshold_db.value()),
            automation.value(EffectParameter::Ratio, n, c.ratio.value()),
            automation.value(EffectParameter::MakeupDb, n, c.makeup_db.value()),
        );
        let level = match c.detection {
            Detection::Peak => sample(0, n).abs().max(sample(1, n).abs()),
            Detection::Rms => {
                squares += (sample(0, n).powi(2) + sample(1, n).powi(2)) / 2.0;
                if n >= window {
                    squares -=
                        (sample(0, n - window).powi(2) + sample(1, n - window).powi(2)) / 2.0;
                }
                (squares.max(0.0) / window as f64).sqrt()
            }
        };
        let x = 20.0 * level.max(1e-10).log10();
        let reduction = x - static_curve(x, threshold, ratio, knee);
        let coefficient = if reduction > smoothed {
            attack
        } else {
            release
        };
        smoothed = coefficient * smoothed + (1.0 - coefficient) * reduction;
        gains.push(db_to_gain(makeup - smoothed));
    }
    gains
}

// --- Limiter ----------------------------------------------------------------

/// Stereo-linked look-ahead limiting. The gain at each sample is a moving
/// average over the look-ahead window of a release-smoothed minimum that is
/// itself at most the gain each sample in the window needs, so no output
/// sample exceeds the ceiling in effect at that sample. A final clamp absorbs
/// rounding. Returns the gain of every sample, before the clamp.
fn limit(l: &Limiter, signal: &mut [Vec<f32>; 2], rate: f64, automation: &Automation) -> Vec<f64> {
    let length = signal[0].len();
    let fixed = db_to_gain(l.ceiling_db.value());
    let lane = automation.lane(EffectParameter::CeilingDb, None);
    let ceiling_at = |n: usize| lane.map_or(fixed, |values| db_to_gain(values.get(n)));
    let window = ms_samples(l.lookahead_ms.value(), rate).max(1);
    let needed: Vec<f64> = (0..length)
        .map(|n| {
            let ceiling = ceiling_at(n);
            let peak = f64::from(signal[0][n].abs()).max(f64::from(signal[1][n].abs()));
            if peak > ceiling { ceiling / peak } else { 1.0 }
        })
        .collect();
    // Minimum of `needed` over [n, n + window), with a monotonic queue.
    let mut held = vec![1.0; length];
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for n in (0..length).rev() {
        while queue.back().is_some_and(|&k| needed[k] >= needed[n]) {
            queue.pop_back();
        }
        queue.push_back(n);
        while queue.front().is_some_and(|&k| k >= n + window) {
            queue.pop_front();
        }
        held[n] = needed[*queue.front().expect("holds n")];
    }
    let recovery = 1.0 - time_constant(l.release_ms.value(), rate);
    let mut previous = 1.0f64;
    for value in held.iter_mut() {
        previous = (previous + (1.0 - previous) * recovery).min(*value);
        *value = previous;
    }
    let limit32 = |ceiling: f64| {
        let limit = ceiling as f32;
        if f64::from(limit) > ceiling {
            limit.next_down()
        } else {
            limit
        }
    };
    let fixed32 = limit32(fixed);
    let mut sum = window as f64;
    let mut gains = Vec::with_capacity(length);
    for n in 0..length {
        sum += held[n] - if n >= window { held[n - window] } else { 1.0 };
        let gain = sum / window as f64;
        let limit = if lane.is_some() {
            limit32(ceiling_at(n))
        } else {
            fixed32
        };
        for side in signal.iter_mut() {
            let out = (f64::from(side[n]) * gain) as f32;
            side[n] = out.clamp(-limit, limit);
        }
        gains.push(gain);
    }
    gains
}

// --- Delay ------------------------------------------------------------------

/// `(first sample, delay in samples)` stretches of a delay time.
fn delay_lengths(time: DelayTime, context: &EffectContext) -> Vec<(usize, usize)> {
    let rate = f64::from(context.sample_rate);
    match time {
        DelayTime::Ms { ms } => vec![(0, ms_samples(ms.value(), rate).max(1))],
        DelayTime::Note {
            numerator,
            denominator,
        } => {
            let length =
                |tempo: u32| ms_samples(note_ms(numerator, denominator, tempo), rate).max(1);
            if context.tempo.is_empty() {
                return vec![(0, length(500_000))];
            }
            context
                .tempo
                .iter()
                .map(|(start, tempo)| (*start, length(*tempo)))
                .collect()
        }
    }
}

/// Feedback delay: the line holds input plus feedback × its output. A
/// note-synced length follows the tempo stretch of each output sample.
fn delay(d: &Delay, signal: &mut [Vec<f32>; 2], context: &EffectContext, automation: &Automation) {
    let length = signal[0].len();
    let lengths = delay_lengths(d.time, context);
    let feedback_at = |n: usize| {
        automation.value(
            EffectParameter::FeedbackPercent,
            n,
            d.feedback_percent.value(),
        ) / 100.0
    };
    let mix_at =
        |n: usize| automation.value(EffectParameter::MixPercent, n, d.mix_percent.value()) / 100.0;
    let mut stretch = 0;
    let mut current = |n: usize| {
        while stretch + 1 < lengths.len() && lengths[stretch + 1].0 <= n {
            stretch += 1;
        }
        lengths[stretch].1
    };
    let dry: [Vec<f64>; 2] = [0, 1].map(|s| signal[s].iter().map(|v| f64::from(*v)).collect());
    let mut line = [vec![0.0f64; length], vec![0.0f64; length]];
    let mut wet = [vec![0.0f64; length], vec![0.0f64; length]];
    for n in 0..length {
        let delay = current(n);
        let feedback = feedback_at(n);
        for side in 0..2 {
            wet[side][n] = if n >= delay {
                line[side][n - delay]
            } else {
                0.0
            };
        }
        if d.ping_pong {
            line[0][n] = (dry[0][n] + dry[1][n]) / 2.0 + feedback * wet[1][n];
            line[1][n] = feedback * wet[0][n];
        } else {
            for side in 0..2 {
                line[side][n] = dry[side][n] + feedback * wet[side][n];
            }
        }
    }
    for side in 0..2 {
        for n in 0..length {
            let mix = mix_at(n);
            signal[side][n] = ((1.0 - mix) * dry[side][n] + mix * wet[side][n]) as f32;
        }
    }
}

// --- Reverb -----------------------------------------------------------------

/// Freeverb tunings in samples at 44.1 kHz (Jezar, 2000), scaled to the render rate.
const FREEVERB_RATE: f64 = 44_100.0;
const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;
const INPUT_GAIN: f64 = 0.015;
const ALLPASS_FEEDBACK: f64 = 0.5;

struct Comb {
    buffer: Vec<f64>,
    index: usize,
    store: f64,
}

impl Comb {
    fn new(length: usize) -> Self {
        Self {
            buffer: vec![0.0; length.max(1)],
            index: 0,
            store: 0.0,
        }
    }

    /// Feedback comb with a one-pole lowpass in the loop.
    fn step(&mut self, input: f64, feedback: f64, damp: f64) -> f64 {
        let output = self.buffer[self.index];
        self.store = output * (1.0 - damp) + self.store * damp;
        self.buffer[self.index] = input + self.store * feedback;
        self.index = (self.index + 1) % self.buffer.len();
        output
    }
}

struct Allpass {
    buffer: Vec<f64>,
    index: usize,
}

impl Allpass {
    fn new(length: usize) -> Self {
        Self {
            buffer: vec![0.0; length.max(1)],
            index: 0,
        }
    }

    /// Freeverb's Schroeder allpass approximation.
    fn step(&mut self, input: f64) -> f64 {
        let delayed = self.buffer[self.index];
        self.buffer[self.index] = input + delayed * ALLPASS_FEEDBACK;
        self.index = (self.index + 1) % self.buffer.len();
        delayed - input
    }
}

/// Freeverb's tank, one sample at a time: eight parallel damped combs into
/// four series allpasses per side, the right side's lines 23 samples (at
/// 44.1 kHz) longer, fed by a mono input scaled by 0.015. The mixer reverb
/// and the reference synthesizer's CC91 reverb share it.
pub(crate) struct ReverbTank {
    banks: [(Vec<Comb>, Vec<Allpass>); 2],
    feedback: f64,
    damp: f64,
}

impl ReverbTank {
    /// Room size and damping in percent, 0–100.
    pub(crate) fn new(room_size_percent: f64, damping_percent: f64, rate: f64) -> Self {
        let scale = |n: usize| ((n as f64) * rate / FREEVERB_RATE).round() as usize;
        let bank = |spread: usize| {
            (
                COMBS.iter().map(|n| Comb::new(scale(n + spread))).collect(),
                ALLPASSES
                    .iter()
                    .map(|n| Allpass::new(scale(n + spread)))
                    .collect(),
            )
        };
        let mut tank = Self {
            banks: [bank(0), bank(STEREO_SPREAD)],
            feedback: 0.0,
            damp: 0.0,
        };
        tank.set(room_size_percent, damping_percent);
        tank
    }

    /// Changes room size and damping, in percent, keeping the lines' contents.
    fn set(&mut self, room_size_percent: f64, damping_percent: f64) {
        self.feedback = 0.7 + 0.28 * room_size_percent / 100.0;
        self.damp = 0.4 * damping_percent / 100.0;
    }

    /// Wet output of both sides for one unscaled mono input sample.
    pub(crate) fn process(&mut self, input: f64) -> [f64; 2] {
        let input = input * INPUT_GAIN;
        let mut out = [0.0; 2];
        for (side, (combs, allpasses)) in self.banks.iter_mut().enumerate() {
            let mut sum = 0.0;
            for comb in combs.iter_mut() {
                sum += comb.step(input, self.feedback, self.damp);
            }
            for allpass in allpasses.iter_mut() {
                sum = allpass.step(sum);
            }
            out[side] = sum;
        }
        out
    }
}

/// The tank fed by the mono sum after the pre-delay, mixed with the dry
/// signal.
fn reverb(r: &Reverb, signal: &mut [Vec<f32>; 2], rate: f64, automation: &Automation) {
    let length = signal[0].len();
    let shape = automation
        .lane(EffectParameter::RoomSizePercent, None)
        .is_some()
        || automation
            .lane(EffectParameter::DampingPercent, None)
            .is_some();
    let pre_delay = ms_samples(r.pre_delay_ms.value(), rate);
    let mut tank = ReverbTank::new(r.room_size_percent.value(), r.damping_percent.value(), rate);
    let input: Vec<f64> = (0..length)
        .map(|n| {
            if n < pre_delay {
                0.0
            } else {
                f64::from(signal[0][n - pre_delay]) + f64::from(signal[1][n - pre_delay])
            }
        })
        .collect();
    for (n, input) in input.into_iter().enumerate() {
        if shape {
            tank.set(
                automation.value(
                    EffectParameter::RoomSizePercent,
                    n,
                    r.room_size_percent.value(),
                ),
                automation.value(
                    EffectParameter::DampingPercent,
                    n,
                    r.damping_percent.value(),
                ),
            );
        }
        let mix = automation.value(EffectParameter::MixPercent, n, r.mix_percent.value()) / 100.0;
        let wet = tank.process(input);
        for (side, wet) in wet.into_iter().enumerate() {
            let dry = f64::from(signal[side][n]);
            signal[side][n] = ((1.0 - mix) * dry + mix * wet) as f32;
        }
    }
}

// --- S-DSP echo -------------------------------------------------------------

/// Echo buffer length in samples: EDL × 16 ms, or one sample for EDL 0.
fn echo_length(delay: u8, rate: f64) -> usize {
    if delay == 0 {
        1
    } else {
        ((f64::from(delay) * 0.016 * rate).round() as usize).max(1)
    }
}

/// Signal value of PCM16 code 1 is 1 / 32767, the scale renders write with.
const PCM16_SCALE: f64 = 32767.0;

fn clamp16(value: i32) -> i32 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX))
}

/// Wraps to 16 bits like a C `(int16_t)` cast.
fn wrap16(value: i32) -> i32 {
    i32::from(value as i16)
}

/// The S-DSP echo path in its integer arithmetic, per side: read the buffer sample written one
/// buffer length ago, keep it halved in an 8-sample history, sum
/// `(history × C) >> 6` over the 7 oldest taps with 16-bit wraparound, add
/// the newest tap wrapped to 16 bits, clamp and clear bit 0, output
/// `clamp16((echo × EVOL) >> 7)` and write
/// `clamp16(input + wrap16((echo × EFB) >> 7))` with bit 0 cleared. Signal
/// values convert to and from 16-bit codes at the render's PCM16 scale
/// (32767), so a PCM16 code enters and leaves the echo unchanged; the dry
/// signal is not quantized.
fn snes_echo(e: &SnesEcho, signal: &mut [Vec<f32>; 2], rate: f64, automation: &Automation) {
    let buffer_length = echo_length(e.delay, rate);
    let volumes = [
        (EffectParameter::VolumeLeft, e.volume_left),
        (EffectParameter::VolumeRight, e.volume_right),
    ];
    for (side, (parameter, volume)) in signal.iter_mut().zip(volumes) {
        let mut buffer = vec![0i32; buffer_length];
        let mut history = [0i32; 8];
        for (n, sample) in side.iter_mut().enumerate() {
            let position = n % buffer_length;
            history.rotate_left(1);
            history[7] = buffer[position] >> 1;
            let mut sum = 0i32;
            for (tap, coefficient) in history.iter().zip(e.fir).take(7) {
                sum += (tap * i32::from(coefficient)) >> 6;
            }
            let newest = wrap16((history[7] * i32::from(e.fir[7])) >> 6);
            let echo = clamp16(wrap16(sum) + newest) & !1;
            let input = clamp16((f64::from(*sample) * PCM16_SCALE).round() as i32);
            let feedback = automation.register(EffectParameter::Feedback, n, e.feedback);
            buffer[position] = clamp16(input + wrap16((echo * feedback) >> 7)) & !1;
            let volume = automation.register(parameter, n, volume);
            let output = f64::from(clamp16((echo * volume) >> 7)) / PCM16_SCALE;
            let dry = if e.dry { f64::from(*sample) } else { 0.0 };
            *sample = (dry + output) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn band(filter: FilterKind, frequency: f64, gain: f64, q: f64) -> EqBand {
        EqBand {
            filter,
            frequency_hz: Tenths((frequency * 10.0).round() as i32),
            gain_db: Tenths((gain * 10.0).round() as i32),
            q: Hundredths((q * 100.0).round() as i32),
        }
    }

    fn context() -> EffectContext<'static> {
        EffectContext {
            sample_rate: RATE,
            tempo: &[],
        }
    }

    fn stereo(samples: Vec<f32>) -> [Vec<f32>; 2] {
        [samples.clone(), samples]
    }

    /// Steady-state gain of a band at `frequency`, measured with a sine.
    fn measured_gain(band: EqBand, frequency: f64) -> f64 {
        let seconds = 1.0;
        let length = (f64::from(RATE) * seconds) as usize;
        let sine: Vec<f32> = (0..length)
            .map(|n| {
                (0.5 * (2.0 * std::f64::consts::PI * frequency * n as f64 / f64::from(RATE)).sin())
                    as f32
            })
            .collect();
        let mut signal = stereo(sine.clone());
        process(
            &Processor::Equalizer(Equalizer { bands: vec![band] }),
            &mut signal,
            Key::Own,
            &context(),
        );
        let rms = |x: &[f32]| {
            (x.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
        };
        let tail = length / 2..length;
        rms(&signal[0][tail.clone()]) / rms(&sine[tail])
    }

    fn db(gain: f64) -> f64 {
        20.0 * gain.log10()
    }

    #[test]
    fn biquads_match_the_cookbook_responses() {
        let q = 1.0 / 2f64.sqrt();
        // (band, frequency, expected dB): properties of the cookbook designs.
        let cases = [
            (band(FilterKind::Peaking, 1000.0, 6.0, 1.0), 1000.0, 6.0),
            (band(FilterKind::Peaking, 1000.0, -9.0, 2.0), 1000.0, -9.0),
            (band(FilterKind::Peaking, 1000.0, 6.0, 4.0), 20.0, 0.0),
            (band(FilterKind::LowShelf, 300.0, 6.0, q), 300.0, 3.0),
            (band(FilterKind::LowShelf, 300.0, 6.0, q), 10.0, 6.0),
            (band(FilterKind::HighShelf, 3000.0, -6.0, q), 3000.0, -3.0),
            (band(FilterKind::HighShelf, 3000.0, -6.0, q), 20.0, 0.0),
            (band(FilterKind::LowPass, 1000.0, 0.0, q), 1000.0, db(0.71)),
            (band(FilterKind::LowPass, 1000.0, 0.0, 2.0), 1000.0, db(2.0)),
            (band(FilterKind::HighPass, 500.0, 0.0, q), 500.0, db(0.71)),
            (band(FilterKind::BandPass, 2000.0, 0.0, 3.0), 2000.0, 0.0),
        ];
        for (band, frequency, expected) in cases {
            let filter = Biquad::design(&band, RATE);
            let analytic = db(filter.magnitude(frequency, RATE));
            assert!(
                (analytic - expected).abs() < 0.02,
                "{band:?} at {frequency} Hz: {analytic} dB instead of {expected}"
            );
            let measured = db(measured_gain(band, frequency));
            assert!(
                (measured - analytic).abs() < 0.02,
                "{band:?} at {frequency} Hz: measured {measured} dB, response {analytic} dB"
            );
        }
        // A notch removes its center frequency.
        let notch = band(FilterKind::Notch, 1000.0, 0.0, 1.0);
        assert!(Biquad::design(&notch, RATE).magnitude(1000.0, RATE) < 1e-9);
        assert!(measured_gain(notch, 1000.0) < 1e-3);
        // Low-pass: -12 dB/octave far above the corner.
        let low_pass = Biquad::design(&band(FilterKind::LowPass, 500.0, 0.0, q), RATE);
        let slope = db(low_pass.magnitude(4000.0, RATE)) - db(low_pass.magnitude(2000.0, RATE));
        assert!((slope + 12.0).abs() < 1.0, "{slope}");
    }

    #[test]
    fn band_frequencies_are_limited_below_nyquist() {
        let high = band(FilterKind::LowPass, 20000.0, 0.0, 0.71);
        let filter = Biquad::design(&high, 16_000);
        let limited = Biquad::design(&band(FilterKind::LowPass, 7200.0, 0.0, 0.71), 16_000);
        assert_eq!(filter.b, limited.b);
        assert!(filter.b.iter().chain(&filter.a).all(|c| c.is_finite()));
    }

    fn compressor(threshold: f64, ratio: f64, knee: f64, attack: f64, release: f64) -> Compressor {
        Compressor {
            threshold_db: Tenths((threshold * 10.0) as i32),
            ratio: Tenths((ratio * 10.0) as i32),
            knee_db: Tenths((knee * 10.0) as i32),
            attack_ms: Tenths((attack * 10.0) as i32),
            release_ms: Tenths((release * 10.0) as i32),
            makeup_db: Tenths(0),
            detection: Detection::Peak,
            sidechain: None,
        }
    }

    #[test]
    fn compressor_follows_its_static_curve() {
        // Hard knee: unity below, 1/ratio above the threshold.
        assert_eq!(static_curve(-30.0, -20.0, 4.0, 0.0), -30.0);
        assert_eq!(static_curve(-8.0, -20.0, 4.0, 0.0), -17.0);
        // Soft knee: continuous at both knee edges, threshold + (W/2)²(1/R-1)/(2W) at the center.
        let edge = |x: f64| static_curve(x, -20.0, 4.0, 10.0);
        assert!((edge(-25.0) - -25.0).abs() < 1e-12);
        assert!((edge(-15.0) - (-20.0 + 5.0 / 4.0)).abs() < 1e-12);
        assert!((edge(-20.0) - (-20.0 + 25.0 * (0.25 - 1.0) / 20.0)).abs() < 1e-12);

        // A settled constant level renders with the curve's gain (peak and RMS).
        for detection in [Detection::Peak, Detection::Rms] {
            for (level_db, knee) in [(-30.0, 0.0), (-8.0, 0.0), (-20.0, 10.0), (-16.0, 10.0)] {
                let mut c = compressor(-20.0, 4.0, knee, 1.0, 5.0);
                c.detection = detection;
                let level = 10f64.powf(level_db / 20.0) as f32;
                let mut signal = stereo(vec![level; RATE as usize / 2]);
                process(&Processor::Compressor(c), &mut signal, Key::Own, &context());
                let out = db(f64::from(*signal[0].last().unwrap()));
                let expected = static_curve(level_db, -20.0, 4.0, knee);
                assert!(
                    (out - expected).abs() < 0.01,
                    "{detection:?} {level_db} dB knee {knee}: {out} instead of {expected}"
                );
            }
        }
    }

    #[test]
    fn compressor_attack_and_release_are_time_constants() {
        // From silence to a level 24 dB over a 4:1 threshold: 18 dB of reduction.
        let (attack_ms, release_ms) = (10.0, 100.0);
        let c = compressor(-30.0, 4.0, 0.0, attack_ms, release_ms);
        let rate = f64::from(RATE);
        let step = (rate * 0.5) as usize;
        let mut samples = vec![0.0f32; step];
        let loud = 10f64.powf(-6.0 / 20.0) as f32;
        samples.extend(vec![loud; step]);
        samples.extend(vec![10f64.powf(-60.0 / 20.0) as f32; step]);
        let gains = compressor_gains(&c, &stereo(samples), Key::Own, rate, &Automation::default());
        let reduction = |n: usize| -db(gains[n]);
        let full = 18.0;
        // After one time constant the reduction covers 1 - 1/e of the step.
        let attack = step + (attack_ms / 1000.0 * rate) as usize - 1;
        assert!(
            (reduction(attack) / full - (1.0 - (-1.0f64).exp())).abs() < 1e-3,
            "{}",
            reduction(attack)
        );
        assert!((reduction(2 * step - 1) - full).abs() < 1e-6);
        let release = 2 * step + (release_ms / 1000.0 * rate) as usize - 1;
        assert!(
            (reduction(release) / full - (-1.0f64).exp()).abs() < 1e-3,
            "{}",
            reduction(release)
        );
    }

    #[test]
    fn a_silent_sidechain_leaves_only_the_makeup_gain() {
        let mut c = compressor(-40.0, 10.0, 0.0, 1.0, 10.0);
        c.makeup_db = Tenths(60);
        let loud = stereo(vec![0.5; 4800]);
        let gains = compressor_gains(
            &c,
            &loud,
            Key::Silent,
            f64::from(RATE),
            &Automation::default(),
        );
        assert!(gains.iter().all(|g| (db(*g) - 6.0).abs() < 1e-9));
        let key = stereo(vec![0.5; 4800]);
        let quiet = stereo(vec![0.01; 4800]);
        let keyed = compressor_gains(
            &c,
            &quiet,
            Key::External(&key),
            f64::from(RATE),
            &Automation::default(),
        );
        assert!(
            db(*keyed.last().unwrap()) < -20.0,
            "the key drives the reduction"
        );
    }

    #[test]
    fn limiter_never_exceeds_its_ceiling() {
        let limiter = Limiter {
            ceiling_db: Tenths(-30),
            release_ms: Tenths(500),
            lookahead_ms: Tenths(50),
        };
        let ceiling = 10f64.powf(-3.0 / 20.0);
        // Pseudo-random bursts with isolated full-scale peaks.
        let mut state = 12345u64;
        let length = RATE as usize;
        let samples: Vec<f32> = (0..length)
            .map(|n| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let noise = ((state >> 33) as f64 / f64::from(1u32 << 31)) * 2.0 - 1.0;
                let burst = if (n / 2000) % 3 == 0 { 1.5 } else { 0.2 };
                (noise * burst) as f32
            })
            .collect();
        let mut signal = [samples.clone(), samples.iter().map(|s| -s * 0.7).collect()];
        process(
            &Processor::Limiter(limiter),
            &mut signal,
            Key::Own,
            &context(),
        );
        let peak = signal
            .iter()
            .flatten()
            .fold(0.0f64, |p, s| p.max(f64::from(s.abs())));
        assert!(peak <= ceiling, "{peak} > {ceiling}");
        assert!(peak > ceiling * 0.95, "the limiter reaches the ceiling");
        // Material below the ceiling far from any peak passes unchanged.
        let mut quiet = stereo(vec![0.1; 4800]);
        process(
            &Processor::Limiter(Limiter {
                ceiling_db: Tenths(-10),
                release_ms: Tenths(500),
                lookahead_ms: Tenths(50),
            }),
            &mut quiet,
            Key::Own,
            &context(),
        );
        assert!(quiet[0].iter().all(|s| *s == 0.1));
    }

    #[test]
    fn limiter_reduces_gain_before_a_peak_without_delaying_it() {
        let limiter = Limiter {
            ceiling_db: Tenths(-60),
            release_ms: Tenths(1000),
            lookahead_ms: Tenths(100),
        };
        let mut samples = vec![0.1f32; 4800];
        samples[2400] = 1.0;
        let mut signal = stereo(samples);
        process(
            &Processor::Limiter(limiter),
            &mut signal,
            Key::Own,
            &context(),
        );
        let ceiling = 10f64.powf(-6.0 / 20.0) as f32;
        assert!(signal[0][2400] <= ceiling && signal[0][2400] > ceiling * 0.999);
        assert_eq!(signal[0][2400 - 481], 0.1, "before the look-ahead window");
        assert!(signal[0][2400 - 240] < 0.1, "within the look-ahead window");
    }

    #[test]
    fn delay_echoes_land_at_the_delay_time_and_decay_by_the_feedback() {
        let d = Delay {
            time: DelayTime::Ms { ms: Tenths(1000) },
            feedback_percent: Tenths(500),
            mix_percent: Tenths(1000),
            ping_pong: false,
        };
        let mut samples = vec![0.0f32; RATE as usize / 2];
        samples[10] = 1.0;
        let mut signal = stereo(samples);
        process(
            &Processor::Delay(d.clone()),
            &mut signal,
            Key::Own,
            &context(),
        );
        let spacing = 4800;
        let nonzero: Vec<(usize, f32)> = signal[0]
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, s)| *s != 0.0)
            .collect();
        assert_eq!(
            nonzero,
            (1..=4)
                .map(|k| (10 + k * spacing, 0.5f32.powi(k as i32 - 1)))
                .collect::<Vec<_>>()
        );

        let ping = Delay {
            ping_pong: true,
            ..d
        };
        let mut samples = vec![0.0f32; RATE as usize / 2];
        samples[0] = 1.0;
        let mut signal = [samples, vec![0.0; RATE as usize / 2]];
        process(&Processor::Delay(ping), &mut signal, Key::Own, &context());
        assert_eq!(
            signal[0][spacing], 0.5,
            "first echo on the left, from the mono sum"
        );
        assert_eq!(signal[1][spacing], 0.0);
        assert_eq!(signal[1][2 * spacing], 0.25, "second on the right");
        assert_eq!(signal[0][2 * spacing], 0.0);
        assert_eq!(signal[0][3 * spacing], 0.125);
    }

    #[test]
    fn note_synced_delays_follow_the_tempo_stretches() {
        let d = Delay {
            time: DelayTime::Note {
                numerator: 1,
                denominator: 8,
            },
            feedback_percent: Tenths(0),
            mix_percent: Tenths(1000),
            ping_pong: false,
        };
        // 120 BPM, then 60 BPM from sample 30000: an eighth is 250 ms, then 500 ms.
        let tempo = [(0usize, 500_000u32), (30_000, 1_000_000)];
        let context = EffectContext {
            sample_rate: RATE,
            tempo: &tempo,
        };
        let mut samples = vec![0.0f32; 60_000];
        samples[100] = 1.0;
        samples[40_000] = 1.0;
        let mut signal = stereo(samples);
        process(
            &Processor::Delay(d.clone()),
            &mut signal,
            Key::Own,
            &context,
        );
        assert_eq!(signal[0][100 + 12_000], 1.0);
        let echoes: Vec<usize> = (0..60_000).filter(|&n| signal[0][n] != 0.0).collect();
        assert_eq!(echoes, vec![12_100]);
        let mut longer = stereo(vec![0.0f32; 70_000]);
        longer[0][40_000] = 1.0;
        longer[1][40_000] = 1.0;
        process(&Processor::Delay(d), &mut longer, Key::Own, &context);
        assert_eq!(longer[0][64_000], 1.0);
    }

    fn reverb_impulse(room: i32, rate: u32) -> Vec<f32> {
        let r = Reverb {
            room_size_percent: Tenths(room),
            damping_percent: Tenths(500),
            pre_delay_ms: Tenths(200),
            mix_percent: Tenths(1000),
        };
        let mut samples = vec![0.0f32; rate as usize * 3];
        samples[0] = 1.0;
        let mut signal = stereo(samples);
        process(
            &Processor::Reverb(r),
            &mut signal,
            Key::Own,
            &EffectContext {
                sample_rate: rate,
                tempo: &[],
            },
        );
        signal[0].clone()
    }

    /// Seconds for the energy of 50 ms windows to fall by 30 dB from the first.
    fn decay_seconds(response: &[f32], rate: u32) -> f64 {
        let window = rate as usize / 20;
        let energies: Vec<f64> = response
            .chunks(window)
            .map(|w| w.iter().map(|s| f64::from(*s).powi(2)).sum())
            .collect();
        let start = energies.iter().position(|e| *e > 0.0).unwrap() + 1;
        let reference = energies[start];
        let end = energies[start..]
            .iter()
            .position(|e| *e < reference * 1e-3)
            .unwrap();
        end as f64 * 0.05
    }

    #[test]
    fn reverb_decays_deterministically_and_scales_with_room_size() {
        let small = reverb_impulse(300, RATE);
        assert_eq!(small, reverb_impulse(300, RATE), "deterministic");
        // Pre-delay: silence for the first 20 ms.
        let pre_delay = RATE as usize / 100 * 2;
        assert!(small[..pre_delay].iter().all(|s| *s == 0.0));
        assert!(small[pre_delay..pre_delay + 4000].iter().any(|s| *s != 0.0));
        // Energy falls window by window after the build-up.
        let window = RATE as usize / 20;
        let energy = |k: usize| -> f64 {
            small[k * window..(k + 1) * window]
                .iter()
                .map(|s| f64::from(*s).powi(2))
                .sum()
        };
        for k in 2..20 {
            assert!(energy(k + 1) < energy(k), "window {k}");
        }
        let (short, long) = (
            decay_seconds(&small, RATE),
            decay_seconds(&reverb_impulse(900, RATE), RATE),
        );
        assert!(long > 2.0 * short, "{short} s vs {long} s");
        // Line lengths scale with the rate, so the decay time is kept.
        let at_rate = decay_seconds(&reverb_impulse(300, 24_000), 24_000);
        assert!((at_rate - short).abs() <= 0.1, "{at_rate} vs {short}");
    }

    #[test]
    fn snes_echo_impulse_follows_the_fir_taps() {
        let rate = 32_000;
        let context = EffectContext {
            sample_rate: rate,
            tempo: &[],
        };
        let fir = [1, 2, 4, 8, 16, 32, 64, -128];
        let echo = SnesEcho {
            fir,
            delay: 1,
            feedback: 0,
            volume_left: 127,
            volume_right: -64,
            dry: false,
        };
        let mut samples = vec![0.0f32; 2000];
        samples[3] = 0.5;
        let mut signal = stereo(samples);
        process(
            &Processor::SnesEcho(echo.clone()),
            &mut signal,
            Key::Own,
            &context,
        );
        // EDL 1 is 512 samples at 32 kHz. Input 16384 is stored, read back
        // halved (8192), and tap k after the read uses C(7-k).
        let code = |value: i32| (f64::from(value) / 32767.0) as f32;
        let expected = |coefficient: i32, volume: i32| {
            let echo = ((8192 * coefficient) >> 6) & !1;
            code((echo * volume) >> 7)
        };
        for k in 0..8 {
            let n = 3 + 512 + k;
            let c = i32::from(fir[7 - k]);
            assert_eq!(signal[0][n], expected(c, 127), "left tap {k}");
            assert_eq!(signal[1][n], expected(c, -64), "right tap {k}");
        }
        let heard: Vec<usize> = (0..2000).filter(|&n| signal[0][n] != 0.0).collect();
        assert_eq!(heard, (515..523).collect::<Vec<_>>());

        // Feedback with the pass-through filter: each echo repeats one buffer
        // length plus the 7-sample FIR position later.
        let feedback = SnesEcho {
            fir: [127, 0, 0, 0, 0, 0, 0, 0],
            feedback: 64,
            volume_left: 127,
            volume_right: 127,
            ..echo
        };
        let mut samples = vec![0.0f32; 2000];
        samples[0] = 0.5;
        let mut signal = stereo(samples);
        process(
            &Processor::SnesEcho(feedback),
            &mut signal,
            Key::Own,
            &context,
        );
        let heard: Vec<(usize, f32)> = (0..2000)
            .filter(|&n| signal[0][n] != 0.0)
            .map(|n| (n, signal[0][n]))
            .collect();
        let first = ((8192 * 127) >> 6) & !1; // 16256
        let written = ((first * 64) >> 7) & !1; // 8128
        let second = (((written >> 1) * 127) >> 6) & !1;
        assert_eq!(
            heard,
            vec![
                (519, code((first * 127) >> 7)),
                (1038, code((second * 127) >> 7)),
                (1557, heard[2].1),
            ]
        );
        assert!(heard[2].1 < heard[1].1);

        // At other rates EDL keeps 16 ms steps.
        assert_eq!(echo_length(3, 48_000.0), 2304);
        assert_eq!(echo_length(0, 48_000.0), 1);
    }

    fn echo_of(echo: SnesEcho, input: &[f32], length: usize) -> [Vec<f32>; 2] {
        let context = EffectContext {
            sample_rate: 32_000,
            tempo: &[],
        };
        let mut samples = input.to_vec();
        samples.resize(length, 0.0);
        let mut signal = stereo(samples);
        process(&Processor::SnesEcho(echo), &mut signal, Key::Own, &context);
        signal
    }

    #[test]
    fn snes_echo_passes_pcm16_codes_through_unchanged() {
        // C7 = -128 and EVOL = -128 return each even buffer code exactly, so
        // the echo's 16-bit codes are the render's PCM16 codes at full scale.
        let codes = [32766, -32766, 20000, -20000, 16384, -16386, 2, -2];
        let input: Vec<f32> = codes
            .iter()
            .map(|&c| (f64::from(c) / 32767.0) as f32)
            .collect();
        let identity = SnesEcho {
            fir: [0, 0, 0, 0, 0, 0, 0, -128],
            delay: 1,
            feedback: 0,
            volume_left: -128,
            volume_right: -128,
            dry: false,
        };
        let signal = echo_of(identity, &input, 600);
        assert_eq!(&signal[0][512..520], input.as_slice());
        assert_eq!(&signal[1][512..520], input.as_slice());
    }

    #[test]
    fn snes_echo_wraps_fir_and_feedback_products_and_clamps_the_output() {
        // A buffer of -32768 halves to -16384; with C7, EFB and EVOL at -128
        // each product shifts to +32768. The newest tap and the EFB product
        // wrap to -32768; the EVOL product is clamped to +32767.
        let wraps = SnesEcho {
            fir: [0, 0, 0, 0, 0, 0, 0, -128],
            delay: 1,
            feedback: -128,
            volume_left: -128,
            volume_right: 127,
            dry: false,
        };
        let signal = echo_of(wraps, &[-1.0; 512], 1600);
        // Left: the echo stays -32768 and its output clamps to +32767.
        assert_eq!(signal[0][512], 1.0);
        // The feedback write wraps to -32768, so the second pass repeats it.
        assert_eq!(signal[0][1024], 1.0);
        // Right: EVOL 127 gives (-32768 x 127) >> 7 = -32512.
        assert_eq!(signal[1][1024], (-32512.0 / 32767.0) as f32);
    }

    #[test]
    fn processors_keep_silence_silent() {
        let processors = [
            Processor::Equalizer(Equalizer {
                bands: vec![band(FilterKind::Peaking, 1000.0, 6.0, 1.0)],
            }),
            Processor::Compressor(compressor(-20.0, 4.0, 6.0, 5.0, 50.0)),
            Processor::Reverb(Reverb {
                room_size_percent: Tenths(500),
                damping_percent: Tenths(500),
                pre_delay_ms: Tenths(0),
                mix_percent: Tenths(300),
            }),
        ];
        for processor in processors {
            let mut signal = stereo(vec![0.0; 1000]);
            process(&processor, &mut signal, Key::Own, &context());
            assert!(signal.iter().flatten().all(|s| *s == 0.0));
        }
    }

    fn noise(length: usize, seed: u64) -> [Vec<f32>; 2] {
        let mut state = seed;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (((state >> 33) as f64 / f64::from(1u32 << 31)) * 2.0 - 1.0) as f32 * 0.8
        };
        let left: Vec<f32> = (0..length).map(|_| next()).collect();
        let right: Vec<f32> = (0..length).map(|_| next()).collect();
        [left, right]
    }

    fn varying(length: usize, value: impl Fn(usize) -> f64) -> Values {
        Values::Varying((0..length).map(value).collect())
    }

    #[test]
    fn lanes_holding_the_static_setting_render_like_no_lane() {
        use EffectParameter as P;
        let length = 4096;
        let echo = SnesEcho {
            fir: [127, 0, 0, 0, 0, 0, 0, 0],
            delay: 0,
            feedback: 50,
            volume_left: 64,
            volume_right: -40,
            dry: true,
        };
        type Lanes = Vec<(P, Option<u8>, f64)>;
        let cases: Vec<(Processor, Lanes)> = vec![
            (
                Processor::Equalizer(Equalizer {
                    bands: vec![
                        band(FilterKind::HighPass, 80.0, 0.0, 0.71),
                        band(FilterKind::Peaking, 1000.0, 6.0, 1.0),
                    ],
                }),
                vec![
                    (P::FrequencyHz, Some(0), 80.0),
                    (P::GainDb, Some(1), 6.0),
                    (P::FrequencyHz, Some(1), 1000.0),
                ],
            ),
            (
                Processor::Compressor(compressor(-20.0, 4.0, 6.0, 5.0, 50.0)),
                vec![
                    (P::ThresholdDb, None, -20.0),
                    (P::Ratio, None, 4.0),
                    (P::MakeupDb, None, 0.0),
                ],
            ),
            (
                Processor::Limiter(Limiter {
                    ceiling_db: Tenths(-60),
                    release_ms: Tenths(500),
                    lookahead_ms: Tenths(30),
                }),
                vec![(P::CeilingDb, None, -6.0)],
            ),
            (
                Processor::Delay(Delay {
                    time: DelayTime::Ms { ms: Tenths(30) },
                    feedback_percent: Tenths(400),
                    mix_percent: Tenths(500),
                    ping_pong: false,
                }),
                vec![
                    (P::FeedbackPercent, None, 40.0),
                    (P::MixPercent, None, 50.0),
                ],
            ),
            (
                Processor::Reverb(Reverb {
                    room_size_percent: Tenths(600),
                    damping_percent: Tenths(300),
                    pre_delay_ms: Tenths(0),
                    mix_percent: Tenths(250),
                }),
                vec![
                    (P::RoomSizePercent, None, 60.0),
                    (P::DampingPercent, None, 30.0),
                    (P::MixPercent, None, 25.0),
                ],
            ),
            (
                Processor::SnesEcho(echo),
                vec![
                    (P::Feedback, None, 50.0),
                    (P::VolumeLeft, None, 64.0),
                    (P::VolumeRight, None, -40.0),
                ],
            ),
        ];
        for (processor, lanes) in cases {
            let input = noise(length, 7);
            let mut plain = input.clone();
            process(&processor, &mut plain, Key::Own, &context());
            for constant in [true, false] {
                let mut automation = Automation::default();
                for &(parameter, band, value) in &lanes {
                    assert_eq!(
                        parameter.static_value(&processor, band),
                        Some(value),
                        "{parameter:?}"
                    );
                    let values = if constant {
                        Values::Constant(value)
                    } else {
                        varying(length, |_| value)
                    };
                    automation.push(parameter, band, values);
                }
                let mut automated = input.clone();
                process_automated(
                    &processor,
                    &mut automated,
                    Key::Own,
                    &context(),
                    &automation,
                );
                assert_eq!(automated, plain, "{} constant={constant}", processor.kind());
            }
        }
    }

    #[test]
    fn a_linear_makeup_ramp_scales_every_sample_exactly() {
        // Ratio 1 never reduces gain, so the output is input × makeup gain.
        let mut c = compressor(0.0, 1.0, 0.0, 0.0, 1.0);
        c.makeup_db = Tenths(0);
        let length = 4800;
        let ramp = |n: usize| 12.0 * n as f64 / length as f64;
        let mut automation = Automation::default();
        automation.push(EffectParameter::MakeupDb, None, varying(length, ramp));
        let input = noise(length, 3);
        let mut signal = input.clone();
        process_automated(
            &Processor::Compressor(c),
            &mut signal,
            Key::Own,
            &context(),
            &automation,
        );
        for side in 0..2 {
            for n in 0..length {
                let expected = (f64::from(input[side][n]) * 10f64.powf(ramp(n) / 20.0)) as f32;
                assert_eq!(signal[side][n], expected, "side {side} sample {n}");
            }
        }
    }

    #[test]
    fn a_falling_limiter_ceiling_holds_a_loud_signal_at_the_ceiling() {
        let limiter = Limiter {
            ceiling_db: Tenths(0),
            release_ms: Tenths(500),
            lookahead_ms: Tenths(0),
        };
        let length = 4800;
        // -6 dBFS falling linearly to -12 dBFS, then a step back to -6 dBFS.
        let ceiling_db = |n: usize| {
            if n < 3600 {
                -6.0 - 6.0 * n as f64 / 3600.0
            } else {
                -6.0
            }
        };
        let mut automation = Automation::default();
        automation.push(
            EffectParameter::CeilingDb,
            None,
            varying(length, ceiling_db),
        );
        let mut signal = stereo(vec![0.9; length]);
        process_automated(
            &Processor::Limiter(limiter),
            &mut signal,
            Key::Own,
            &context(),
            &automation,
        );
        for (n, out) in signal[0].iter().enumerate() {
            let ceiling = 10f64.powf(ceiling_db(n) / 20.0);
            let out = f64::from(*out);
            assert!(out <= ceiling, "sample {n}: {out} over {ceiling}");
            if n < 3600 {
                // A falling ceiling is followed at once, without look-ahead.
                assert!(
                    (out - ceiling).abs() < 1e-6,
                    "sample {n}: {out} not {ceiling}"
                );
            }
        }
        // After the step up the gain recovers with the release time constant.
        let after = f64::from(signal[0][3600]);
        assert!(after < 10f64.powf(-6.0 / 20.0) * 0.6, "{after}");
        assert!(signal[0][4799] > signal[0][3601]);
    }

    #[test]
    fn an_eq_gain_step_settles_at_each_static_response() {
        let length = RATE as usize;
        let frequency = 1000.0;
        let sine: Vec<f32> = (0..length)
            .map(|n| {
                (0.5 * (2.0 * std::f64::consts::PI * frequency * n as f64 / f64::from(RATE)).sin())
                    as f32
            })
            .collect();
        let step = length / 2;
        let eq = Processor::Equalizer(Equalizer {
            bands: vec![band(FilterKind::Peaking, frequency, 0.0, 1.0)],
        });
        let mut automation = Automation::default();
        automation.push(
            EffectParameter::GainDb,
            Some(0),
            varying(length, |n| if n < step { 0.0 } else { 6.0 }),
        );
        let mut signal = stereo(sine.clone());
        let mut plain = stereo(sine.clone());
        process(&eq, &mut plain, Key::Own, &context());
        process_automated(&eq, &mut signal, Key::Own, &context(), &automation);
        // Identical up to the step sample, then a different response.
        assert_eq!(signal[0][..step], plain[0][..step]);
        assert_ne!(signal[0][step + 1], plain[0][step + 1]);
        let rms = |x: &[f32]| {
            (x.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
        };
        let gain =
            |range: std::ops::Range<usize>| db(rms(&signal[0][range.clone()]) / rms(&sine[range]));
        assert!(
            gain(step / 2..step).abs() < 0.02,
            "{}",
            gain(step / 2..step)
        );
        let settled = gain(step + step / 2..length);
        assert!((settled - 6.0).abs() < 0.02, "{settled}");
    }

    #[test]
    fn register_lanes_round_to_whole_values() {
        let mut automation = Automation::default();
        automation.push(
            EffectParameter::VolumeLeft,
            None,
            Values::Varying(vec![40.4, 40.5, -40.5, 200.0]),
        );
        let values: Vec<i32> = (0..4)
            .map(|n| automation.register(EffectParameter::VolumeLeft, n, 0))
            .collect();
        assert_eq!(values, [40, 41, -41, 127]);
        assert_eq!(automation.register(EffectParameter::VolumeRight, 0, -7), -7);
    }
}
