//! Mixer state of a revision: voice strips, buses, sends, the master gain,
//! their volume and pan automation, and insert effect chains.
//!
//! The mixer is applied after the reference synthesizer, so it changes what a
//! render sounds like without changing channel messages. MIDI export does not
//! write it; the composition's volume and pan control lanes are the MIDI-carried
//! levels.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::composition::{Composition, Timebase};
use crate::diagnostic::{DiagnosticCode as Code, Report};
use crate::edit::{EditOperation, EditRequest, check_change};
use crate::effects::{self, Automation, EffectChain, EffectContext, EffectParameter, Key, Values};
use crate::render::sample_at;
use crate::review::RenderPart;
use crate::timeline::{Rational, seconds_at};
use crate::voice_instruments::InstrumentAssignment;

/// Lowest volume or send gain, in dB. Use `mute` for silence.
pub const MIN_GAIN_DB: f64 = -120.0;
/// Highest volume or send gain, in dB.
pub const MAX_GAIN_DB: f64 = 12.0;
/// Pan extent: -100 is fully left, 100 fully right.
pub const MAX_PAN: f64 = 100.0;
/// Output name of the master bus; bus ids cannot use it.
pub const MASTER: &str = "master";

/// A number with at most one digit after the decimal point, stored exactly
/// as an integer count of tenths so revisions compare and hash exactly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tenths(pub i32);

impl Tenths {
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }

    pub fn value(self) -> f64 {
        f64::from(self.0) / 10.0
    }
}

impl Serialize for Tenths {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.value())
    }
}

impl<'de> Deserialize<'de> for Tenths {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        let scaled = value * 10.0;
        let rounded = scaled.round();
        if !value.is_finite() || (scaled - rounded).abs() > 1e-6 || rounded.abs() > 1.0e9 {
            return Err(serde::de::Error::custom(format!(
                "{value} is not a decimal with at most one digit after the point, such as -6.5"
            )));
        }
        Ok(Self(rounded as i32))
    }
}

impl JsonSchema for Tenths {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Tenths".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "number",
            "description": "A decimal with at most one digit after the point, such as -6.5."
        })
    }
}

/// Mixer settings of a revision. Everything left out has its neutral value:
/// 0 dB, centered, unmuted, routed to the master.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mix {
    /// Strips of voices with non-neutral settings, at most one per voice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voices: Vec<VoiceStrip>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buses: Vec<Bus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<MixSend>,
    /// Gain in dB applied to the sum of every voice and bus.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub master_volume_db: Tenths,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automation: Vec<MixLane>,
    /// Insert effect chains, at most one per voice, bus or the master.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<EffectChain>,
    /// Automation of effect parameters, at most one lane per target, effect,
    /// parameter and band.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effect_automation: Vec<EffectLane>,
    /// Chip instruments that play voices instead of the reference
    /// synthesizer, at most one per voice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instruments: Vec<InstrumentAssignment>,
    /// Audio clips placed on voices' timelines, played through their strips.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<crate::audio_clips::AudioClip>,
}

impl Mix {
    /// True when the mixer has no data, so renders use the plain synthesizer sum.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    fn strip(&self, voice: &str) -> Option<&VoiceStrip> {
        self.voices.iter().find(|s| s.voice == voice)
    }

    fn bus(&self, id: &str) -> Option<&Bus> {
        self.buses.iter().find(|b| b.id == id)
    }

    pub(crate) fn chain(&self, target: &MixTarget) -> Option<&EffectChain> {
        self.effects.iter().find(|c| &c.target == target)
    }

    /// Voice or bus strips whose sidechain keys `target`'s chain reads.
    fn keys_of(&self, target: &MixTarget) -> Vec<&MixTarget> {
        self.chain(target)
            .into_iter()
            .flat_map(|c| &c.effects)
            .filter(|e| !e.bypass)
            .filter_map(|e| e.processor.sidechain())
            .collect()
    }

    fn lane(&self, target: &MixTarget, parameter: MixParameter) -> Option<&MixLane> {
        self.automation
            .iter()
            .find(|l| &l.target == target && l.parameter == parameter)
    }

    /// Value of `parameter` on `target` when no automation point applies.
    fn static_value(&self, target: &MixTarget, parameter: MixParameter) -> Tenths {
        match (target, parameter) {
            (MixTarget::Voice { id }, MixParameter::Volume) => {
                self.strip(id).map(|s| s.volume_db).unwrap_or_default()
            }
            (MixTarget::Voice { id }, MixParameter::Pan) => {
                self.strip(id).map(|s| s.pan).unwrap_or_default()
            }
            (MixTarget::Bus { id }, MixParameter::Volume) => {
                self.bus(id).map(|b| b.volume_db).unwrap_or_default()
            }
            (MixTarget::Bus { id }, MixParameter::Pan) => {
                self.bus(id).map(|b| b.pan).unwrap_or_default()
            }
            (MixTarget::Master, _) => self.master_volume_db,
        }
    }

    fn effect_lane(&self, key: &EffectLaneKey) -> Option<&EffectLane> {
        self.effect_automation.iter().find(|l| &l.key() == key)
    }

    /// The effect a lane addresses, if its chain holds it.
    fn effect(&self, target: &MixTarget, id: &str) -> Option<&effects::Effect> {
        self.chain(target)
            .and_then(|c| c.effects.iter().find(|e| e.id == id))
    }

    /// Values of an effect lane, with the effect's static value before the
    /// first point; `None` when the effect or its parameter does not exist.
    fn effect_curve<'a>(
        &'a self,
        key: &EffectLaneKey,
        points: &'a [MixPoint],
    ) -> Option<Curve<'a>> {
        let effect = self.effect(&key.0, &key.1)?;
        Some(Curve {
            points,
            base: key.2.static_value(&effect.processor, key.3)?,
        })
    }

    fn curve(&self, target: &MixTarget, parameter: MixParameter) -> Curve<'_> {
        Curve {
            points: self
                .lane(target, parameter)
                .map(|l| l.points.as_slice())
                .unwrap_or_default(),
            base: self.static_value(target, parameter).value(),
        }
    }

    /// Whether `voice` contributes to a render: not muted, and soloed when
    /// any voice is soloed.
    pub fn audible(&self, voice: &str) -> bool {
        let strip = self.strip(voice);
        let soloing = self.voices.iter().any(|s| s.solo);
        !strip.is_some_and(|s| s.mute) && (!soloing || strip.is_some_and(|s| s.solo))
    }
}

/// Mixer channel strip of one voice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceStrip {
    pub voice: String,
    /// Fader gain in dB, -120.0–12.0.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub volume_db: Tenths,
    /// Stereo balance, -100.0 (left) to 100.0 (right); the center is unity.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub pan: Tenths,
    /// Silences the voice and its sends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mute: bool,
    /// When any voice is soloed, only soloed voices are heard.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub solo: bool,
    /// Bus receiving the voice's main output; absent means the master.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl VoiceStrip {
    fn is_neutral(&self) -> bool {
        self.volume_db.is_zero()
            && self.pan.is_zero()
            && !self.mute
            && !self.solo
            && self.output.is_none()
    }
}

/// A submix that sums voice outputs and sends into the master.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Bus {
    /// Unique among buses; `master` is reserved.
    pub id: String,
    /// Free description, such as `drums` or `echo return`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Gain in dB, -120.0–12.0.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub volume_db: Tenths,
    /// Stereo balance, -100.0–100.0.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub pan: Tenths,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mute: bool,
}

/// An extra feed from a voice into a bus, besides the voice's main output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MixSend {
    pub voice: String,
    pub bus: String,
    /// Send gain in dB, -120.0–12.0.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub gain_db: Tenths,
    /// Feeds the signal before the voice's volume and pan instead of after.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pre_fader: bool,
}

/// Automation of one mixer parameter. Before the first point the strip's
/// static value applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MixLane {
    pub target: MixTarget,
    pub parameter: MixParameter,
    pub points: Vec<MixPoint>,
}

/// Automation of one effect parameter, addressed by the chain's target, the
/// effect id and the parameter (with the band for equalizer parameters).
/// Before the first point the effect's static setting applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectLane {
    pub target: MixTarget,
    /// Id of the effect in the target's chain.
    pub effect: String,
    pub parameter: EffectParameter,
    /// 0-based equalizer band for `gain_db` and `frequency_hz`; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<u8>,
    /// Values in the parameter's unit, strictly increasing `at`.
    pub points: Vec<MixPoint>,
}

/// `(target, effect id, parameter, band)` of an effect lane.
pub(crate) type EffectLaneKey = (MixTarget, String, EffectParameter, Option<u8>);

impl EffectLane {
    pub(crate) fn key(&self) -> EffectLaneKey {
        (
            self.target.clone(),
            self.effect.clone(),
            self.parameter,
            self.band,
        )
    }

    /// A readable name, such as ``gain_db of band 0 of effect `tone` on voice `lead` ``.
    pub(crate) fn label(key: &EffectLaneKey) -> String {
        let band = key.3.map(|b| format!(" band {b}")).unwrap_or_default();
        format!(
            "{}{band} automation of effect `{}` on {}",
            key.2.name(),
            key.1,
            key.0
        )
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MixTarget {
    Voice { id: String },
    Bus { id: String },
    Master,
}

impl std::fmt::Display for MixTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Voice { id } => write!(f, "voice `{id}`"),
            Self::Bus { id } => write!(f, "bus `{id}`"),
            Self::Master => write!(f, "the master"),
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MixParameter {
    /// Gain in dB, -120.0–12.0.
    Volume,
    /// Stereo balance, -100.0–100.0. Voices and buses only.
    Pan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MixPoint {
    /// Position in the project's timebase unit.
    pub at: u64,
    /// dB for volume, -100.0–100.0 for pan; an effect lane's value in its
    /// parameter's unit.
    pub value: Tenths,
    /// How the value moves from this point to the next one: step (default),
    /// linear, or exponential (effect EQ frequency_hz and compressor ratio
    /// only).
    #[serde(default, skip_serializing_if = "MixShape::is_step")]
    pub shape: MixShape,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MixShape {
    /// Hold the value until the next point.
    #[default]
    Step,
    /// Ramp linearly, in the parameter's unit and timebase position, to the next point.
    Linear,
    /// Ramp geometrically to the next point: equal ratios in equal timebase
    /// spans, so a frequency sweep moves evenly in octaves. Only for effect
    /// parameters on a ratio scale: EQ `frequency_hz` and compressor `ratio`.
    Exponential,
}

impl MixShape {
    fn is_step(&self) -> bool {
        *self == Self::Step
    }
}

/// Values of one parameter over timebase positions.
#[derive(Clone, Copy)]
struct Curve<'a> {
    points: &'a [MixPoint],
    base: f64,
}

impl Curve<'_> {
    /// Value at `at` on the segment that follows the first `index` points.
    fn segment_value(&self, index: usize, at: f64) -> f64 {
        if index == 0 {
            return self.base;
        }
        let point = self.points[index - 1];
        match (point.shape, self.points.get(index)) {
            (MixShape::Linear, Some(next)) => {
                let (from, to) = (point.at as f64, next.at as f64);
                point.value.value()
                    + (next.value.value() - point.value.value()) * (at - from) / (to - from)
            }
            (MixShape::Exponential, Some(next)) => {
                let (from, to) = (point.at as f64, next.at as f64);
                geometric(
                    point.value.value(),
                    next.value.value(),
                    (at - from) / (to - from),
                )
            }
            _ => point.value.value(),
        }
    }

    /// Whether the segment in effect at `at` ramps geometrically.
    fn geometric_at(&self, at: f64) -> bool {
        let index = self.points.iter().take_while(|p| p.at as f64 <= at).count();
        index > 0
            && self.points[index - 1].shape == MixShape::Exponential
            && index < self.points.len()
    }

    /// Value in effect at `at`, including a point placed exactly there.
    fn at(&self, at: f64) -> f64 {
        self.segment_value(
            self.points.iter().take_while(|p| p.at as f64 <= at).count(),
            at,
        )
    }

    /// Value approaching `at` from earlier positions.
    fn before(&self, at: f64) -> f64 {
        self.segment_value(
            self.points
                .iter()
                .take_while(|p| (p.at as f64) < at)
                .count(),
            at,
        )
    }
}

const SAME_VALUE: f64 = 1e-9;

/// `from · (to / from)^fraction`: the value a fraction of the way along a
/// geometric ramp between two positive values.
fn geometric(from: f64, to: f64, fraction: f64) -> f64 {
    if from == to {
        from
    } else {
        from * (to / from).powf(fraction)
    }
}

/// Refuses `exponential` points on parameters without a ratio scale.
fn check_shapes(
    points: &[MixPoint],
    allowed: bool,
    name: &str,
    report: &mut Report,
    at: impl Fn(usize) -> String,
) {
    for (j, point) in points.iter().enumerate() {
        if point.shape != MixShape::Exponential {
            continue;
        }
        if !allowed {
            report.push(
                Code::InvalidRange,
                at(j),
                format!("exponential applies to EQ frequency_hz and compressor ratio only; {name} moves in its own unit (decibels are already logarithmic), so use linear"),
            );
        } else if point.value.value() <= 0.0
            || points.get(j + 1).is_some_and(|n| n.value.value() <= 0.0)
        {
            report.push(
                Code::ValueOutOfRange,
                at(j),
                "an exponential ramp needs positive values at both ends",
            );
        }
    }
}

fn range_of(parameter: MixParameter) -> (f64, f64) {
    match parameter {
        MixParameter::Volume => (MIN_GAIN_DB, MAX_GAIN_DB),
        MixParameter::Pan => (-MAX_PAN, MAX_PAN),
    }
}

fn check_value(value: Tenths, parameter: MixParameter, report: &mut Report, at: String) {
    let (low, high) = range_of(parameter);
    if !(low..=high).contains(&value.value()) {
        let unit = match parameter {
            MixParameter::Volume => " dB",
            MixParameter::Pan => "",
        };
        report.push(
            Code::ValueOutOfRange,
            at,
            format!("value must be {low}–{high}{unit}"),
        );
    }
}

/// Checks references, ranges and automation order of the mixer.
pub(crate) fn check_mix(composition: &Composition, voice_ids: &HashSet<&str>, report: &mut Report) {
    let mix = &composition.mix;
    let end = composition.playback.end;
    let bus_ids: HashSet<&str> = mix.buses.iter().map(|b| b.id.as_str()).collect();

    let mut strips = HashSet::new();
    for (i, strip) in mix.voices.iter().enumerate() {
        if !voice_ids.contains(strip.voice.as_str()) {
            report.push(
                Code::UnknownReference,
                format!("mix/voices/{i}/voice"),
                format!("voice `{}` does not exist", strip.voice),
            );
        }
        if !strips.insert(strip.voice.as_str()) {
            report.push(
                Code::DuplicateId,
                format!("mix/voices/{i}/voice"),
                format!("voice `{}` has two mixer strips", strip.voice),
            );
        }
        check_value(
            strip.volume_db,
            MixParameter::Volume,
            report,
            format!("mix/voices/{i}/volume_db"),
        );
        check_value(
            strip.pan,
            MixParameter::Pan,
            report,
            format!("mix/voices/{i}/pan"),
        );
        if let Some(output) = &strip.output
            && !bus_ids.contains(output.as_str())
        {
            report.push(
                Code::UnknownReference,
                format!("mix/voices/{i}/output"),
                format!("bus `{output}` does not exist"),
            );
        }
    }

    let mut buses = HashSet::new();
    for (i, bus) in mix.buses.iter().enumerate() {
        if bus.id.is_empty() || bus.id == MASTER {
            report.push(
                Code::InvalidRange,
                format!("mix/buses/{i}/id"),
                "a bus id must not be empty or `master`",
            );
        } else if !buses.insert(bus.id.as_str()) {
            report.push(
                Code::DuplicateId,
                format!("mix/buses/{i}/id"),
                format!("bus `{}` is defined twice", bus.id),
            );
        }
        check_value(
            bus.volume_db,
            MixParameter::Volume,
            report,
            format!("mix/buses/{i}/volume_db"),
        );
        check_value(
            bus.pan,
            MixParameter::Pan,
            report,
            format!("mix/buses/{i}/pan"),
        );
    }

    let mut sends = HashSet::new();
    for (i, send) in mix.sends.iter().enumerate() {
        if !voice_ids.contains(send.voice.as_str()) {
            report.push(
                Code::UnknownReference,
                format!("mix/sends/{i}/voice"),
                format!("voice `{}` does not exist", send.voice),
            );
        }
        if !bus_ids.contains(send.bus.as_str()) {
            report.push(
                Code::UnknownReference,
                format!("mix/sends/{i}/bus"),
                format!("bus `{}` does not exist", send.bus),
            );
        }
        if !sends.insert((send.voice.as_str(), send.bus.as_str())) {
            report.push(
                Code::DuplicateId,
                format!("mix/sends/{i}"),
                format!("voice `{}` sends to bus `{}` twice", send.voice, send.bus),
            );
        }
        check_value(
            send.gain_db,
            MixParameter::Volume,
            report,
            format!("mix/sends/{i}/gain_db"),
        );
    }
    check_value(
        mix.master_volume_db,
        MixParameter::Volume,
        report,
        "mix/master_volume_db".into(),
    );

    let mut lanes = HashSet::new();
    for (i, lane) in mix.automation.iter().enumerate() {
        let exists = match &lane.target {
            MixTarget::Voice { id } => voice_ids.contains(id.as_str()),
            MixTarget::Bus { id } => bus_ids.contains(id.as_str()),
            MixTarget::Master => true,
        };
        if !exists {
            report.push(
                Code::UnknownReference,
                format!("mix/automation/{i}/target"),
                format!("{} does not exist", lane.target),
            );
        }
        if lane.target == MixTarget::Master && lane.parameter == MixParameter::Pan {
            report.push(
                Code::InvalidRange,
                format!("mix/automation/{i}/parameter"),
                "the master has volume only",
            );
        }
        if !lanes.insert((&lane.target, lane.parameter)) {
            report.push(
                Code::DuplicateControlLane,
                format!("mix/automation/{i}"),
                "each mixer parameter has at most one automation lane",
            );
        }
        if lane.points.is_empty() {
            report.push(
                Code::EmptyControlLane,
                format!("mix/automation/{i}/points"),
                "remove the lane instead of leaving it empty",
            );
            continue;
        }
        let parameter_name = match lane.parameter {
            MixParameter::Volume => "volume",
            MixParameter::Pan => "pan",
        };
        check_shapes(&lane.points, false, parameter_name, report, |j| {
            format!("mix/automation/{i}/points/{j}/shape")
        });
        for (j, point) in lane.points.iter().enumerate() {
            check_value(
                point.value,
                lane.parameter,
                report,
                format!("mix/automation/{i}/points/{j}/value"),
            );
            if j > 0 && point.at <= lane.points[j - 1].at {
                report.push(
                    Code::ControlPointsUnordered,
                    format!("mix/automation/{i}/points/{j}/at"),
                    "automation points must have increasing times",
                );
            }
            if point.at >= end && end > 0 {
                report.push(
                    Code::EventBeyondEnd,
                    format!("mix/automation/{i}/points/{j}/at"),
                    "automation points must be before the playback end",
                );
            }
        }
        if let Some(loop_start) = composition.playback.loop_start
            && !lane.points.iter().any(|p| p.at == loop_start)
        {
            let curve = mix.curve(&lane.target, lane.parameter);
            let (at_loop, at_end) = (curve.at(loop_start as f64), curve.at(end as f64));
            if (at_loop - at_end).abs() > SAME_VALUE {
                report.push(
                    Code::LoopStateMismatch,
                    format!("mix/automation/{i}"),
                    format!("value is {at_end} at the end but {at_loop} at the loop start; add a point at {loop_start}"),
                );
            }
        }
    }
    effects::check_effects(composition, voice_ids, &bus_ids, report);
    check_effect_automation(composition, report);
    crate::voice_instruments::check_instruments(composition, voice_ids, report);
    crate::audio_clips::check_clips(composition, voice_ids, report);
}

/// Checks effect lanes: the addressed effect and parameter exist, values are
/// in the parameter's range, points are ordered and loops are consistent.
fn check_effect_automation(composition: &Composition, report: &mut Report) {
    let mix = &composition.mix;
    let end = composition.playback.end;
    let mut keys = HashSet::new();
    for (i, lane) in mix.effect_automation.iter().enumerate() {
        let at = |rest: &str| format!("mix/effect_automation/{i}{rest}");
        let key = lane.key();
        let name = lane.parameter.name();
        if !keys.insert(key.clone()) {
            report.push(
                Code::DuplicateControlLane,
                at(""),
                "each effect parameter has at most one automation lane",
            );
        }
        let Some(effect) = mix.effect(&lane.target, &lane.effect) else {
            report.push(
                Code::UnknownReference,
                at("/effect"),
                format!(
                    "{} has no effect `{}`, but its {name} lane remains; clear the lane with set_effect_automation and empty points in the request that removes the effect",
                    lane.target, lane.effect
                ),
            );
            continue;
        };
        let kind = effect.processor.kind();
        if !lane.parameter.kinds().contains(&kind) {
            report.push(
                Code::InvalidRange,
                at("/parameter"),
                format!(
                    "effect `{}` is a {kind}; {name} belongs to {}",
                    lane.effect,
                    lane.parameter.kinds().join(" and ")
                ),
            );
            continue;
        }
        match (lane.parameter.per_band(), lane.band) {
            (true, None) => {
                report.push(
                    Code::InvalidRange,
                    at("/band"),
                    format!("{name} needs the 0-based band of the equalizer"),
                );
                continue;
            }
            (false, Some(_)) => {
                report.push(
                    Code::InvalidRange,
                    at("/band"),
                    format!("{name} has no band; only gain_db and frequency_hz take one"),
                );
                continue;
            }
            _ => {}
        }
        if lane
            .parameter
            .static_value(&effect.processor, lane.band)
            .is_none()
        {
            let bands = match &effect.processor {
                effects::Processor::Equalizer(eq) => eq.bands.len(),
                _ => 0,
            };
            let message = if lane.band.is_some_and(|b| usize::from(b) >= bands) {
                format!(
                    "effect `{}` has bands 0–{}",
                    lane.effect,
                    bands.saturating_sub(1)
                )
            } else {
                "gain_db applies to peaking and shelf bands only".to_owned()
            };
            report.push(Code::InvalidRange, at("/band"), message);
            continue;
        }
        if lane.points.is_empty() {
            report.push(
                Code::EmptyControlLane,
                at("/points"),
                "remove the lane instead of leaving it empty",
            );
            continue;
        }
        let (low, high, unit) = lane.parameter.range();
        check_shapes(
            &lane.points,
            lane.parameter.ratio_scale(),
            name,
            report,
            |j| at(&format!("/points/{j}/shape")),
        );
        for (j, point) in lane.points.iter().enumerate() {
            let value = point.value.value();
            if !(low..=high).contains(&value) {
                report.push(
                    Code::ValueOutOfRange,
                    at(&format!("/points/{j}/value")),
                    format!("{name} must be {low}–{high}{unit}"),
                );
            } else if lane.parameter.whole() && point.value.0 % 10 != 0 {
                report.push(
                    Code::ValueOutOfRange,
                    at(&format!("/points/{j}/value")),
                    format!("{name} is a register value and must be a whole number"),
                );
            }
            if j > 0 && point.at <= lane.points[j - 1].at {
                report.push(
                    Code::ControlPointsUnordered,
                    at(&format!("/points/{j}/at")),
                    "automation points must have increasing times",
                );
            }
            if point.at >= end && end > 0 {
                report.push(
                    Code::EventBeyondEnd,
                    at(&format!("/points/{j}/at")),
                    "automation points must be before the playback end",
                );
            }
        }
        if let Some(loop_start) = composition.playback.loop_start
            && !lane.points.iter().any(|p| p.at == loop_start)
            && let Some(curve) = mix.effect_curve(&key, &lane.points)
        {
            let (at_loop, at_end) = (curve.at(loop_start as f64), curve.at(end as f64));
            if (at_loop - at_end).abs() > SAME_VALUE {
                report.push(
                    Code::LoopStateMismatch,
                    at(""),
                    format!("{name} is {at_end} at the end but {at_loop} at the loop start; add a point at {loop_start}"),
                );
            }
        }
    }
}

/// Applies one mixer operation of an edit request.
pub(crate) fn apply_mix_operation(
    composition: &mut Composition,
    operation: &EditOperation,
    report: &mut Report,
) {
    let mix = &mut composition.mix;
    match operation {
        EditOperation::SetMix {
            target,
            volume_db,
            pan,
            mute,
            solo,
            output,
        } => {
            if volume_db.is_none()
                && pan.is_none()
                && mute.is_none()
                && solo.is_none()
                && output.is_none()
            {
                report.push(Code::EmptyEdit, "", "set at least one mixer field");
                return;
            }
            match target {
                MixTarget::Voice { id } => {
                    if !composition.voices.iter().any(|v| &v.id == id) {
                        report.push(
                            Code::UnknownReference,
                            "target/id",
                            format!("voice `{id}` does not exist"),
                        );
                        return;
                    }
                    let index = match mix.voices.iter().position(|s| &s.voice == id) {
                        Some(index) => index,
                        None => {
                            mix.voices.push(VoiceStrip {
                                voice: id.clone(),
                                volume_db: Tenths(0),
                                pan: Tenths(0),
                                mute: false,
                                solo: false,
                                output: None,
                            });
                            mix.voices.len() - 1
                        }
                    };
                    let strip = &mut mix.voices[index];
                    strip.volume_db = volume_db.unwrap_or(strip.volume_db);
                    strip.pan = pan.unwrap_or(strip.pan);
                    strip.mute = mute.unwrap_or(strip.mute);
                    strip.solo = solo.unwrap_or(strip.solo);
                    if let Some(output) = output {
                        strip.output = (output != MASTER).then(|| output.clone());
                    }
                    if strip.is_neutral() {
                        mix.voices.remove(index);
                    }
                }
                MixTarget::Bus { id } => {
                    if solo.is_some() || output.is_some() {
                        report.push(
                            Code::InvalidRange,
                            "",
                            "buses have no solo or output; they sum into the master",
                        );
                    }
                    let Some(bus) = mix.buses.iter_mut().find(|b| &b.id == id) else {
                        report.push(
                            Code::UnknownReference,
                            "target/id",
                            format!("bus `{id}` does not exist; add it with add_bus"),
                        );
                        return;
                    };
                    bus.volume_db = volume_db.unwrap_or(bus.volume_db);
                    bus.pan = pan.unwrap_or(bus.pan);
                    bus.mute = mute.unwrap_or(bus.mute);
                }
                MixTarget::Master => {
                    if pan.is_some() || mute.is_some() || solo.is_some() || output.is_some() {
                        report.push(Code::InvalidRange, "", "the master has volume_db only");
                    }
                    mix.master_volume_db = volume_db.unwrap_or(mix.master_volume_db);
                }
            }
        }
        EditOperation::AddBus { bus } => {
            if mix.bus(&bus.id).is_some() {
                report.push(
                    Code::DuplicateId,
                    "bus/id",
                    format!("bus `{}` already exists", bus.id),
                );
                return;
            }
            mix.buses.push(bus.clone());
        }
        EditOperation::RemoveBus { id } => {
            let Some(index) = mix.buses.iter().position(|b| &b.id == id) else {
                report.push(
                    Code::UnknownReference,
                    "id",
                    format!("bus `{id}` does not exist"),
                );
                return;
            };
            mix.buses.remove(index);
            mix.automation
                .retain(|lane| !matches!(&lane.target, MixTarget::Bus { id: bus } if bus == id));
            mix.effect_automation
                .retain(|lane| !matches!(&lane.target, MixTarget::Bus { id: bus } if bus == id));
            effects::remove_chain(&mut mix.effects, &MixTarget::Bus { id: id.clone() });
        }
        EditOperation::SetSend { send } => {
            match mix
                .sends
                .iter_mut()
                .find(|s| s.voice == send.voice && s.bus == send.bus)
            {
                Some(existing) => *existing = send.clone(),
                None => mix.sends.push(send.clone()),
            }
        }
        EditOperation::RemoveSend { voice, bus } => {
            let before = mix.sends.len();
            mix.sends.retain(|s| !(&s.voice == voice && &s.bus == bus));
            if mix.sends.len() == before {
                report.push(
                    Code::UnknownReference,
                    "",
                    format!("voice `{voice}` has no send to bus `{bus}`"),
                );
            }
        }
        EditOperation::SetMixAutomation {
            target,
            parameter,
            points,
        } => {
            mix.automation
                .retain(|lane| !(&lane.target == target && lane.parameter == *parameter));
            if !points.is_empty() {
                mix.automation.push(MixLane {
                    target: target.clone(),
                    parameter: *parameter,
                    points: points.clone(),
                });
            }
        }
        EditOperation::SetEffectAutomation {
            target,
            effect,
            parameter,
            band,
            points,
        } => {
            let lane = EffectLane {
                target: target.clone(),
                effect: effect.clone(),
                parameter: *parameter,
                band: *band,
                points: points.clone(),
            };
            let key = lane.key();
            let before = mix.effect_automation.len();
            mix.effect_automation.retain(|l| l.key() != key);
            if points.is_empty() {
                if mix.effect_automation.len() == before {
                    report.push(
                        Code::UnknownReference,
                        "",
                        format!("there is no {} to clear", EffectLane::label(&key)),
                    );
                }
            } else {
                mix.effect_automation.push(lane);
            }
        }
        _ => unreachable!("only mixer operations are delegated here"),
    }
}

/// The half-open span where two curves of one parameter sound different.
fn differing_span(before: Curve, after: Curve, end: u64) -> Option<(u64, u64)> {
    let mut times: BTreeSet<u64> = before
        .points
        .iter()
        .chain(after.points)
        .map(|p| p.at)
        .collect();
    times.insert(0);
    let times: Vec<u64> = times.into_iter().filter(|t| *t < end).collect();
    let mut span: Option<(u64, u64)> = None;
    for (i, &start) in times.iter().enumerate() {
        let next = times.get(i + 1).copied().unwrap_or(end);
        let differs = (before.at(start as f64) - after.at(start as f64)).abs() > SAME_VALUE
            || (before.before(next as f64) - after.before(next as f64)).abs() > SAME_VALUE;
        if differs {
            span = Some(span.map_or((start, next), |(s, e)| (s.min(start), e.max(next))));
        }
    }
    span
}

/// Where a lane change is heard: each added or removed point, and the span
/// where the values differ. A missing curve (its effect absent in that
/// version) compares as the other version's static value.
fn lane_spans(
    before_points: &[MixPoint],
    after_points: &[MixPoint],
    before: Option<Curve>,
    after: Option<Curve>,
    end: u64,
) -> Vec<Option<(u64, u64)>> {
    let mut spans: Vec<Option<(u64, u64)>> = before_points
        .iter()
        .filter(|p| !after_points.contains(p))
        .chain(after_points.iter().filter(|p| !before_points.contains(p)))
        .map(|p| Some((p.at, p.at.saturating_add(1))))
        .collect();
    let flat = |curve: Curve| Curve {
        points: &[],
        base: curve.base,
    };
    let curves = match (before, after) {
        (Some(before), Some(after)) => Some((before, after)),
        (Some(before), None) => Some((before, flat(before))),
        (None, Some(after)) => Some((flat(after), after)),
        (None, None) => None,
    };
    if let Some((before, after)) = curves {
        spans.extend(differing_span(before, after, end).map(Some));
    }
    spans
}

/// Reports mixer changes outside the request's scope or inside its
/// protections. Voice strips, sends and voice automation belong to their
/// voice; buses and the master need `scope.buses`.
pub(crate) fn check_mix_bounds(
    base: &Composition,
    result: &Composition,
    request: &EditRequest,
    report: &mut Report,
) {
    let (old, new) = (&base.mix, &result.mix);
    if old == new {
        return;
    }
    let end = base.playback.end.max(result.playback.end);
    let mut voices: BTreeSet<&str> = BTreeSet::new();
    voices.extend(
        old.voices
            .iter()
            .chain(&new.voices)
            .map(|s| s.voice.as_str()),
    );
    for voice in voices {
        if old.strip(voice) != new.strip(voice) {
            check_change(
                request,
                &[(voice, None)],
                report,
                format!("mixer strip of `{voice}`"),
            );
        }
    }
    let sends = |mix: &Mix| -> BTreeMap<(String, String), MixSend> {
        mix.sends
            .iter()
            .map(|s| ((s.voice.clone(), s.bus.clone()), s.clone()))
            .collect()
    };
    let (old_sends, new_sends) = (sends(old), sends(new));
    for key in old_sends
        .keys()
        .chain(new_sends.keys())
        .collect::<BTreeSet<_>>()
    {
        if old_sends.get(key) != new_sends.get(key) {
            check_change(
                request,
                &[(&key.0, None)],
                report,
                format!("send from `{}` to `{}`", key.0, key.1),
            );
        }
    }

    let mut shared: Vec<(String, Option<(u64, u64)>)> = Vec::new();
    let buses = |mix: &Mix| -> BTreeMap<String, Bus> {
        mix.buses
            .iter()
            .map(|b| (b.id.clone(), b.clone()))
            .collect()
    };
    let (old_buses, new_buses) = (buses(old), buses(new));
    for id in old_buses
        .keys()
        .chain(new_buses.keys())
        .collect::<BTreeSet<_>>()
    {
        if old_buses.get(id) != new_buses.get(id) {
            shared.push((format!("bus `{id}`"), None));
        }
    }
    if old.master_volume_db != new.master_volume_db {
        shared.push(("the master volume".into(), None));
    }
    crate::audio_clips::check_clip_bounds(&old.clips, &new.clips, request, report);
    for voice in crate::voice_instruments::changed_voices(&old.instruments, &new.instruments) {
        check_change(
            request,
            &[(voice, None)],
            report,
            format!("instrument of `{voice}`"),
        );
    }
    let chains: BTreeSet<&MixTarget> = old
        .effects
        .iter()
        .chain(&new.effects)
        .map(|c| &c.target)
        .collect();
    for target in chains {
        if old.chain(target) == new.chain(target) {
            continue;
        }
        match target {
            MixTarget::Voice { id } => check_change(
                request,
                &[(id.as_str(), None)],
                report,
                format!("effect chain of `{id}`"),
            ),
            _ => shared.push((format!("effect chain of {target}"), None)),
        }
    }

    let keys: BTreeSet<(MixTarget, MixParameter)> = old
        .automation
        .iter()
        .chain(&new.automation)
        .map(|l| (l.target.clone(), l.parameter))
        .collect();
    for (target, parameter) in keys {
        let (before, after) = (old.lane(&target, parameter), new.lane(&target, parameter));
        if before == after {
            continue;
        }
        let (before_points, after_points) = (
            before.map(|l| l.points.as_slice()).unwrap_or_default(),
            after.map(|l| l.points.as_slice()).unwrap_or_default(),
        );
        let spans = lane_spans(
            before_points,
            after_points,
            Some(old.curve(&target, parameter)),
            Some(new.curve(&target, parameter)),
            end,
        );
        let what = format!("{parameter:?} automation of {target}");
        match &target {
            MixTarget::Voice { id } => {
                let versions: Vec<(&str, Option<(u64, u64)>)> =
                    spans.into_iter().map(|span| (id.as_str(), span)).collect();
                check_change(request, &versions, report, what);
            }
            _ => shared.extend(spans.into_iter().map(|span| (what.clone(), span))),
        }
    }

    let effect_keys: BTreeSet<EffectLaneKey> = old
        .effect_automation
        .iter()
        .chain(&new.effect_automation)
        .map(EffectLane::key)
        .collect();
    for key in effect_keys {
        let (before, after) = (old.effect_lane(&key), new.effect_lane(&key));
        if before == after {
            // An unchanged lane must not follow its effect id to another kind.
            let kinds = (
                old.effect(&key.0, &key.1).map(|e| e.processor.kind()),
                new.effect(&key.0, &key.1).map(|e| e.processor.kind()),
            );
            if let (Some(was), Some(now)) = kinds
                && was != now
            {
                let index = new
                    .effect_automation
                    .iter()
                    .position(|l| l.key() == key)
                    .unwrap_or_default();
                report.push(
                    Code::InvalidRange,
                    format!("result/mix/effect_automation/{index}"),
                    format!(
                        "effect `{}` changed from a {was} to a {now} while its {} lane stayed; set or clear the lane in the same request",
                        key.1,
                        key.2.name()
                    ),
                );
            }
            continue;
        }
        let (before_points, after_points) = (
            before.map(|l| l.points.as_slice()).unwrap_or_default(),
            after.map(|l| l.points.as_slice()).unwrap_or_default(),
        );
        let spans = lane_spans(
            before_points,
            after_points,
            old.effect_curve(&key, before_points),
            new.effect_curve(&key, after_points),
            end,
        );
        let what = EffectLane::label(&key);
        match &key.0 {
            MixTarget::Voice { id } => {
                let versions: Vec<(&str, Option<(u64, u64)>)> =
                    spans.into_iter().map(|span| (id.as_str(), span)).collect();
                check_change(request, &versions, report, what);
            }
            _ => shared.extend(spans.into_iter().map(|span| (what.clone(), span))),
        }
    }

    for (what, span) in shared {
        if !request.scope.buses {
            report.push(
                Code::OutOfScope,
                "scope/buses",
                format!("{what} changes without buses scope"),
            );
        } else if let Some(range) = request.scope.range
            && !span.is_some_and(|(start, end)| range.start <= start && end <= range.end)
        {
            report.push(
                Code::OutOfScope,
                "scope/range",
                format!("{what} changes outside the request range"),
            );
        }
    }
}

/// Labels of mixer items that differ, with the voice each belongs to.
pub(crate) fn mix_changes(before: &Mix, after: &Mix) -> Vec<(Option<String>, String)> {
    let mut changes = Vec::new();
    let mut voices: BTreeSet<&str> = BTreeSet::new();
    voices.extend(
        before
            .voices
            .iter()
            .chain(&after.voices)
            .map(|s| s.voice.as_str()),
    );
    for voice in voices {
        if before.strip(voice) != after.strip(voice) {
            changes.push((Some(voice.to_owned()), format!("strip of voice `{voice}`")));
        }
    }
    let mut buses: BTreeSet<&str> = BTreeSet::new();
    buses.extend(
        before
            .buses
            .iter()
            .chain(&after.buses)
            .map(|b| b.id.as_str()),
    );
    for bus in buses {
        if before.bus(bus) != after.bus(bus) {
            changes.push((None, format!("bus `{bus}`")));
        }
    }
    let mut sends: BTreeSet<(&str, &str)> = BTreeSet::new();
    sends.extend(
        before
            .sends
            .iter()
            .chain(&after.sends)
            .map(|s| (s.voice.as_str(), s.bus.as_str())),
    );
    for (voice, bus) in sends {
        let find = |mix: &Mix| {
            mix.sends
                .iter()
                .find(|s| s.voice == voice && s.bus == bus)
                .cloned()
        };
        if find(before) != find(after) {
            changes.push((
                Some(voice.to_owned()),
                format!("send from `{voice}` to `{bus}`"),
            ));
        }
    }
    if before.master_volume_db != after.master_volume_db {
        changes.push((None, "master volume".into()));
    }
    let keys: BTreeSet<(MixTarget, MixParameter)> = before
        .automation
        .iter()
        .chain(&after.automation)
        .map(|l| (l.target.clone(), l.parameter))
        .collect();
    for (target, parameter) in keys {
        if before.lane(&target, parameter) != after.lane(&target, parameter) {
            let voice = match &target {
                MixTarget::Voice { id } => Some(id.clone()),
                _ => None,
            };
            changes.push((
                voice,
                format!(
                    "{} automation of {target}",
                    format!("{parameter:?}").to_lowercase()
                ),
            ));
        }
    }
    let chains: BTreeSet<&MixTarget> = before
        .effects
        .iter()
        .chain(&after.effects)
        .map(|c| &c.target)
        .collect();
    for target in chains {
        if before.chain(target) != after.chain(target) {
            let voice = match target {
                MixTarget::Voice { id } => Some(id.clone()),
                _ => None,
            };
            changes.push((voice, format!("effects of {target}")));
        }
    }
    let effect_keys: BTreeSet<EffectLaneKey> = before
        .effect_automation
        .iter()
        .chain(&after.effect_automation)
        .map(EffectLane::key)
        .collect();
    for key in effect_keys {
        if before.effect_lane(&key) != after.effect_lane(&key) {
            let voice = match &key.0 {
                MixTarget::Voice { id } => Some(id.clone()),
                _ => None,
            };
            changes.push((voice, EffectLane::label(&key)));
        }
    }
    for voice in crate::voice_instruments::changed_voices(&before.instruments, &after.instruments) {
        changes.push((
            Some(voice.to_owned()),
            format!("instrument of voice `{voice}`"),
        ));
    }
    changes.extend(crate::audio_clips::clip_labels(&before.clips, &after.clips));
    changes
}

/// The mixer of an excerpt `[start, end)` shifted to start at 0: values in
/// effect at `start` become points at 0, and a ramp crossing `end` ends there.
pub(crate) fn excerpt_mix(mix: &Mix, start: u64, end: u64) -> Mix {
    let automation = mix
        .automation
        .iter()
        .filter_map(|lane| {
            let points = excerpt_points(mix.curve(&lane.target, lane.parameter), start, end);
            (!points.is_empty()).then(|| MixLane {
                points,
                ..lane.clone()
            })
        })
        .collect();
    let effect_automation = mix
        .effect_automation
        .iter()
        .filter_map(|lane| {
            let curve = mix.effect_curve(&lane.key(), &lane.points)?;
            let points = excerpt_points(curve, start, end);
            (!points.is_empty()).then(|| EffectLane {
                points,
                ..lane.clone()
            })
        })
        .collect();
    // Clips are placed from the full composition (`audio_clips::layout_placements`).
    Mix {
        automation,
        effect_automation,
        clips: vec![],
        ..mix.clone()
    }
}

/// A lane's points for an excerpt `[start, end)` shifted to start at 0.
fn excerpt_points(curve: Curve, start: u64, end: u64) -> Vec<MixPoint> {
    let all = curve.points;
    let held = all.iter().take_while(|p| p.at <= start).count();
    let mut points = Vec::new();
    let shape_from = |index: usize| match (all[index].shape, all.get(index + 1)) {
        (shape @ (MixShape::Linear | MixShape::Exponential), Some(_)) => shape,
        _ => MixShape::Step,
    };
    if held > 0 {
        points.push(MixPoint {
            at: 0,
            value: Tenths((curve.at(start as f64) * 10.0).round() as i32),
            shape: shape_from(held - 1),
        });
    }
    points.extend(
        all.iter()
            .filter(|p| p.at > start && p.at < end)
            .map(|p| MixPoint {
                at: p.at - start,
                ..*p
            }),
    );
    if points.last().is_some_and(|p| p.shape != MixShape::Step) {
        points.push(MixPoint {
            at: end - start,
            value: Tenths((curve.before(end as f64) * 10.0).round() as i32),
            shape: MixShape::Step,
        });
    }
    points
}

/// A stretch of rendered samples where a parameter is constant or ramps.
struct Segment {
    /// First and one-past-last sample index.
    samples: (usize, usize),
    /// Exact sample positions of the start and end, for interpolation.
    positions: (f64, f64),
    /// Value at the start and approaching the end.
    values: (f64, f64),
    /// Interpolate geometrically rather than linearly.
    geometric: bool,
}

/// Mixes voice renders into stereo output following a revision's mixer.
/// Positions follow the render layout: loop passes replay `[loop_start, end)`.
pub(crate) struct Mixer<'a> {
    mix: &'a Mix,
    /// `(first tick, last tick, seconds added)` of each pass.
    windows: Vec<(u64, u64, Rational)>,
    timebase: Timebase,
    composition: &'a Composition,
    sample_rate: u32,
    length: usize,
    master: [Vec<f32>; 2],
    buses: BTreeMap<String, [Vec<f32>; 2]>,
    /// `(first sample, microseconds per quarter)` of each tempo stretch.
    tempo: Vec<(usize, u32)>,
    /// Strips whose post-chain, pre-fader signal keys a sidechain.
    key_sources: BTreeSet<MixTarget>,
    keys: BTreeMap<MixTarget, [Vec<f32>; 2]>,
    /// Stems followed through the mix beside the mixdown.
    tracks: Vec<Track>,
    pre_master: bool,
}

/// Stems a render follows through the mixer beside its mixdown, for
/// `render_batch`. [`RenderPart::Mixdown`] parts are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StemCapture {
    pub parts: Vec<RenderPart>,
    /// Stems stop before the master volume and master chain.
    pub pre_master: bool,
}

/// One captured stem.
#[derive(Debug, Clone, PartialEq)]
pub struct StemAudio {
    pub part: RenderPart,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    /// Shared effects, such as ``effect `echo` on bus `band` ``, whose
    /// processing of this stem only approximates its share of the mix.
    pub approximated_by: Vec<String>,
}

/// What a mixer produced: the mixdown, the captured stems, and with
/// pre-master stems the master input they sum to.
pub(crate) struct MixOutput {
    pub mixdown: [Vec<f32>; 2],
    pub stems: Vec<StemAudio>,
    pub pre_master_mix: Option<[Vec<f32>; 2]>,
}

/// A stem on its way through the mixer: what has reached each bus (`Some`)
/// and the master (`None`) so far.
struct Track {
    part: RenderPart,
    inputs: BTreeMap<Option<String>, [Vec<f32>; 2]>,
    approximated_by: Vec<String>,
}

impl Track {
    fn takes(&self, voice: &str, bus: Option<&str>) -> bool {
        match &self.part {
            RenderPart::Voice { id } => id == voice,
            RenderPart::Group { voices, .. } => voices.iter().any(|v| v == voice),
            RenderPart::Direct => bus.is_none(),
            RenderPart::Bus { .. } | RenderPart::Mixdown => false,
        }
    }
}

/// Adds `signal × gain` to `out`, rounding to f32 like the mixer's sums.
fn accumulate(out: &mut [Vec<f32>; 2], signal: &[Vec<f32>; 2], gain: f64) {
    for (out, input) in out.iter_mut().zip(signal) {
        for (o, i) in out.iter_mut().zip(input) {
            *o = (f64::from(*o) + f64::from(*i) * gain) as f32;
        }
    }
}

/// The input of `track` at `bus` (`None` for the master), created silent.
fn track_input<'t>(
    track: &'t mut Track,
    bus: Option<&str>,
    length: usize,
) -> &'t mut [Vec<f32>; 2] {
    track
        .inputs
        .entry(bus.map(str::to_owned))
        .or_insert_with(|| [vec![0.0; length], vec![0.0; length]])
}

/// Voices whose sound a part holds: the voice, the group, the voices
/// routed or sending to a bus, or those routed straight to the master.
pub fn part_voices(mix: &Mix, voices: &[String], part: &RenderPart) -> Vec<String> {
    let output = |voice: &str| mix.strip(voice).and_then(|s| s.output.clone());
    voices
        .iter()
        .filter(|voice| match part {
            RenderPart::Mixdown => true,
            RenderPart::Voice { id } => id == *voice,
            RenderPart::Group { voices, .. } => voices.contains(voice),
            RenderPart::Direct => output(voice).is_none(),
            RenderPart::Bus { id } => {
                output(voice).as_deref() == Some(id.as_str())
                    || mix.sends.iter().any(|s| &s.voice == *voice && &s.bus == id)
            }
        })
        .cloned()
        .collect()
}

/// Indices in an order where each item follows the items it depends on;
/// without dependencies, the original order.
fn dependency_order(count: usize, dependencies: impl Fn(usize) -> Vec<usize>) -> Vec<usize> {
    let mut done = vec![false; count];
    let mut order = Vec::with_capacity(count);
    while order.len() < count {
        let ready = (0..count).find(|&i| !done[i] && dependencies(i).iter().all(|&d| done[d]));
        // Composition checks reject cycles; an unchecked one keeps the given order.
        let next = ready.unwrap_or_else(|| (0..count).find(|&i| !done[i]).expect("one is left"));
        done[next] = true;
        order.push(next);
    }
    order
}

fn amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Stereo balance gains: the center leaves both sides unchanged.
fn balance(pan: f64) -> (f64, f64) {
    let p = (pan / MAX_PAN).clamp(-1.0, 1.0);
    (
        if p > 0.0 { 1.0 - p } else { 1.0 },
        if p < 0.0 { 1.0 + p } else { 1.0 },
    )
}

impl<'a> Mixer<'a> {
    /// `end` is the last rendered position of each pass, `passes` the number
    /// of times content plays, and `length` the samples including the tail.
    pub(crate) fn new(
        timebase: Timebase,
        composition: &'a Composition,
        mix: &'a Mix,
        end: u64,
        passes: u32,
        sample_rate: u32,
        length: usize,
    ) -> Self {
        let seconds =
            |tick: u64| seconds_at(timebase, composition, Rational::from_integer(tick.into()));
        let mut windows = vec![(0, end, Rational::from_integer(0))];
        if let Some(loop_start) = composition.playback.loop_start {
            let loop_length = seconds(end) - seconds(loop_start);
            for pass in 1..passes {
                windows.push((loop_start, end, loop_length * i128::from(pass)));
            }
        }
        let mut tempo = Vec::new();
        if matches!(timebase, Timebase::Music { .. }) && !mix.effects.is_empty() {
            for &(first, last, offset) in &windows {
                let sample = |tick: u64| sample_at(seconds(tick) + offset, sample_rate) as usize;
                let map = &composition.tempo_map;
                if let Some(held) = map.iter().take_while(|t| t.tick <= first).last() {
                    tempo.push((sample(first), held.microseconds_per_quarter));
                }
                tempo.extend(
                    map.iter()
                        .filter(|t| first < t.tick && t.tick < last)
                        .map(|t| (sample(t.tick), t.microseconds_per_quarter)),
                );
            }
        }
        let key_sources = mix
            .effects
            .iter()
            .flat_map(|c| mix.keys_of(&c.target))
            .cloned()
            .collect();
        Self {
            mix,
            windows,
            timebase,
            composition,
            sample_rate,
            length,
            master: [vec![0.0; length], vec![0.0; length]],
            buses: mix
                .buses
                .iter()
                .map(|b| (b.id.clone(), [vec![0.0; length], vec![0.0; length]]))
                .collect(),
            tempo,
            key_sources,
            keys: BTreeMap::new(),
            tracks: Vec::new(),
            pre_master: false,
        }
    }

    /// Follows the stems of `capture` through the mix. The mixdown is
    /// unchanged by it.
    pub(crate) fn capture(&mut self, capture: &StemCapture) {
        self.pre_master = capture.pre_master;
        self.tracks = capture
            .parts
            .iter()
            .filter(|p| **p != RenderPart::Mixdown)
            .map(|part| Track {
                part: part.clone(),
                inputs: BTreeMap::new(),
                approximated_by: Vec::new(),
            })
            .collect();
    }

    fn context(&self) -> EffectContext<'_> {
        EffectContext {
            sample_rate: self.sample_rate,
            tempo: &self.tempo,
        }
    }

    /// Runs `target`'s effect chain in place and keeps the result when it
    /// keys a sidechain. A key source not heard in this render is silent.
    fn insert(&mut self, target: &MixTarget, signal: &mut [Vec<f32>; 2]) {
        self.insert_shared(target, signal, &mut []);
    }

    /// [`Self::insert`] that also runs `shares` of the signal through the
    /// chain ([`effects::process_shared`]). Returns the effects whose shares
    /// only approximate the processed signal.
    fn insert_shared(
        &mut self,
        target: &MixTarget,
        signal: &mut [Vec<f32>; 2],
        shares: &mut [&mut [Vec<f32>; 2]],
    ) -> Vec<String> {
        let mix = self.mix;
        let mut approximated = Vec::new();
        if let Some(chain) = mix.chain(target) {
            let context = self.context();
            for effect in chain.effects.iter().filter(|e| !e.bypass) {
                let key = match effect.processor.sidechain() {
                    None => Key::Own,
                    Some(source) => self.keys.get(source).map_or(Key::Silent, Key::External),
                };
                let mut automation = Automation::default();
                for lane in mix
                    .effect_automation
                    .iter()
                    .filter(|l| &l.target == target && l.effect == effect.id)
                {
                    // Composition checks reject lanes whose parameter is missing.
                    if let Some(curve) = mix.effect_curve(&lane.key(), &lane.points) {
                        automation.push(lane.parameter, lane.band, self.curve_values(curve));
                    }
                }
                let linear = effects::process_shared(
                    &effect.processor,
                    signal,
                    key,
                    &context,
                    &automation,
                    shares,
                );
                if !linear && !shares.is_empty() {
                    approximated.push(format!("effect `{}` on {target}", effect.id));
                }
            }
        }
        if self.key_sources.contains(target) {
            self.keys.insert(target.clone(), signal.clone());
        }
        approximated
    }

    /// Order to add `voices` in, so sidechain key voices come first.
    pub(crate) fn voice_order(&self, voices: &[&str]) -> Vec<usize> {
        dependency_order(voices.len(), |i| {
            self.mix
                .keys_of(&MixTarget::Voice {
                    id: voices[i].to_owned(),
                })
                .into_iter()
                .filter_map(|key| match key {
                    MixTarget::Voice { id } => voices.iter().position(|v| v == id),
                    _ => None,
                })
                .collect()
        })
    }

    /// Silent samples the output needs before the tail can end: the
    /// summed delay-line lengths of every active time-based effect.
    pub(crate) fn silence_hold(&self) -> usize {
        let context = self.context();
        self.mix
            .effects
            .iter()
            .flat_map(|c| &c.effects)
            .filter(|e| !e.bypass)
            .map(|e| effects::hold_samples(&e.processor, &context))
            .sum()
    }

    fn values(&self, target: &MixTarget, parameter: MixParameter) -> Values {
        self.curve_values(self.mix.curve(target, parameter))
    }

    /// A curve evaluated at every rendered sample of the layout.
    fn curve_values(&self, curve: Curve) -> Values {
        if curve.points.is_empty() {
            return Values::Constant(curve.base);
        }
        let rate = i128::from(self.sample_rate);
        let tempo_ticks: Vec<u64> = match self.timebase {
            Timebase::Music { .. } => self.composition.tempo_map.iter().map(|t| t.tick).collect(),
            Timebase::Effect => vec![],
        };
        let mut segments = Vec::new();
        let mut hold = curve.base;
        for &(first, last, offset) in &self.windows {
            let mut ticks: BTreeSet<u64> = curve
                .points
                .iter()
                .map(|p| p.at)
                .chain(tempo_ticks.iter().copied())
                .filter(|t| first < *t && *t < last)
                .collect();
            ticks.insert(first);
            ticks.insert(last);
            let ticks: Vec<u64> = ticks.into_iter().collect();
            // A value takes effect at the first sample at or after its exact time.
            let sample = |tick: u64| {
                let seconds = seconds_at(
                    self.timebase,
                    self.composition,
                    Rational::from_integer(tick.into()),
                ) + offset;
                let exact = seconds * rate;
                (
                    sample_at(seconds, self.sample_rate) as usize,
                    *exact.numer() as f64 / *exact.denom() as f64,
                )
            };
            for pair in ticks.windows(2) {
                let ((first_sample, start), (next_sample, end)) =
                    (sample(pair[0]), sample(pair[1]));
                segments.push(Segment {
                    samples: (first_sample, next_sample),
                    positions: (start, end),
                    values: (curve.at(pair[0] as f64), curve.before(pair[1] as f64)),
                    geometric: curve.geometric_at(pair[0] as f64),
                });
            }
            hold = curve.at(last as f64);
        }
        let constant = segments.iter().all(|s| {
            (s.values.0 - hold).abs() <= SAME_VALUE && (s.values.1 - hold).abs() <= SAME_VALUE
        });
        if constant {
            return Values::Constant(hold);
        }
        let mut out = vec![hold; self.length];
        for segment in &segments {
            let (from, to) = segment.values;
            let (start, end) = segment.positions;
            let last = segment.samples.1.min(self.length);
            for (n, value) in out
                .iter_mut()
                .enumerate()
                .take(last)
                .skip(segment.samples.0)
            {
                *value = if from == to || end <= start {
                    from
                } else if segment.geometric {
                    geometric(from, to, (n as f64 - start) / (end - start))
                } else {
                    from + (to - from) * (n as f64 - start) / (end - start)
                };
            }
        }
        Values::Varying(out)
    }

    /// Applies volume and pan of `target` to a stereo signal in place.
    fn strip(&self, target: &MixTarget, signal: &mut [Vec<f32>; 2]) {
        let volume = self.values(target, MixParameter::Volume);
        let pan = match target {
            MixTarget::Master => Values::Constant(0.0),
            _ => self.values(target, MixParameter::Pan),
        };
        let [left, right] = signal;
        for n in 0..self.length.min(left.len()) {
            let gain = amplitude(volume.get(n));
            let (l, r) = balance(pan.get(n));
            left[n] = (f64::from(left[n]) * gain * l) as f32;
            right[n] = (f64::from(right[n]) * gain * r) as f32;
        }
    }

    /// Samples per channel, including the tail.
    pub(crate) fn length(&self) -> usize {
        self.length
    }

    pub(crate) fn audible(&self, voice: &str) -> bool {
        self.mix.audible(voice)
    }

    /// Adds one voice's dry render to its output and send buses.
    pub(crate) fn add_voice(&mut self, voice: &str, dry: [Vec<f32>; 2]) {
        let mut dry = dry;
        for side in &mut dry {
            side.resize(self.length, 0.0);
        }
        self.insert(
            &MixTarget::Voice {
                id: voice.to_owned(),
            },
            &mut dry,
        );
        let sends: Vec<&MixSend> = self.mix.sends.iter().filter(|s| s.voice == voice).collect();
        for send in sends.iter().filter(|s| s.pre_fader) {
            let gain = amplitude(send.gain_db.value());
            self.feed(Some(&send.bus), &dry, gain);
            self.track_feed(voice, Some(&send.bus), &dry, gain);
        }
        let mut wet = dry;
        self.strip(
            &MixTarget::Voice {
                id: voice.to_owned(),
            },
            &mut wet,
        );
        for send in sends.iter().filter(|s| !s.pre_fader) {
            let gain = amplitude(send.gain_db.value());
            self.feed(Some(&send.bus), &wet, gain);
            self.track_feed(voice, Some(&send.bus), &wet, gain);
        }
        let output = self.mix.strip(voice).and_then(|s| s.output.clone());
        self.feed(output.as_deref(), &wet, 1.0);
        self.track_feed(voice, output.as_deref(), &wet, 1.0);
    }

    /// Adds a feed of `voice` to the stems that hold it, like [`Self::feed`].
    fn track_feed(&mut self, voice: &str, bus: Option<&str>, signal: &[Vec<f32>; 2], gain: f64) {
        if bus.is_some_and(|bus| !self.buses.contains_key(bus)) {
            return;
        }
        let length = self.length;
        for track in self.tracks.iter_mut().filter(|t| t.takes(voice, bus)) {
            accumulate(track_input(track, bus, length), signal, gain);
        }
    }

    fn feed(&mut self, bus: Option<&str>, signal: &[Vec<f32>; 2], gain: f64) {
        // Composition checks reject unknown buses; an unchecked one is silent.
        let destination = match bus {
            Some(bus) => match self.buses.get_mut(bus) {
                Some(destination) => destination,
                None => return,
            },
            None => &mut self.master,
        };
        for (out, input) in destination.iter_mut().zip(signal) {
            for (o, i) in out.iter_mut().zip(input) {
                *o = (f64::from(*o) + f64::from(*i) * gain) as f32;
            }
        }
    }

    /// Runs bus chains and sums buses into the master, then applies the
    /// master volume and the master chain; carries every captured stem
    /// through the bus and master stages its signal reaches.
    pub(crate) fn finish(mut self) -> MixOutput {
        let length = self.length;
        let mut tracks = std::mem::take(&mut self.tracks);
        let mut buses = std::mem::take(&mut self.buses);
        let mix = self.mix;
        let order = dependency_order(mix.buses.len(), |i| {
            mix.keys_of(&MixTarget::Bus {
                id: mix.buses[i].id.clone(),
            })
            .into_iter()
            .filter_map(|key| match key {
                MixTarget::Bus { id } => mix.buses.iter().position(|b| &b.id == id),
                _ => None,
            })
            .collect()
        });
        for bus in order.into_iter().map(|i| &mix.buses[i]) {
            if bus.mute {
                continue;
            }
            let mut signal = buses.remove(&bus.id).expect("allocated per bus");
            let target = MixTarget::Bus { id: bus.id.clone() };
            let at = Some(bus.id.clone());
            let mut shares: Vec<(usize, [Vec<f32>; 2])> = tracks
                .iter_mut()
                .enumerate()
                .filter_map(|(i, t)| t.inputs.remove(&at).map(|share| (i, share)))
                .collect();
            let approximated = self.insert_shared(
                &target,
                &mut signal,
                &mut shares.iter_mut().map(|(_, s)| s).collect::<Vec<_>>(),
            );
            self.strip(&target, &mut signal);
            self.feed(None, &signal, 1.0);
            for (i, mut share) in shares {
                self.strip(&target, &mut share);
                let track = &mut tracks[i];
                track.approximated_by.extend(approximated.iter().cloned());
                accumulate(track_input(track, None, length), &share, 1.0);
            }
            for track in tracks
                .iter_mut()
                .filter(|t| matches!(&t.part, RenderPart::Bus { id } if *id == bus.id))
            {
                accumulate(track_input(track, None, length), &signal, 1.0);
            }
        }
        let mut master = std::mem::take(&mut self.master);
        let pre_master_mix = (self.pre_master && !tracks.is_empty()).then(|| master.clone());
        self.strip(&MixTarget::Master, &mut master);
        let mut shares: Vec<[Vec<f32>; 2]> = tracks
            .iter_mut()
            .map(|t| {
                t.inputs
                    .remove(&None)
                    .unwrap_or_else(|| [vec![0.0; length], vec![0.0; length]])
            })
            .collect();
        if self.pre_master {
            self.insert(&MixTarget::Master, &mut master);
        } else {
            for share in &mut shares {
                self.strip(&MixTarget::Master, share);
            }
            let approximated = self.insert_shared(
                &MixTarget::Master,
                &mut master,
                &mut shares.iter_mut().collect::<Vec<_>>(),
            );
            for track in &mut tracks {
                track.approximated_by.extend(approximated.iter().cloned());
            }
        }
        let stems = tracks
            .into_iter()
            .zip(shares)
            .map(|(track, [left, right])| StemAudio {
                part: track.part,
                left,
                right,
                approximated_by: track.approximated_by,
            })
            .collect();
        MixOutput {
            mixdown: master,
            stems,
            pre_master_mix,
        }
    }
}

impl Mix {
    /// Static settings with automation reduced to point counts, for inspection.
    pub fn summary(&self) -> serde_json::Value {
        let mut summary = serde_json::json!({
            "voices": self.voices, "buses": self.buses, "sends": self.sends,
            "master_volume_db": self.master_volume_db,
            "automation": self.automation.iter().map(|lane| serde_json::json!({
                "target": lane.target, "parameter": lane.parameter, "points": lane.points.len()})).collect::<Vec<_>>(),
            "effects": self.effects,
            "instruments": self.instruments,
            "applied_by_render": !self.is_empty(),
        });
        if !self.clips.is_empty() {
            summary["clips"] = serde_json::json!(self.clips);
        }
        if !self.effect_automation.is_empty() {
            summary["effect_automation"] = self
                .effect_automation
                .iter()
                .map(|lane| {
                    serde_json::json!({"target": lane.target, "effect": lane.effect,
                        "parameter": lane.parameter, "band": lane.band, "points": lane.points.len()})
                })
                .collect();
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RATE: u32 = 44_100;

    /// A composition with an EQ on `lead` whose band 0 frequency follows `points`.
    fn sweep(
        timebase: Timebase,
        tempo: serde_json::Value,
        end: u64,
        points: serde_json::Value,
    ) -> Composition {
        let mut composition: Composition = serde_json::from_value(json!({
            "tempo_map": tempo,
            "voices": [{"id": "lead"}],
            "notes": [],
            "playback": {"end": end},
        }))
        .unwrap();
        composition.mix = serde_json::from_value(json!({
            "effects": [{"target": {"kind": "voice", "id": "lead"}, "effects": [{"id": "tone",
                "processor": {"kind": "eq", "bands": [{"filter": "low_pass", "frequency_hz": 100.0, "q": 0.71}]}}]}],
            "effect_automation": [{"target": {"kind": "voice", "id": "lead"}, "effect": "tone",
                "parameter": "frequency_hz", "band": 0, "points": points}],
        }))
        .unwrap();
        let mut diagnostics = Vec::new();
        crate::composition::check_composition(
            timebase,
            &composition,
            &mut Report::new("", &mut diagnostics),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        composition
    }

    fn lane_values(
        timebase: Timebase,
        composition: &Composition,
        end: u64,
        length: usize,
    ) -> Vec<f64> {
        let mix = &composition.mix;
        let mixer = Mixer::new(timebase, composition, mix, end, 1, RATE, length);
        let lane = &mix.effect_automation[0];
        let curve = mix.effect_curve(&lane.key(), &lane.points).unwrap();
        match mixer.curve_values(curve) {
            Values::Varying(values) => values,
            Values::Constant(value) => vec![value; length],
        }
    }

    #[test]
    fn an_exponential_ramp_is_geometric_on_every_sample() {
        // Effect timebase: one tick is 1 µs, so 0.1 s and 0.2 s are samples 4410 and 8820.
        let composition = sweep(
            Timebase::Effect,
            json!([]),
            300_000,
            json!([{"at": 100000, "value": 100.0, "shape": "exponential"}, {"at": 200000, "value": 1600.0}]),
        );
        let values = lane_values(Timebase::Effect, &composition, 300_000, 13_230);
        for (n, &value) in values.iter().enumerate() {
            let expected = if n < 4410 {
                100.0
            } else if n < 8820 {
                100.0 * 16f64.powf((n as f64 - 4410.0) / 4410.0)
            } else {
                1600.0
            };
            assert!(
                (value / expected - 1.0).abs() < 1e-12,
                "sample {n}: {value} {expected}"
            );
        }
        // Halfway is two octaves up, not the linear midpoint of 850 Hz.
        assert!((values[6615] - 400.0).abs() < 1e-9);
        // The excerpt from 150000 µs starts at the ramp's value there, in the
        // lane's tenths, and keeps the exponential shape.
        let excerpt = excerpt_mix(&composition.mix, 150_000, 300_000);
        let points = &excerpt.effect_automation[0].points;
        assert_eq!(
            (points[0].at, points[0].value.0, points[0].shape),
            (0, 4000, MixShape::Exponential)
        );
        assert_eq!((points[1].at, points[1].value.0), (50_000, 16_000));
    }

    #[test]
    fn an_exponential_ramp_across_a_tempo_change_is_geometric_in_ticks() {
        // 480 PPQ: 500000 µs per quarter for the first beat, then 250000. A
        // ramp over ticks 0–960 from 100 to 400 Hz reaches 200 Hz at tick 480
        // (sample 22050) and 400 Hz at tick 960 (sample 33075).
        let timebase = Timebase::Music { ppq: 480 };
        let composition = sweep(
            timebase,
            json!([{"tick": 0, "microseconds_per_quarter": 500000},
                {"tick": 480, "microseconds_per_quarter": 250000}]),
            1920,
            json!([{"at": 0, "value": 100.0, "shape": "exponential"}, {"at": 960, "value": 400.0}]),
        );
        let values = lane_values(timebase, &composition, 1920, 44_100);
        for (n, &value) in values.iter().enumerate().take(33_075) {
            let tick = if n < 22_050 {
                n as f64 / 22_050.0 * 480.0
            } else {
                480.0 + (n as f64 - 22_050.0) / 11_025.0 * 480.0
            };
            let expected = 100.0 * 4f64.powf(tick / 960.0);
            assert!(
                (value / expected - 1.0).abs() < 1e-12,
                "sample {n}: {value} {expected}"
            );
        }
        assert!((values[22_050] - 200.0).abs() < 1e-9);
        assert!((values[33_075] - 400.0).abs() < 1e-9);
    }

    #[test]
    fn exponential_shapes_are_refused_off_ratio_scales() {
        let check = |composition: &Composition| {
            let mut diagnostics = Vec::new();
            let voices: HashSet<&str> = composition.voices.iter().map(|v| v.id.as_str()).collect();
            check_mix(composition, &voices, &mut Report::new("", &mut diagnostics));
            diagnostics
                .iter()
                .map(|d| (d.code, d.location.clone()))
                .collect::<Vec<_>>()
        };
        let mut composition = sweep(
            Timebase::Effect,
            json!([]),
            300_000,
            json!([{"at": 0, "value": 100.0, "shape": "exponential"}, {"at": 1000, "value": 200.0}]),
        );
        assert!(check(&composition).is_empty());
        composition.mix.automation = serde_json::from_value(json!([{"target": {"kind": "master"},
            "parameter": "volume", "points": [{"at": 0, "value": -6.0, "shape": "exponential"}, {"at": 1000, "value": 0.0}]}]))
        .unwrap();
        assert_eq!(
            check(&composition),
            [(
                Code::InvalidRange,
                "/mix/automation/0/points/0/shape".to_owned()
            )]
        );
        composition.mix.automation.clear();
        composition.mix.effects[0].effects[0].processor = serde_json::from_value(json!(
            {"kind": "eq", "bands": [{"filter": "peaking", "frequency_hz": 100.0, "gain_db": 0.0, "q": 0.71}]}))
        .unwrap();
        composition.mix.effect_automation[0].parameter = effects::EffectParameter::GainDb;
        composition.mix.effect_automation[0].points[0].value = Tenths(30);
        composition.mix.effect_automation[0].points[1].value = Tenths(60);
        let found = check(&composition);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, Code::InvalidRange);
        assert!(found[0].1.ends_with("/points/0/shape"), "{}", found[0].1);
    }
}
