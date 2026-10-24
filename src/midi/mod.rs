//! Standard MIDI File projection, source-preserving editing and verification.
//!
//! The service retains original bytes and edits selected modeled fields. The
//! strict read_smf/write_smf pair remains available for normalized composition
//! interchange. Ambiguous projections block with event locations.

mod export;
mod import;
mod inspection;
mod preservation;
mod source;

pub use export::write_smf;
pub use import::{read_smf, read_smf_preserving};
pub use inspection::{analyze_smf, preview_blockers};
pub use preservation::{check_source_edit, check_source_revision, compare_midi, write_preserving};
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
    Playback, TempoChange, TimeSignature, Timebase, Voice, check_composition,
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
}

/// Original bytes remain authoritative for imported MIDI events and their order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MidiSource {
    pub sha256: String,
    pub bytes: Vec<u8>,
}
