//! Edit requests and their all-or-nothing application to a composition.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::audio_clips::{AudioClip, ClipUpdate};
use crate::composition::{
    Composition, ControlKind, ControlLane, ControlPoint, Marker, Note, Playback, Section,
    StackedNotes, TempoChange, TimeSignature, Timebase, Voice, check_composition_with,
};
use crate::diagnostic::{Diagnostic, DiagnosticCode as Code, Report};
use crate::effects::{Effect, EffectParameter, Processor};
use crate::mix::{Bus, MixParameter, MixPoint, MixSend, MixTarget, Tenths};
use crate::voice_instruments::VoiceInstrument;

/// One agent decision applied as a unit. Replaying the same `job_id` with the
/// same base and operations yields the stored result; partial results never exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditRequest {
    pub job_id: String,
    pub base_revision: String,
    #[serde(default)]
    pub scope: EditScope,
    /// Data that must stay exactly the same, checked by comparing the result
    /// with the base.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protections: Vec<Protection>,
    pub operations: Vec<EditOperation>,
    /// Delivered feedback this edit responds to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<String>,
    /// The agent's reason or hypothesis for the change, kept for review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

/// What the request may change. Omitted voices or range allow every voice or time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voices: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<TimeRange>,
    /// Allows tempo, time signature, section, marker and playback changes.
    #[serde(default)]
    pub timeline: bool,
    /// Allows changes to buses and the master: their settings, automation,
    /// creation and removal. Voice strips, sends and voice automation are
    /// voice data, checked by `voices`, `range` and protections.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub buses: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimeRange {
    pub start: u64,
    pub end: u64,
}

impl TimeRange {
    fn contains(&self, start: u64, end: u64) -> bool {
        self.start <= start && end <= self.end
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        start < self.end && self.start < end
    }
}

/// Everything of `voice` must stay unchanged: its notes, control lanes and
/// audio clips, and its mixer data (strip settings, sends, effect chain,
/// mixer and effect automation, and chip instrument). With `range`, notes,
/// control points, clips and automation points are protected where they
/// overlap it; data without a time (strip settings, sends, the effect chain
/// and the instrument) stays protected whatever the range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Protection {
    pub voice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<TimeRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditOperation {
    AddVoice {
        voice: Voice,
    },
    AddNotes {
        notes: Vec<Note>,
    },
    /// Replaces the given fields of existing notes.
    UpdateNotes {
        notes: Vec<NoteUpdate>,
    },
    RemoveNotes {
        ids: Vec<String>,
    },
    /// Moves notes in time and transposes them.
    ShiftNotes {
        ids: Vec<String>,
        #[serde(default)]
        time_offset: i64,
        #[serde(default)]
        pitch_offset: i8,
    },
    /// Replaces a whole lane; empty `points` removes it.
    SetControlPoints {
        voice: String,
        kind: ControlKind,
        points: Vec<ControlPoint>,
    },
    SetTempoMap {
        tempo_map: Vec<TempoChange>,
    },
    SetTimeSignatures {
        time_signatures: Vec<TimeSignature>,
    },
    /// Replaces every section. A section is a named region `[start, end)`
    /// that `render_batch` can render by id; sections may overlap.
    SetSections {
        sections: Vec<Section>,
    },
    /// Replaces every marker: named positions, in time order, written to
    /// MIDI as meta events of their kind (`marker` by default).
    SetMarkers {
        markers: Vec<Marker>,
    },
    SetPlayback {
        playback: Playback,
    },
    /// Copies the notes that lie entirely inside `source` so they start at
    /// `target_start`. Copies get the id `<id>@<target_start>`; controls are
    /// not copied.
    DuplicateRange {
        source: TimeRange,
        target_start: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        voices: Option<Vec<String>>,
    },
    /// Rounds note starts, and optionally ends, to the nearest multiple of
    /// `grid`; halfway values round up.
    QuantizeNotes {
        ids: Vec<String>,
        grid: u64,
        #[serde(default)]
        ends: bool,
    },
    /// Offsets starts and velocities by reproducible pseudo-random amounts of at
    /// most `timing` and `velocity`. The same seed and note id always give the
    /// same offset; starts stop at 0 and velocities stay 1–127.
    HumanizeNotes {
        ids: Vec<String>,
        seed: u64,
        #[serde(default)]
        timing: u64,
        #[serde(default)]
        velocity: u8,
    },
    /// Sets each velocity to `velocity × percent / 100 + offset`, rounded and
    /// kept within 1–127.
    ScaleVelocities {
        ids: Vec<String>,
        #[serde(default = "hundred")]
        percent: u16,
        #[serde(default)]
        offset: i16,
    },
    /// Interpolates velocities linearly from `from` to `to` across the notes in
    /// start order (ties by id).
    RampVelocities {
        ids: Vec<String>,
        from: u8,
        to: u8,
    },
    /// Extends each note to the next later start among the selected notes of
    /// its voice, minus `gap`. Notes of the last start keep their length.
    LegatoNotes {
        ids: Vec<String>,
        #[serde(default)]
        gap: u64,
    },
    /// Spreads notes that start together in one voice by `step` each, in
    /// ascending (`up`) or descending (`down`) pitch order; ends stay fixed.
    StrumNotes {
        ids: Vec<String>,
        step: u64,
        #[serde(default)]
        direction: StrumDirection,
    },
    /// Moves pitches onto a scale. `scale` names a built-in scale, or
    /// `intervals` lists semitones above `root` (pitch class 0–11). Halfway
    /// pitches go down for `nearest`.
    SnapToScale {
        ids: Vec<String>,
        root: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scale: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intervals: Option<Vec<u8>>,
        #[serde(default)]
        direction: SnapDirection,
    },
    /// Scales start offsets from `anchor` and durations by
    /// `numerator / denominator`; halfway values round to the later time.
    StretchNotes {
        ids: Vec<String>,
        anchor: u64,
        numerator: u64,
        denominator: u64,
    },
    /// Replaces a control lane's points in `start..=end` with a linear ramp
    /// from `from` to `to`: a point every `step`, and `to` exactly at `end`.
    /// Values round half up; later points of the lane are kept.
    RampControlPoints {
        voice: String,
        kind: ControlKind,
        start: u64,
        end: u64,
        from: i32,
        to: i32,
        step: u64,
    },
    /// Changes the given mixer fields of a voice, bus or the master. Voices
    /// accept every field, buses `volume_db`, `pan` and `mute`, the master
    /// `volume_db`. `output` is `master` or a bus id.
    SetMix {
        target: MixTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume_db: Option<Tenths>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pan: Option<Tenths>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mute: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        solo: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// Adds a bus that sums into the master.
    AddBus {
        bus: Bus,
    },
    /// Removes a bus and its automation. Outputs and sends to it must be
    /// changed in the same request.
    RemoveBus {
        id: String,
    },
    /// Adds or replaces the send from `send.voice` to `send.bus`.
    SetSend {
        send: MixSend,
    },
    RemoveSend {
        voice: String,
        bus: String,
    },
    /// Replaces the volume or pan automation of a voice, bus or the master;
    /// empty `points` removes it.
    SetMixAutomation {
        target: MixTarget,
        parameter: MixParameter,
        points: Vec<MixPoint>,
    },
    /// Replaces the automation of one effect parameter: the effect `effect`
    /// in the chain of `target`, the parameter, and `band` (0-based) for
    /// equalizer gain_db and frequency_hz. Point shapes are step, linear, or
    /// exponential (geometric ramps, for EQ frequency_hz and compressor ratio
    /// only). Empty `points` removes the lane; clearing a lane that does not
    /// exist is refused.
    SetEffectAutomation {
        target: MixTarget,
        effect: String,
        parameter: EffectParameter,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        band: Option<u8>,
        points: Vec<MixPoint>,
    },
    /// Replaces the insert effect chain of a voice, bus or the master;
    /// empty `effects` removes it. Effects process in list order.
    SetEffects {
        target: MixTarget,
        effects: Vec<Effect>,
    },
    /// Inserts an effect into a chain at `index` (default: the end),
    /// creating the chain when needed.
    InsertEffect {
        target: MixTarget,
        effect: Effect,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    /// Removes an effect; a chain left empty is removed.
    RemoveEffect {
        target: MixTarget,
        id: String,
    },
    /// Moves an effect to `index` in its chain's processing order.
    MoveEffect {
        target: MixTarget,
        id: String,
        index: usize,
    },
    /// Replaces an effect's settings with a processor of the same kind,
    /// or changes its bypass.
    SetEffect {
        target: MixTarget,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        processor: Option<Processor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bypass: Option<bool>,
    },
    /// Plays `voice` through a chip instrument instead of the reference
    /// synthesizer in renders, replacing any earlier assignment. Render-only
    /// state: MIDI export keeps the voice's notes and program numbers.
    SetInstrument {
        voice: String,
        instrument: VoiceInstrument,
    },
    /// Returns `voice` to the reference synthesizer.
    ClearInstrument {
        voice: String,
    },
    /// Places a range of a retained WAV on a voice's timeline; renders play it
    /// through the voice's strip. Render-only state: MIDI export does not carry it.
    AddClip {
        clip: AudioClip,
    },
    /// Replaces the given fields of an existing clip.
    UpdateClip {
        clip: ClipUpdate,
    },
    RemoveClip {
        id: String,
    },
}

/// Upper bound on the points one ramp writes.
pub const MAX_RAMP_POINTS: u64 = 4096;

fn hundred() -> u16 {
    100
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StrumDirection {
    #[default]
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapDirection {
    #[default]
    Nearest,
    Up,
    Down,
}

/// Semitones above the root for each built-in scale name.
pub const SCALES: [(&str, &[u8]); 11] = [
    ("major", &[0, 2, 4, 5, 7, 9, 11]),
    ("natural_minor", &[0, 2, 3, 5, 7, 8, 10]),
    ("harmonic_minor", &[0, 2, 3, 5, 7, 8, 11]),
    ("melodic_minor", &[0, 2, 3, 5, 7, 9, 11]),
    ("dorian", &[0, 2, 3, 5, 7, 9, 10]),
    ("phrygian", &[0, 1, 3, 5, 7, 8, 10]),
    ("lydian", &[0, 2, 4, 6, 7, 9, 11]),
    ("mixolydian", &[0, 2, 4, 5, 7, 9, 10]),
    ("locrian", &[0, 1, 3, 5, 6, 8, 10]),
    ("major_pentatonic", &[0, 2, 4, 7, 9]),
    ("minor_pentatonic", &[0, 3, 5, 7, 10]),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteUpdate {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity: Option<u8>,
}

/// Applies a request to `base` and returns the new composition only when the
/// operations succeed, the result passes every composition check and the
/// changes stay inside the scope and protections.
pub fn apply_edit(
    timebase: Timebase,
    base: &Composition,
    request: &EditRequest,
) -> Result<Composition, Vec<Diagnostic>> {
    apply_edit_with(timebase, base, request, StackedNotes::Reject)
}

/// `apply_edit` for a project whose stacked notes come from a MIDI source;
/// the source writer checks those stacks separately.
pub(crate) fn apply_edit_with(
    timebase: Timebase,
    base: &Composition,
    request: &EditRequest,
    stacked: StackedNotes,
) -> Result<Composition, Vec<Diagnostic>> {
    let result = apply_operations(base, request)?;
    let mut diagnostics = Vec::new();
    check_composition_with(
        timebase,
        &result,
        stacked,
        &mut Report::new("/result", &mut diagnostics),
    );
    check_edit_bounds(
        base,
        &result,
        request,
        &mut Report::new("", &mut diagnostics),
    );
    if diagnostics.is_empty() {
        Ok(result)
    } else {
        Err(diagnostics)
    }
}

/// Runs the operations in order without checking the result.
pub fn apply_operations(
    base: &Composition,
    request: &EditRequest,
) -> Result<Composition, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let mut report = Report::new("", &mut diagnostics);
    if request.operations.is_empty() {
        report.push(
            Code::EmptyEdit,
            "operations",
            "an edit request needs at least one operation",
        );
    }
    let mut result = base.clone();
    for (i, operation) in request.operations.iter().enumerate() {
        apply_operation(
            &mut result,
            operation,
            &mut report.nested(format!("operations/{i}")),
        );
    }
    if diagnostics.is_empty() {
        Ok(result)
    } else {
        Err(diagnostics)
    }
}

fn note_index(
    composition: &Composition,
    id: &str,
    report: &mut Report,
    at: String,
) -> Option<usize> {
    let found = composition.notes.iter().position(|n| n.id == id);
    if found.is_none() {
        report.push(
            Code::UnknownReference,
            at,
            format!("note `{id}` does not exist at this point of the request"),
        );
    }
    found
}

/// Indices of the selected notes, reporting unknown and repeated ids.
fn note_indices(composition: &Composition, ids: &[String], report: &mut Report) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut indices = Vec::new();
    for (j, id) in ids.iter().enumerate() {
        if !seen.insert(id) {
            report.push(
                Code::DuplicateId,
                format!("ids/{j}"),
                format!("note `{id}` is listed twice"),
            );
        } else if let Some(i) = note_index(composition, id, report, format!("ids/{j}")) {
            indices.push(i);
        }
    }
    indices
}

/// Allowed pitch classes for a scale name or explicit intervals.
fn scale_classes(
    root: u8,
    scale: Option<&str>,
    intervals: Option<&[u8]>,
    report: &mut Report,
) -> Option<Vec<u8>> {
    if root > 11 {
        report.push(Code::ValueOutOfRange, "root", "root is a pitch class 0–11");
        return None;
    }
    let intervals = match (scale, intervals) {
        (Some(name), None) => match SCALES.iter().find(|(n, _)| *n == name) {
            Some((_, intervals)) => *intervals,
            None => {
                report.push(
                    Code::UnknownReference,
                    "scale",
                    format!("unknown scale `{name}`"),
                );
                return None;
            }
        },
        (None, Some(intervals)) if !intervals.is_empty() && intervals.iter().all(|i| *i < 12) => {
            intervals
        }
        _ => {
            report.push(
                Code::InvalidRange,
                "scale",
                "give either a scale name or non-empty intervals 0–11",
            );
            return None;
        }
    };
    Some(intervals.iter().map(|i| (root + i) % 12).collect())
}

/// SplitMix64 keyed by the seed and the note id, so offsets do not depend on
/// selection order.
struct Random(u64);

impl Random {
    fn new(seed: u64, id: &str) -> Self {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in id.bytes() {
            hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
        }
        Self(seed ^ hash)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Uniform in `-max..=max`.
    fn offset(&mut self, max: u64) -> i64 {
        if max == 0 {
            return 0;
        }
        let max = max.min(i64::MAX as u64 / 2);
        (self.next() % (2 * max + 1)) as i64 - max as i64
    }
}

fn apply_operation(composition: &mut Composition, operation: &EditOperation, report: &mut Report) {
    match operation {
        EditOperation::AddVoice { voice } => composition.voices.push(voice.clone()),
        EditOperation::AddNotes { notes } => composition.notes.extend(notes.iter().cloned()),
        EditOperation::UpdateNotes { notes } => {
            for (j, update) in notes.iter().enumerate() {
                let Some(i) = note_index(composition, &update.id, report, format!("notes/{j}/id"))
                else {
                    continue;
                };
                let note = &mut composition.notes[i];
                if let Some(voice) = &update.voice {
                    note.voice = voice.clone();
                }
                note.start = update.start.unwrap_or(note.start);
                note.duration = update.duration.unwrap_or(note.duration);
                note.pitch = update.pitch.unwrap_or(note.pitch);
                note.velocity = update.velocity.unwrap_or(note.velocity);
            }
        }
        EditOperation::RemoveNotes { ids } => {
            for (j, id) in ids.iter().enumerate() {
                if let Some(i) = note_index(composition, id, report, format!("ids/{j}")) {
                    composition.notes.remove(i);
                }
            }
        }
        EditOperation::ShiftNotes {
            ids,
            time_offset,
            pitch_offset,
        } => {
            for (j, id) in ids.iter().enumerate() {
                let Some(i) = note_index(composition, id, report, format!("ids/{j}")) else {
                    continue;
                };
                let note = &mut composition.notes[i];
                let start = note.start.checked_add_signed(*time_offset);
                let pitch = note
                    .pitch
                    .checked_add_signed(*pitch_offset)
                    .filter(|p| *p <= 127);
                match (start, pitch) {
                    (Some(start), Some(pitch)) => {
                        note.start = start;
                        note.pitch = pitch;
                    }
                    _ => report.push(
                        Code::ValueOutOfRange,
                        format!("ids/{j}"),
                        format!("shifting `{id}` leaves the valid time or pitch range"),
                    ),
                }
            }
        }
        EditOperation::SetControlPoints {
            voice,
            kind,
            points,
        } => {
            composition
                .control_lanes
                .retain(|lane| !(lane.voice == *voice && lane.kind == *kind));
            if !points.is_empty() {
                composition.control_lanes.push(ControlLane {
                    voice: voice.clone(),
                    kind: *kind,
                    points: points.clone(),
                });
            }
        }
        EditOperation::SetTempoMap { tempo_map } => composition.tempo_map = tempo_map.clone(),
        EditOperation::SetTimeSignatures { time_signatures } => {
            composition.time_signatures = time_signatures.clone()
        }
        EditOperation::SetSections { sections } => composition.sections = sections.clone(),
        EditOperation::SetMarkers { markers } => composition.markers = markers.clone(),
        EditOperation::SetPlayback { playback } => composition.playback = *playback,
        EditOperation::DuplicateRange {
            source,
            target_start,
            voices,
        } => {
            if source.start >= source.end {
                report.push(Code::InvalidRange, "source", "the source range is empty");
                return;
            }
            let selected =
                |voice: &str| voices.as_ref().is_none_or(|v| v.iter().any(|x| x == voice));
            let mut copies = Vec::new();
            for note in composition.notes.iter().filter(|n| selected(&n.voice)) {
                let note_end = note.end().unwrap_or(u64::MAX);
                let inside = source.start <= note.start && note_end <= source.end;
                if !inside && note.start < source.end && note_end > source.start {
                    report.push(
                        Code::InvalidRange,
                        "source",
                        format!("note `{}` crosses the source range boundary", note.id),
                    );
                } else if inside {
                    copies.push(Note {
                        id: format!("{}@{target_start}", note.id),
                        start: target_start + (note.start - source.start),
                        ..note.clone()
                    });
                }
            }
            composition.notes.extend(copies);
        }
        EditOperation::HumanizeNotes {
            ids,
            seed,
            timing,
            velocity,
        } => {
            for i in note_indices(composition, ids, report) {
                let note = &mut composition.notes[i];
                let mut random = Random::new(*seed, &note.id);
                let time = random.offset(*timing);
                let level = random.offset(*velocity as u64);
                note.start = note.start.saturating_add_signed(time);
                note.velocity = (note.velocity as i64 + level).clamp(1, 127) as u8;
            }
        }
        EditOperation::ScaleVelocities {
            ids,
            percent,
            offset,
        } => {
            for i in note_indices(composition, ids, report) {
                let note = &mut composition.notes[i];
                let scaled = (note.velocity as i64 * *percent as i64 * 2 + 100) / 200;
                note.velocity = (scaled + *offset as i64).clamp(1, 127) as u8;
            }
        }
        EditOperation::RampVelocities { ids, from, to } => {
            if !(1..=127).contains(from) || !(1..=127).contains(to) {
                report.push(Code::ValueOutOfRange, "from", "velocities must be 1–127");
                return;
            }
            let mut order = note_indices(composition, ids, report);
            order.sort_by(|&a, &b| {
                let (a, b) = (&composition.notes[a], &composition.notes[b]);
                (a.start, &a.id).cmp(&(b.start, &b.id))
            });
            let last = order.len().saturating_sub(1).max(1) as i64;
            for (k, &i) in order.iter().enumerate() {
                let span = (*to as i64 - *from as i64) * k as i64;
                composition.notes[i].velocity =
                    (*from as i64 + (2 * span + last).div_euclid(2 * last)) as u8;
            }
        }
        EditOperation::LegatoNotes { ids, gap } => {
            let selected = note_indices(composition, ids, report);
            for &i in &selected {
                let note = &composition.notes[i];
                let next = selected
                    .iter()
                    .map(|&j| &composition.notes[j])
                    .filter(|other| other.voice == note.voice && other.start > note.start)
                    .map(|other| other.start)
                    .min();
                let Some(next) = next else { continue };
                match (next - note.start).checked_sub(*gap).filter(|d| *d > 0) {
                    Some(duration) => composition.notes[i].duration = duration,
                    None => report.push(
                        Code::NonPositiveDuration,
                        "gap",
                        format!("the gap leaves no length for note `{}`", note.id),
                    ),
                }
            }
        }
        EditOperation::StrumNotes {
            ids,
            step,
            direction,
        } => {
            let mut chords: BTreeMap<(String, u64), Vec<usize>> = BTreeMap::new();
            for i in note_indices(composition, ids, report) {
                let note = &composition.notes[i];
                chords
                    .entry((note.voice.clone(), note.start))
                    .or_default()
                    .push(i);
            }
            for mut chord in chords.into_values() {
                chord.sort_by_key(|&i| {
                    (composition.notes[i].pitch, composition.notes[i].id.clone())
                });
                if *direction == StrumDirection::Down {
                    chord.reverse();
                }
                for (k, &i) in chord.iter().enumerate() {
                    let delay = step.saturating_mul(k as u64);
                    let note = &mut composition.notes[i];
                    if delay >= note.duration {
                        report.push(
                            Code::NonPositiveDuration,
                            "step",
                            format!("strumming leaves no length for note `{}`", note.id),
                        );
                        continue;
                    }
                    note.start += delay;
                    note.duration -= delay;
                }
            }
        }
        EditOperation::SnapToScale {
            ids,
            root,
            scale,
            intervals,
            direction,
        } => {
            let Some(classes) =
                scale_classes(*root, scale.as_deref(), intervals.as_deref(), report)
            else {
                return;
            };
            for i in note_indices(composition, ids, report) {
                let note = &mut composition.notes[i];
                let fits =
                    |p: i32| (0..=127).contains(&p) && classes.contains(&(p.rem_euclid(12) as u8));
                let pitch = note.pitch as i32;
                let found = (0..12).find_map(|d| {
                    let candidates: &[i32] = match direction {
                        SnapDirection::Nearest => &[pitch - d, pitch + d],
                        SnapDirection::Up => &[pitch + d],
                        SnapDirection::Down => &[pitch - d],
                    };
                    candidates.iter().copied().find(|&p| fits(p))
                });
                match found {
                    Some(p) => note.pitch = p as u8,
                    None => report.push(
                        Code::ValueOutOfRange,
                        "direction",
                        format!("no scale pitch for note `{}` within 0–127", note.id),
                    ),
                }
            }
        }
        EditOperation::RampControlPoints {
            voice,
            kind,
            start,
            end,
            from,
            to,
            step,
        } => {
            if start >= end || *step == 0 || (end - start) / step > MAX_RAMP_POINTS {
                report.push(
                    Code::InvalidRange,
                    "step",
                    format!("a ramp needs start < end and a positive step giving at most {MAX_RAMP_POINTS} points"),
                );
                return;
            }
            let span = (end - start) as i128;
            let value = |at: u64| {
                let moved = (*to as i128 - *from as i128) * (at - start) as i128;
                (*from as i128 + (2 * moved + span).div_euclid(2 * span)) as i32
            };
            let mut ramp: Vec<ControlPoint> = (*start..*end)
                .step_by(*step as usize)
                .map(|at| ControlPoint {
                    at,
                    value: value(at),
                })
                .collect();
            ramp.push(ControlPoint {
                at: *end,
                value: *to,
            });
            let lane = match composition
                .control_lanes
                .iter_mut()
                .find(|l| l.voice == *voice && l.kind == *kind)
            {
                Some(lane) => lane,
                None => {
                    composition.control_lanes.push(ControlLane {
                        voice: voice.clone(),
                        kind: *kind,
                        points: Vec::new(),
                    });
                    composition.control_lanes.last_mut().expect("just pushed")
                }
            };
            lane.points.retain(|p| p.at < *start || p.at > *end);
            lane.points.extend(ramp);
            lane.points.sort_by_key(|p| p.at);
        }
        EditOperation::StretchNotes {
            ids,
            anchor,
            numerator,
            denominator,
        } => {
            if *numerator == 0 || *denominator == 0 {
                report.push(
                    Code::ValueOutOfRange,
                    "numerator",
                    "the ratio must be positive",
                );
                return;
            }
            let (n, d, anchor) = (*numerator as i128, *denominator as i128, *anchor as i128);
            let scale = |t: u64| anchor + (2 * (t as i128 - anchor) * n + d).div_euclid(2 * d);
            for i in note_indices(composition, ids, report) {
                let note = &mut composition.notes[i];
                let start = scale(note.start);
                let end = scale(note.end().unwrap_or(u64::MAX));
                match (u64::try_from(start), u64::try_from(end - start)) {
                    (Ok(start), Ok(duration)) if duration > 0 => {
                        note.start = start;
                        note.duration = duration;
                    }
                    _ => report.push(
                        Code::ValueOutOfRange,
                        "numerator",
                        format!("stretching `{}` leaves the valid time range", note.id),
                    ),
                }
            }
        }
        EditOperation::QuantizeNotes { ids, grid, ends } => {
            if *grid == 0 {
                report.push(Code::ValueOutOfRange, "grid", "the grid must be positive");
                return;
            }
            let round = |value: u64| (value + grid / 2) / grid * grid;
            for (j, id) in ids.iter().enumerate() {
                let Some(i) = note_index(composition, id, report, format!("ids/{j}")) else {
                    continue;
                };
                let note = &mut composition.notes[i];
                let end = note.end().unwrap_or(u64::MAX);
                note.start = round(note.start);
                if *ends {
                    note.duration = round(end).saturating_sub(note.start);
                }
            }
        }
        EditOperation::SetMix { .. }
        | EditOperation::AddBus { .. }
        | EditOperation::RemoveBus { .. }
        | EditOperation::SetSend { .. }
        | EditOperation::RemoveSend { .. }
        | EditOperation::SetMixAutomation { .. }
        | EditOperation::SetEffectAutomation { .. } => {
            crate::mix::apply_mix_operation(composition, operation, report)
        }
        EditOperation::SetEffects { .. }
        | EditOperation::InsertEffect { .. }
        | EditOperation::RemoveEffect { .. }
        | EditOperation::MoveEffect { .. }
        | EditOperation::SetEffect { .. } => {
            crate::effects::apply_effect_operation(composition, operation, report)
        }
        EditOperation::SetInstrument { .. } | EditOperation::ClearInstrument { .. } => {
            crate::voice_instruments::apply_instrument_operation(composition, operation, report)
        }
        EditOperation::AddClip { .. }
        | EditOperation::UpdateClip { .. }
        | EditOperation::RemoveClip { .. } => {
            crate::audio_clips::apply_clip_operation(composition, operation, report)
        }
    }
}

/// Compares `result` with `base` and reports changes outside the request's
/// scope or inside its protections.
pub(crate) fn check_edit_bounds(
    base: &Composition,
    result: &Composition,
    request: &EditRequest,
    report: &mut Report,
) {
    let known_voices: BTreeSet<&str> = base
        .voices
        .iter()
        .chain(&result.voices)
        .map(|v| v.id.as_str())
        .collect();
    if let Some(voices) = &request.scope.voices {
        for (i, voice) in voices.iter().enumerate() {
            if !known_voices.contains(voice.as_str()) {
                report.push(
                    Code::UnknownReference,
                    format!("scope/voices/{i}"),
                    format!("voice `{voice}` does not exist"),
                );
            }
        }
    }
    for (i, protection) in request.protections.iter().enumerate() {
        if !known_voices.contains(protection.voice.as_str()) {
            report.push(
                Code::UnknownReference,
                format!("protections/{i}/voice"),
                format!("voice `{}` does not exist", protection.voice),
            );
        }
    }

    let timeline_changed = base.tempo_map != result.tempo_map
        || base.time_signatures != result.time_signatures
        || base.sections != result.sections
        || base.playback != result.playback;
    if timeline_changed && !request.scope.timeline {
        report.push(
            Code::OutOfScope,
            "scope/timeline",
            "the request changes tempo, meter, sections or playback without timeline scope",
        );
    }
    if base.markers != result.markers && !request.scope.timeline {
        report.push(
            Code::OutOfScope,
            "scope/timeline",
            "the request changes markers without timeline scope",
        );
    }

    for voice in result.voices.iter().filter(|v| !base.voices.contains(v)) {
        check_change(
            request,
            &[(&voice.id, None)],
            report,
            format!("voice `{}`", voice.id),
        );
    }
    for voice in base.voices.iter().filter(|v| !result.voices.contains(v)) {
        check_change(
            request,
            &[(&voice.id, None)],
            report,
            format!("voice `{}`", voice.id),
        );
    }

    let base_notes: BTreeMap<&str, &Note> = base.notes.iter().map(|n| (n.id.as_str(), n)).collect();
    let result_notes: BTreeMap<&str, &Note> =
        result.notes.iter().map(|n| (n.id.as_str(), n)).collect();
    for id in base_notes
        .keys()
        .chain(result_notes.keys())
        .collect::<BTreeSet<_>>()
    {
        let before = base_notes.get(id);
        let after = result_notes.get(id);
        if before == after {
            continue;
        }
        let versions: Vec<(&str, Option<(u64, u64)>)> = before
            .into_iter()
            .chain(after)
            .map(|note| {
                (
                    note.voice.as_str(),
                    Some((note.start, note.end().unwrap_or(u64::MAX))),
                )
            })
            .collect();
        check_change(request, &versions, report, format!("note `{id}`"));
    }

    let lanes = |c: &Composition| -> BTreeMap<(String, ControlKind), Vec<ControlPoint>> {
        c.control_lanes
            .iter()
            .map(|l| ((l.voice.clone(), l.kind), l.points.clone()))
            .collect()
    };
    let (base_lanes, result_lanes) = (lanes(base), lanes(result));
    for key in base_lanes
        .keys()
        .chain(result_lanes.keys())
        .collect::<BTreeSet<_>>()
    {
        let before = base_lanes.get(key).map(Vec::as_slice).unwrap_or_default();
        let after = result_lanes.get(key).map(Vec::as_slice).unwrap_or_default();
        let versions: Vec<(&str, Option<(u64, u64)>)> = before
            .iter()
            .filter(|p| !after.contains(p))
            .chain(after.iter().filter(|p| !before.contains(p)))
            .map(|point| (key.0.as_str(), Some((point.at, point.at.saturating_add(1)))))
            .collect();
        if !versions.is_empty() {
            check_change(
                request,
                &versions,
                report,
                format!("{:?} control of `{}`", key.1, key.0),
            );
        }
    }
    crate::mix::check_mix_bounds(base, result, request, report);
}

/// Reports a change once, considering every version of the changed item: a
/// moved note must be allowed and unprotected both where it was and where it went.
pub(crate) fn check_change(
    request: &EditRequest,
    versions: &[(&str, Option<(u64, u64)>)],
    report: &mut Report,
    what: String,
) {
    let scope = &request.scope;
    let allowed = |(voice, span): &(&str, Option<(u64, u64)>)| {
        let voice_allowed = scope
            .voices
            .as_ref()
            .is_none_or(|voices| voices.iter().any(|v| v == voice));
        let range_allowed = match (scope.range, span) {
            (None, _) => true,
            (Some(range), Some((start, end))) => range.contains(*start, *end),
            (Some(_), None) => false,
        };
        voice_allowed && range_allowed
    };
    if !versions.iter().all(allowed) {
        report.push(
            Code::OutOfScope,
            "scope",
            format!("{what} changes outside the request scope"),
        );
    }
    for (i, protection) in request.protections.iter().enumerate() {
        let protected = versions.iter().any(|(voice, span)| {
            protection.voice == *voice
                && match (protection.range, span) {
                    (None, _) | (_, None) => true,
                    (Some(range), Some((start, end))) => range.overlaps(*start, *end),
                }
        });
        if protected {
            report.push(
                Code::ProtectedChange,
                format!("protections/{i}"),
                format!("{what} is protected"),
            );
        }
    }
}
