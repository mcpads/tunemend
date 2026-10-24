//! Musical content of one revision and the checks every revision must pass.
//!
//! Positions and lengths are integers in the project's timebase unit: ticks
//! for music, microseconds for effects. Ranges are half-open `[start, end)`.

use std::collections::{BTreeMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::diagnostic::{DiagnosticCode as Code, Report};
use crate::mix::Mix;

/// Fixed when a project is created; edits never switch it implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Timebase {
    /// Integer ticks with `ppq` ticks per quarter note and a tempo map.
    Music { ppq: u16 },
    /// Integer microseconds without a beat grid.
    Effect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Composition {
    /// Music only. Must start at tick 0.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tempo_map: Vec<TempoChange>,
    /// Music only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub time_signatures: Vec<TimeSignature>,
    /// Music only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_signatures: Vec<KeySignature>,
    /// Text events kept with their positions, such as markers from a MIDI file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<Marker>,
    pub voices: Vec<Voice>,
    pub notes: Vec<Note>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control_lanes: Vec<ControlLane>,
    /// Render-time mixer: voice strips, buses, sends, master and automation.
    #[serde(default, skip_serializing_if = "Mix::is_empty")]
    pub mix: Mix,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
    pub playback: Playback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TempoChange {
    pub tick: u64,
    pub microseconds_per_quarter: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimeSignature {
    pub tick: u64,
    pub numerator: u8,
    /// Note value of one beat, a power of two such as 4 or 8.
    pub denominator: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeySignature {
    pub tick: u64,
    /// Negative for flats, -7–7.
    pub sharps: i8,
    pub minor: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub at: u64,
    /// Default `marker`.
    #[serde(default)]
    pub kind: MarkerKind,
    /// The marker's name or text.
    pub text: String,
}

/// The MIDI meta event a marker is written as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    #[default]
    Marker,
    Text,
    CuePoint,
    Lyric,
    Copyright,
}

/// A musical part. It is independent of MIDI tracks, MIDI channels and the
/// physical channels of a game sound chip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Voice {
    pub id: String,
    /// Free description of the musical role, such as `melody` or `bass`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub id: String,
    pub voice: String,
    pub start: u64,
    pub duration: u64,
    /// MIDI note number, 0–127.
    pub pitch: u8,
    /// Note-on velocity, 1–127.
    pub velocity: u8,
    /// Note-off velocity, 0–127, when the source or the request specifies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_velocity: Option<u8>,
}

impl Note {
    pub fn end(&self) -> Option<u64> {
        self.start.checked_add(self.duration)
    }
}

/// Explicit step values for one controller of one voice. Values hold until
/// the next point; curves are not interpolated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlLane {
    pub voice: String,
    pub kind: ControlKind,
    pub points: Vec<ControlPoint>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    /// Program change, 0–127. Not a chip patch.
    Program,
    /// CC7, 0–127.
    Volume,
    /// CC10, 0–127.
    Pan,
    /// CC11, 0–127.
    Expression,
    /// CC1 modulation depth, 0–127.
    Modulation,
    /// CC91 reverb send, 0–127.
    Reverb,
    /// CC93 chorus send, 0–127.
    Chorus,
    /// Signed bend, -8192–8191.
    PitchBend,
    /// RPN 0 range in semitones, 0–127.
    PitchBendRange,
}

impl ControlKind {
    pub fn value_range(self) -> (i32, i32) {
        match self {
            Self::PitchBend => (-8192, 8191),
            _ => (0, 127),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlPoint {
    pub at: u64,
    pub value: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub id: String,
    pub start: u64,
    pub end: u64,
}

/// The content ends at `end`. With `loop_start`, playback repeats
/// `[loop_start, end)` after the intro `[0, loop_start)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Playback {
    pub end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_start: Option<u64>,
}

/// Largest tempo a Standard MIDI File can store.
pub const MAX_MICROSECONDS_PER_QUARTER: u32 = 0xFF_FFFF;

/// Value of a step lane in effect at `at`, or `None` before its first point.
pub(crate) fn value_at(points: &[ControlPoint], at: u64) -> Option<i32> {
    points
        .iter()
        .take_while(|p| p.at <= at)
        .last()
        .map(|p| p.value)
}

fn tempo_at(tempo_map: &[TempoChange], tick: u64) -> Option<u32> {
    tempo_map
        .iter()
        .take_while(|t| t.tick <= tick)
        .last()
        .map(|t| t.microseconds_per_quarter)
}

pub(crate) fn check_composition(
    timebase: Timebase,
    composition: &Composition,
    report: &mut Report,
) {
    let end = composition.playback.end;
    check_playback(&composition.playback, report);
    check_timing(timebase, composition, report);

    let mut voice_ids = HashSet::new();
    for (i, voice) in composition.voices.iter().enumerate() {
        if voice.id.is_empty() {
            report.push(
                Code::InvalidRange,
                format!("voices/{i}/id"),
                "voice id must not be empty",
            );
        } else if !voice_ids.insert(voice.id.as_str()) {
            report.push(
                Code::DuplicateId,
                format!("voices/{i}/id"),
                format!("voice `{}` is defined twice", voice.id),
            );
        }
    }

    check_notes(composition, &voice_ids, report);

    for (i, marker) in composition.markers.iter().enumerate() {
        if marker.at > end {
            report.push(
                Code::EventBeyondEnd,
                format!("markers/{i}/at"),
                "markers must not be after the playback end",
            );
        }
        if i > 0 && marker.at < composition.markers[i - 1].at {
            report.push(
                Code::ControlPointsUnordered,
                format!("markers/{i}/at"),
                "markers are listed in time order",
            );
        }
    }

    let mut section_ids = HashSet::new();
    for (i, section) in composition.sections.iter().enumerate() {
        if !section_ids.insert(section.id.as_str()) {
            report.push(
                Code::DuplicateId,
                format!("sections/{i}/id"),
                format!("section `{}` is defined twice", section.id),
            );
        }
        if section.start >= section.end {
            report.push(
                Code::InvalidRange,
                format!("sections/{i}"),
                "section start must be before its end",
            );
        } else if section.end > end {
            report.push(
                Code::EventBeyondEnd,
                format!("sections/{i}/end"),
                "section ends after the playback end",
            );
        }
    }

    check_control_lanes(composition, &voice_ids, report);
    crate::mix::check_mix(composition, &voice_ids, report);
}

fn check_playback(playback: &Playback, report: &mut Report) {
    if playback.end == 0 {
        report.push(
            Code::InvalidRange,
            "playback/end",
            "playback end must be after time 0",
        );
    }
    if let Some(loop_start) = playback.loop_start
        && loop_start >= playback.end
    {
        report.push(
            Code::InvalidRange,
            "playback/loop_start",
            "loop start must be before the playback end",
        );
    }
}

fn check_timing(timebase: Timebase, composition: &Composition, report: &mut Report) {
    let end = composition.playback.end;
    match timebase {
        Timebase::Effect => {
            if !composition.tempo_map.is_empty() {
                report.push(
                    Code::MusicalTimingInEffect,
                    "tempo_map",
                    "effects use microseconds and have no tempo map",
                );
            }
            if !composition.time_signatures.is_empty() {
                report.push(
                    Code::MusicalTimingInEffect,
                    "time_signatures",
                    "effects have no time signatures",
                );
            }
            if !composition.key_signatures.is_empty() {
                report.push(
                    Code::MusicalTimingInEffect,
                    "key_signatures",
                    "effects have no key signatures",
                );
            }
        }
        Timebase::Music { .. } => {
            match composition.tempo_map.first() {
                None => report.push(
                    Code::InvalidTempoMap,
                    "tempo_map",
                    "music needs a tempo at tick 0",
                ),
                Some(first) if first.tick != 0 => report.push(
                    Code::InvalidTempoMap,
                    "tempo_map/0/tick",
                    "the tempo map must start at tick 0",
                ),
                Some(_) => {}
            }
            for (i, change) in composition.tempo_map.iter().enumerate() {
                if change.microseconds_per_quarter == 0
                    || change.microseconds_per_quarter > MAX_MICROSECONDS_PER_QUARTER
                {
                    report.push(
                        Code::ValueOutOfRange,
                        format!("tempo_map/{i}/microseconds_per_quarter"),
                        format!("tempo must be 1–{MAX_MICROSECONDS_PER_QUARTER} microseconds per quarter note"),
                    );
                }
                if i > 0 && change.tick <= composition.tempo_map[i - 1].tick {
                    report.push(
                        Code::InvalidTempoMap,
                        format!("tempo_map/{i}/tick"),
                        "tempo changes must have increasing ticks",
                    );
                }
                if change.tick >= end && end > 0 {
                    report.push(
                        Code::EventBeyondEnd,
                        format!("tempo_map/{i}/tick"),
                        "tempo changes must be before the playback end",
                    );
                }
            }
            for (i, signature) in composition.time_signatures.iter().enumerate() {
                if signature.numerator == 0 || !signature.denominator.is_power_of_two() {
                    report.push(
                        Code::InvalidTimeSignature,
                        format!("time_signatures/{i}"),
                        "numerator must be positive and denominator a power of two",
                    );
                }
                if i > 0 && signature.tick <= composition.time_signatures[i - 1].tick {
                    report.push(
                        Code::InvalidTimeSignature,
                        format!("time_signatures/{i}/tick"),
                        "time signatures must have increasing ticks",
                    );
                }
                if signature.tick >= end && end > 0 {
                    report.push(
                        Code::EventBeyondEnd,
                        format!("time_signatures/{i}/tick"),
                        "time signatures must be before the playback end",
                    );
                }
            }
            for (i, key) in composition.key_signatures.iter().enumerate() {
                if !(-7..=7).contains(&key.sharps) {
                    report.push(
                        Code::ValueOutOfRange,
                        format!("key_signatures/{i}/sharps"),
                        "a key signature has -7–7 sharps",
                    );
                }
                if i > 0 && key.tick <= composition.key_signatures[i - 1].tick {
                    report.push(
                        Code::InvalidTimeSignature,
                        format!("key_signatures/{i}/tick"),
                        "key signatures must have increasing ticks",
                    );
                }
                if key.tick >= end && end > 0 {
                    report.push(
                        Code::EventBeyondEnd,
                        format!("key_signatures/{i}/tick"),
                        "key signatures must be before the playback end",
                    );
                }
            }
            if let Some(loop_start) = composition.playback.loop_start
                && let (Some(at_loop), Some(at_end)) = (
                    tempo_at(&composition.tempo_map, loop_start),
                    tempo_at(&composition.tempo_map, end),
                )
                && at_loop != at_end
                && !composition.tempo_map.iter().any(|t| t.tick == loop_start)
            {
                report.push(
                    Code::LoopStateMismatch,
                    "tempo_map",
                    format!("tempo is {at_end} at the end but {at_loop} at the loop start; add a tempo change at tick {loop_start}"),
                );
            }
        }
    }
}

fn check_notes(composition: &Composition, voice_ids: &HashSet<&str>, report: &mut Report) {
    let end = composition.playback.end;
    let mut note_ids = HashSet::new();
    let mut by_voice_pitch: BTreeMap<(&str, u8), Vec<usize>> = BTreeMap::new();

    for (i, note) in composition.notes.iter().enumerate() {
        if note.id.is_empty() {
            report.push(
                Code::InvalidRange,
                format!("notes/{i}/id"),
                "note id must not be empty",
            );
        } else if !note_ids.insert(note.id.as_str()) {
            report.push(
                Code::DuplicateId,
                format!("notes/{i}/id"),
                format!("note `{}` is defined twice", note.id),
            );
        }
        if !voice_ids.contains(note.voice.as_str()) {
            report.push(
                Code::UnknownReference,
                format!("notes/{i}/voice"),
                format!("voice `{}` does not exist", note.voice),
            );
        }
        if note.pitch > 127 {
            report.push(
                Code::ValueOutOfRange,
                format!("notes/{i}/pitch"),
                "pitch must be 0–127",
            );
        }
        if !(1..=127).contains(&note.velocity) {
            report.push(
                Code::ValueOutOfRange,
                format!("notes/{i}/velocity"),
                "velocity must be 1–127; use note removal instead of 0",
            );
        }
        if note.release_velocity.is_some_and(|v| v > 127) {
            report.push(
                Code::ValueOutOfRange,
                format!("notes/{i}/release_velocity"),
                "release velocity must be 0–127",
            );
        }
        if note.duration == 0 {
            report.push(
                Code::NonPositiveDuration,
                format!("notes/{i}/duration"),
                "note duration must be positive",
            );
            continue;
        }
        let Some(note_end) = note.end() else {
            report.push(
                Code::ValueOutOfRange,
                format!("notes/{i}"),
                "note end overflows",
            );
            continue;
        };
        if note_end > end {
            report.push(
                Code::EventBeyondEnd,
                format!("notes/{i}"),
                format!("note ends at {note_end}, after the playback end {end}"),
            );
        }
        if let Some(loop_start) = composition.playback.loop_start
            && note.start < loop_start
            && loop_start < note_end
        {
            report.push(
                Code::NoteCrossesLoopStart,
                format!("notes/{i}"),
                format!(
                    "note `{}` sounds across the loop start {loop_start}",
                    note.id
                ),
            );
        }
        by_voice_pitch
            .entry((note.voice.as_str(), note.pitch))
            .or_default()
            .push(i);
    }

    for indices in by_voice_pitch.values_mut() {
        report_overlaps(&composition.notes, indices, report, "the same voice");
    }
}

/// Reports notes of one pitch that start before the previous one ends.
pub(crate) fn report_overlaps(
    notes: &[Note],
    indices: &mut [usize],
    report: &mut Report,
    scope: &str,
) {
    indices.sort_by_key(|&i| (notes[i].start, i));
    let mut sounding: Option<(usize, u64)> = None;
    for &i in indices.iter() {
        let note = &notes[i];
        if let Some((held, held_end)) = sounding
            && note.start < held_end
        {
            report.push(
                Code::OverlappingNotes,
                format!("notes/{i}"),
                format!(
                    "note `{}` starts while `{}` of the same pitch still sounds in {scope}",
                    note.id, notes[held].id
                ),
            );
        }
        let note_end = note.end().unwrap_or(u64::MAX);
        if sounding.is_none_or(|(_, held_end)| note_end > held_end) {
            sounding = Some((i, note_end));
        }
    }
}

fn check_control_lanes(composition: &Composition, voice_ids: &HashSet<&str>, report: &mut Report) {
    let end = composition.playback.end;
    let mut lane_keys = HashSet::new();
    for (i, lane) in composition.control_lanes.iter().enumerate() {
        if !voice_ids.contains(lane.voice.as_str()) {
            report.push(
                Code::UnknownReference,
                format!("control_lanes/{i}/voice"),
                format!("voice `{}` does not exist", lane.voice),
            );
        }
        if !lane_keys.insert((lane.voice.as_str(), lane.kind)) {
            report.push(
                Code::DuplicateControlLane,
                format!("control_lanes/{i}"),
                "each voice has at most one lane per controller",
            );
        }
        if lane.points.is_empty() {
            report.push(
                Code::EmptyControlLane,
                format!("control_lanes/{i}/points"),
                "remove the lane instead of leaving it empty",
            );
            continue;
        }
        let (low, high) = lane.kind.value_range();
        for (j, point) in lane.points.iter().enumerate() {
            if !(low..=high).contains(&point.value) {
                report.push(
                    Code::ValueOutOfRange,
                    format!("control_lanes/{i}/points/{j}/value"),
                    format!("value must be {low}–{high}"),
                );
            }
            if j > 0 && point.at <= lane.points[j - 1].at {
                report.push(
                    Code::ControlPointsUnordered,
                    format!("control_lanes/{i}/points/{j}/at"),
                    "control points must have increasing times",
                );
            }
            if point.at >= end && end > 0 {
                report.push(
                    Code::EventBeyondEnd,
                    format!("control_lanes/{i}/points/{j}/at"),
                    "control points must be before the playback end",
                );
            }
        }
        if let Some(loop_start) = composition.playback.loop_start
            && !lane.points.iter().any(|p| p.at == loop_start)
        {
            let at_loop = value_at(&lane.points, loop_start);
            let at_end = value_at(&lane.points, end);
            if at_loop != at_end {
                report.push(
                    Code::LoopStateMismatch,
                    format!("control_lanes/{i}"),
                    format!("value is {at_end:?} at the end but {at_loop:?} at the loop start; add a point at {loop_start}"),
                );
            }
        }
    }

    for (i, lane) in composition.control_lanes.iter().enumerate() {
        if lane.kind != ControlKind::PitchBend {
            continue;
        }
        let first_bend = lane.points.first().map(|p| p.at);
        let range_set = composition
            .control_lanes
            .iter()
            .filter(|l| l.voice == lane.voice && l.kind == ControlKind::PitchBendRange)
            .any(
                |l| matches!((l.points.first(), first_bend), (Some(p), Some(bend)) if p.at <= bend),
            );
        if !range_set {
            report.push(
                Code::PitchBendRangeUnspecified,
                format!("control_lanes/{i}"),
                format!(
                    "voice `{}` bends without a pitch bend range set at or before its first bend",
                    lane.voice
                ),
            );
        }
    }
}
