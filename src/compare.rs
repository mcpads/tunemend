//! Differences between two revisions of the same project.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::Serialize;

use crate::audio_clips::AudioClip;
use crate::composition::{Composition, ControlKind, ControlPoint, Note};
use crate::edit::TimeRange;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RevisionDifference {
    pub added_notes: Vec<Note>,
    pub removed_notes: Vec<Note>,
    pub changed_notes: Vec<NoteChange>,
    pub changed_controls: Vec<ControlChange>,
    pub added_voices: Vec<String>,
    pub removed_voices: Vec<String>,
    /// Timeline parts that differ: tempo map, meter, keys, sections, markers, playback.
    pub changed_timeline: Vec<String>,
    /// Mixer items that differ: voice strips, buses, sends, master, automation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_mix: Vec<String>,
    /// Audio clips that were added, removed or changed, with both versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_clips: Vec<ClipChange>,
    pub unchanged_notes: usize,
}

/// A clip's record before and after; `None` where it does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ClipChange {
    pub id: String,
    pub before: Option<AudioClip>,
    pub after: Option<AudioClip>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct NoteChange {
    pub before: Note,
    pub after: Note,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ControlChange {
    pub voice: String,
    pub kind: ControlKind,
    pub before: Vec<ControlPoint>,
    pub after: Vec<ControlPoint>,
}

/// Compares notes by id. With `range` or `voices`, only notes and controls
/// touching that range or those voices in either revision are listed.
pub fn compare_compositions(
    before: &Composition,
    after: &Composition,
    range: Option<TimeRange>,
    voices: Option<&[String]>,
) -> RevisionDifference {
    let selected_voice = |voice: &str| voices.is_none_or(|v| v.iter().any(|x| x == voice));
    let in_range = |note: &Note| {
        range.is_none_or(|r| note.start < r.end && note.start + note.duration > r.start)
    };
    let selected = |note: &Note| selected_voice(&note.voice) && in_range(note);
    let by_id = |c: &Composition| -> BTreeMap<String, Note> {
        c.notes.iter().map(|n| (n.id.clone(), n.clone())).collect()
    };
    let (old, new) = (by_id(before), by_id(after));
    let mut difference = RevisionDifference::default();
    for id in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        match (old.get(id), new.get(id)) {
            (Some(a), Some(b)) if a == b => difference.unchanged_notes += usize::from(selected(a)),
            (Some(a), Some(b)) if selected(a) || selected(b) => {
                difference.changed_notes.push(NoteChange {
                    before: a.clone(),
                    after: b.clone(),
                })
            }
            (Some(a), None) if selected(a) => difference.removed_notes.push(a.clone()),
            (None, Some(b)) if selected(b) => difference.added_notes.push(b.clone()),
            _ => {}
        }
    }

    let lanes = |c: &Composition| -> BTreeMap<(String, ControlKind), Vec<ControlPoint>> {
        c.control_lanes
            .iter()
            .map(|l| ((l.voice.clone(), l.kind), l.points.clone()))
            .collect()
    };
    let (old_lanes, new_lanes) = (lanes(before), lanes(after));
    for key in old_lanes
        .keys()
        .chain(new_lanes.keys())
        .collect::<BTreeSet<_>>()
    {
        let (a, b) = (
            old_lanes.get(key).cloned().unwrap_or_default(),
            new_lanes.get(key).cloned().unwrap_or_default(),
        );
        let touches_range = |points: &[ControlPoint]| {
            range.is_none_or(|r| points.iter().any(|p| r.start <= p.at && p.at < r.end))
        };
        if a != b && selected_voice(&key.0) && (touches_range(&a) || touches_range(&b)) {
            difference.changed_controls.push(ControlChange {
                voice: key.0.clone(),
                kind: key.1,
                before: a,
                after: b,
            });
        }
    }

    let voice_ids =
        |c: &Composition| -> BTreeSet<String> { c.voices.iter().map(|v| v.id.clone()).collect() };
    let (old_voices, new_voices) = (voice_ids(before), voice_ids(after));
    difference.added_voices = new_voices.difference(&old_voices).cloned().collect();
    difference.removed_voices = old_voices.difference(&new_voices).cloned().collect();

    for (name, changed) in [
        ("tempo_map", before.tempo_map != after.tempo_map),
        (
            "time_signatures",
            before.time_signatures != after.time_signatures,
        ),
        (
            "key_signatures",
            before.key_signatures != after.key_signatures,
        ),
        ("sections", before.sections != after.sections),
        ("markers", before.markers != after.markers),
        ("playback", before.playback != after.playback),
    ] {
        if changed {
            difference.changed_timeline.push(name.into());
        }
    }
    difference.changed_mix = crate::mix::mix_changes(&before.mix, &after.mix)
        .into_iter()
        .filter(|(voice, _)| voice.as_deref().is_none_or(selected_voice))
        .map(|(_, label)| label)
        .collect();
    let find = |clips: &[AudioClip], id: &str| clips.iter().find(|c| c.id == id).cloned();
    let clip_selected = |clip: &AudioClip| {
        selected_voice(&clip.voice)
            && range.is_none_or(|r| clip.start < r.end && clip.end().unwrap_or(u64::MAX) > r.start)
    };
    for id in crate::audio_clips::changed_clips(&before.mix.clips, &after.mix.clips) {
        let (a, b) = (find(&before.mix.clips, id), find(&after.mix.clips, id));
        if a.iter().chain(&b).any(clip_selected) {
            difference.changed_clips.push(ClipChange {
                id: id.to_owned(),
                before: a,
                after: b,
            });
        }
    }
    difference
}
