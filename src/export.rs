//! How a revision maps onto a Standard MIDI File.

use std::collections::{BTreeMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::composition::{Composition, MAX_MICROSECONDS_PER_QUARTER, Timebase, report_overlaps};
use crate::diagnostic::{DiagnosticCode as Code, Report};
use crate::timeline::Rational;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MidiExport {
    pub id: String,
    pub revision: String,
    /// Required for effects, which have no tempo map; absent for music.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<MidiTiming>,
    pub assignments: Vec<VoiceAssignment>,
    /// Each section's id was also written as a marker meta event at its start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sections_as_markers: bool,
}

/// File timing for effects. PPQ 1000 with 1,000,000 µs per quarter gives a 1 ms grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MidiTiming {
    /// Ticks per quarter note, 1..32767: an SMF header division with bit 15
    /// clear has 15 bits.
    #[schemars(range(min = 1, max = MAX_PPQ))]
    pub ppq: u16,
    /// Tempo, 1..16777215: the 24-bit SMF Set Tempo value.
    #[schemars(range(min = 1, max = MAX_MICROSECONDS_PER_QUARTER))]
    pub microseconds_per_quarter: u32,
}

impl MidiTiming {
    /// Exact file ticks for an effect position in microseconds.
    pub fn ticks_for_microseconds(&self, microseconds: u64) -> Rational {
        Rational::new(
            i128::from(microseconds) * i128::from(self.ppq),
            i128::from(self.microseconds_per_quarter),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceAssignment {
    pub voice: String,
    pub track: String,
    /// MIDI channel as the 0–15 status nibble; messages and voice IDs
    /// number channels 1–16.
    pub channel: u8,
    /// Retained receiver identity for a multi-port source; omitted means port 0.
    /// Single-receiver sources keep their original routing without this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u8>,
}

/// Largest PPQ representable in a metrical SMF header.
const MAX_PPQ: u16 = 0x7FFF;

/// With `source_backed`, shared channels come from the retained source; the
/// source writer checks them by readback instead of refusing shared state.
pub(crate) fn check_midi_export(
    timebase: Timebase,
    composition: &Composition,
    export: &MidiExport,
    source_backed: bool,
    report: &mut Report,
) {
    match (timebase, export.timing) {
        (Timebase::Music { .. }, Some(_)) => report.push(
            Code::InvalidExportTiming,
            "timing",
            "music exports use the composition's PPQ and tempo map",
        ),
        (Timebase::Effect, None) => report.push(
            Code::InvalidExportTiming,
            "timing",
            "effect exports need explicit MIDI timing",
        ),
        (Timebase::Effect, Some(timing)) => {
            if timing.ppq == 0
                || timing.ppq > MAX_PPQ
                || timing.microseconds_per_quarter == 0
                || timing.microseconds_per_quarter > MAX_MICROSECONDS_PER_QUARTER
            {
                report.push(
                    Code::InvalidExportTiming,
                    "timing",
                    "PPQ must be 1–32767 and tempo 1–16777215",
                );
            } else {
                check_effect_grid(composition, timing, report);
            }
        }
        (Timebase::Music { ppq }, None) if ppq > MAX_PPQ => report.push(
            Code::InvalidExportTiming,
            "timing",
            "the project's PPQ does not fit a MIDI header",
        ),
        (Timebase::Music { .. }, None) => {}
    }

    let mut assigned = HashSet::new();
    let mut channel_voices: BTreeMap<u8, Vec<&str>> = BTreeMap::new();
    for (i, assignment) in export.assignments.iter().enumerate() {
        if !composition.voices.iter().any(|v| v.id == assignment.voice) {
            report.push(
                Code::UnknownReference,
                format!("assignments/{i}/voice"),
                format!("voice `{}` does not exist", assignment.voice),
            );
        } else if !assigned.insert(assignment.voice.as_str()) {
            report.push(
                Code::DuplicateId,
                format!("assignments/{i}/voice"),
                format!("voice `{}` is assigned twice", assignment.voice),
            );
        }
        if assignment.track.is_empty() {
            report.push(
                Code::InvalidRange,
                format!("assignments/{i}/track"),
                "track name must not be empty",
            );
        }
        if assignment.port.is_some_and(|port| port > 127)
            || (!source_backed && assignment.port.is_some())
        {
            report.push(Code::ValueOutOfRange, format!("assignments/{i}/port"), "port is a retained source identity in 0–127; new receiver assignments are not supported");
        }
        if assignment.channel > 15 {
            report.push(
                Code::ValueOutOfRange,
                format!("assignments/{i}/channel"),
                "MIDI channel must be 0–15",
            );
        } else {
            channel_voices
                .entry(assignment.channel)
                .or_default()
                .push(assignment.voice.as_str());
        }
    }
    for voice in composition
        .voices
        .iter()
        .filter(|v| !assigned.contains(v.id.as_str()))
    {
        report.push(
            Code::UnassignedVoice,
            "assignments",
            format!("voice `{}` has no track and channel", voice.id),
        );
    }

    if source_backed {
        return;
    }
    for (channel, voices) in channel_voices.iter().filter(|(_, voices)| voices.len() > 1) {
        if let Some(lane) = composition
            .control_lanes
            .iter()
            .find(|l| voices.contains(&l.voice.as_str()))
        {
            report.push(
                Code::SharedChannelControl,
                "assignments",
                format!("MIDI channel {} (assignment channel {channel}) is shared by {voices:?}, so the {:?} lane of `{}` would affect all of them", channel + 1, lane.kind, lane.voice),
            );
        }
        let mut by_pitch: BTreeMap<u8, Vec<usize>> = BTreeMap::new();
        for (i, note) in composition
            .notes
            .iter()
            .enumerate()
            .filter(|(_, n)| voices.contains(&n.voice.as_str()))
        {
            by_pitch.entry(note.pitch).or_default().push(i);
        }
        for indices in by_pitch.values_mut() {
            report_overlaps(
                &composition.notes,
                indices,
                report,
                &format!(
                    "MIDI channel {} (assignment channel {channel})",
                    channel + 1
                ),
            );
        }
    }
}

fn check_effect_grid(composition: &Composition, timing: MidiTiming, report: &mut Report) {
    let mut times: Vec<(String, u64)> = vec![("playback end".into(), composition.playback.end)];
    times.extend(
        composition
            .playback
            .loop_start
            .map(|t| ("loop start".into(), t)),
    );
    for note in &composition.notes {
        times.push((format!("start of note `{}`", note.id), note.start));
        times.extend(
            note.end()
                .map(|end| (format!("end of note `{}`", note.id), end)),
        );
    }
    for lane in &composition.control_lanes {
        for point in &lane.points {
            times.push((
                format!("{:?} point of `{}`", lane.kind, lane.voice),
                point.at,
            ));
        }
    }
    for (what, microseconds) in times {
        let ticks = timing.ticks_for_microseconds(microseconds);
        if !ticks.is_integer() {
            report.push(
                Code::EffectTimeNotRepresentable,
                "timing",
                format!("{what} at {microseconds} µs is {ticks} ticks; quantize it explicitly or choose finer timing"),
            );
        }
    }
}
