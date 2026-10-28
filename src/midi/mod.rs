//! Standard MIDI File projection, source-preserving editing and verification.
//!
//! The service retains original bytes and edits selected modeled fields. The
//! strict read_smf/write_smf pair remains available for normalized composition
//! interchange. Ambiguous projections block with event locations.
//!
//! Note pairing is per track, channel and key. A release ends the earliest
//! still-sounding onset (first in, first out), so stacked onsets stay separate
//! notes. The preserving projection keeps unpaired and zero-length note events
//! in the source without an editable note instead of inventing durations.

mod comparison;
mod export;
mod import;
mod inspection;
mod preservation;
mod source;

pub use comparison::{
    DEFAULT_DIFFERENCE_PAGE, MAX_DIFFERENCE_PAGE, compare_midi, compare_midi_page,
};
pub use export::write_smf;
pub use import::{read_smf, read_smf_declaring, read_smf_preserving};
pub use inspection::{analyze_smf, gm_initialization, preview_blockers, reference_channel_message};
pub use preservation::{
    HeldRetainedNote, check_source_bounds, check_source_edit, check_source_revision,
    declared_end_changes, declared_notes, held_retained_notes, write_preserving,
    write_preserving_declared,
};
pub use source::{
    EmbeddedMidi, container_summary, embedded_midi, replace_source_tracks, source_events,
    source_layout, source_summary,
};

use std::collections::{BTreeMap, HashMap};

use midly::num::{u4, u7, u15, u24, u28};
use midly::{
    Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, Timing, TrackEvent, TrackEventKind,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::composition::{
    Composition, ControlKind, ControlLane, ControlPoint, KeySignature, Marker, MarkerKind, Note,
    Playback, StackedNotes, TempoChange, TimeSignature, Timebase, Voice, check_composition,
    check_composition_with,
};
use crate::diagnostic::{Diagnostic, DiagnosticCode as Code, Report};
use crate::export::{MidiExport, VoiceAssignment, check_midi_export};
use crate::performance::{
    CC_CHORUS, CC_DATA_ENTRY, CC_DATA_ENTRY_LSB, CC_EXPRESSION, CC_MODULATION, CC_PAN, CC_REVERB,
    CC_RPN_LSB, CC_RPN_MSB, CC_VOLUME, ChannelMessage, DEFAULT_RELEASE_VELOCITY, RPN_NULL,
    channel_events,
};

/// Tempo a Standard MIDI File implies before its first tempo event.
pub const DEFAULT_MICROSECONDS_PER_QUARTER: u32 = 500_000;
/// Pitch bend range a General MIDI receiver assumes before RPN 0 is sent.
pub const DEFAULT_PITCH_BEND_RANGE: i32 = 2;
pub const LOOP_START_MARKER: &str = "loopStart";
pub const LOOP_END_MARKER: &str = "loopEnd";

/// A change between the file and the imported project that keeps the
/// supported meaning but is not byte-for-byte identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Normalization {
    pub location: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedMidi {
    pub timebase: Timebase,
    pub composition: Composition,
    /// How the file's tracks and channels map to the imported voices.
    pub assignments: Vec<VoiceAssignment>,
    pub normalizations: Vec<Normalization>,
    pub retained: Vec<Normalization>,
    /// The source events behind each projected note, in note order.
    pub note_bindings: Vec<NoteBinding>,
    /// Note events kept in the source without an editable note: releases
    /// without a sounding note, notes never released and zero-length pairs.
    pub retained_note_events: Vec<RetainedNoteEvent>,
}

/// A source event position: track index and event index within that track.
pub type SourceEvent = (usize, usize);

/// Projected note `id` is the onset and release at these source positions.
/// Bindings are recomputed from the immutable source, so note IDs keep the
/// same events across revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteBinding {
    pub id: String,
    pub onset: SourceEvent,
    /// `None` for a never-released note given a declared end.
    pub release: Option<SourceEvent>,
    /// Set when the projected end comes from the source's [`DeclaredEnd`]
    /// rule rather than from a source release.
    pub declared: Option<DeclaredFrom>,
}

/// The source pattern a declared end completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredFrom {
    NeverReleased,
    ZeroLength,
}

fn both_patterns() -> Vec<DeclaredFrom> {
    vec![DeclaredFrom::NeverReleased, DeclaredFrom::ZeroLength]
}

fn is_both_patterns(patterns: &[DeclaredFrom]) -> bool {
    patterns == both_patterns().as_slice()
}

/// An explicitly selected end for notes the source leaves without a
/// positive length: notes never released and zero-length pairs. Without
/// it they stay retained without an editable note. The source is unchanged
/// until an edit touches such a note, or an export writes declared ends.
/// `apply_to` limits the completed patterns; omitted, both are completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "end", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeclaredEnd {
    /// The next onset of the same key in the same track and channel, or the
    /// track end when none follows.
    NextOnset {
        #[serde(default = "both_patterns", skip_serializing_if = "is_both_patterns")]
        apply_to: Vec<DeclaredFrom>,
    },
    /// The end of the note's track.
    TrackEnd {
        #[serde(default = "both_patterns", skip_serializing_if = "is_both_patterns")]
        apply_to: Vec<DeclaredFrom>,
    },
    /// A fixed length in ticks, shortened to the track end.
    Fixed {
        ticks: u64,
        #[serde(default = "both_patterns", skip_serializing_if = "is_both_patterns")]
        apply_to: Vec<DeclaredFrom>,
    },
}

impl DeclaredEnd {
    /// The source patterns this rule completes.
    pub fn apply_to(&self) -> &[DeclaredFrom] {
        match self {
            Self::NextOnset { apply_to }
            | Self::TrackEnd { apply_to }
            | Self::Fixed { apply_to, .. } => apply_to,
        }
    }
}

/// Why a note event stays in the source without an editable note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetainedKind {
    NeverReleased,
    ReleaseWithoutOnset,
    ZeroLength,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedNoteEvent {
    pub event: SourceEvent,
    pub tick: u64,
    /// Internal receiver/channel key; not a MIDI status nibble.
    pub channel: u16,
    pub message: MidiMessage,
    pub kind: RetainedKind,
}

/// Original bytes remain authoritative for imported MIDI events and their order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MidiSource {
    pub sha256: String,
    pub bytes: Vec<u8>,
    /// The end rule selected at import for notes without a positive length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_ends: Option<DeclaredEnd>,
    /// Saved projection policy. Missing values are resolved from the initial
    /// revision; legacy true retains pre-FIFO-check declared notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_declared_ends: Option<bool>,
}

/// Resolve an older project's declared-end interpretation from its stored root,
/// never from its current edits or a guessed producer version.
pub(crate) fn resolve_declared_projection(source: &mut MidiSource, root: &Composition) {
    if source.legacy_declared_ends.is_some() || source.declared_ends.is_none() {
        return;
    }
    let current = read_smf_declaring(&source.bytes, source.declared_ends.clone());
    if current
        .as_ref()
        .is_ok_and(|r| r.composition.notes == root.notes)
    {
        source.legacy_declared_ends = Some(false);
    } else if import::read_smf_legacy_declaring(&source.bytes, source.declared_ends.clone())
        .is_ok_and(|r| r.composition.notes == root.notes)
    {
        source.legacy_declared_ends = Some(true);
    }
}

/// Preserve historical single-receiver voice IDs, including nonzero source ports.
/// Multi-receiver keys pack the port above the four-bit MIDI channel.
pub(crate) fn receiver_channel(multiple: bool, port: u8, channel: u8) -> u16 {
    u16::from(channel) + if multiple { u16::from(port) * 16 } else { 0 }
}

pub(crate) fn source_voice(track: usize, channel: u16) -> String {
    if channel < 16 {
        format!("track{track}-ch{}", channel + 1)
    } else {
        format!("track{track}-port{}-ch{}", channel / 16, channel % 16 + 1)
    }
}

pub(crate) fn multiple_receivers(smf: &midly::Smf<'_>) -> bool {
    let mut ports = std::collections::BTreeSet::new();
    for events in &smf.tracks {
        let mut port = 0;
        for event in events {
            match event.kind {
                midly::TrackEventKind::Meta(midly::MetaMessage::MidiPort(value)) => {
                    port = value.as_int()
                }
                midly::TrackEventKind::Midi { .. }
                | midly::TrackEventKind::SysEx(_)
                | midly::TrackEventKind::Escape(_) => {
                    ports.insert(port);
                }
                _ => {}
            }
        }
    }
    ports.len() > 1
}
