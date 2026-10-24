use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A blocking problem found while checking a document. `location` is a JSON
/// Pointer into the checked document so an agent can find the exact value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub location: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    DuplicateId,
    UnknownReference,
    InvalidTimebase,
    InvalidTempoMap,
    InvalidTimeSignature,
    MusicalTimingInEffect,
    InvalidRange,
    NonPositiveDuration,
    ValueOutOfRange,
    EventBeyondEnd,
    OverlappingNotes,
    NoteCrossesLoopStart,
    LoopStateMismatch,
    EmptyControlLane,
    DuplicateControlLane,
    ControlPointsUnordered,
    PitchBendRangeUnspecified,
    RevisionLineage,
    RevisionContentMismatch,
    EmptyEdit,
    OutOfScope,
    ProtectedChange,
    InvalidRenderLayout,
    RenderTooShort,
    MarkOutsideRender,
    EmptyDeliveredFeedback,
    InvalidExportTiming,
    UnassignedVoice,
    SharedChannelControl,
    EffectTimeNotRepresentable,
    UnsupportedMidi,
    UnsupportedMidiController,
    ConflictingMidiControl,
    UnpairedNoteEvent,
    JobConflict,
    StaleRevision,
    TargetRefused,
    TargetFailed,
    InvalidAudio,
    ChipVoicesExceeded,
    ClipCrossesLoopStart,
    MissingRenderAudio,
    /// The tool arguments do not fit the tool's input schema: an unknown or
    /// missing field, or a value of the wrong type or variant.
    SchemaMismatch,
}

/// Collects diagnostics with locations relative to a base pointer.
pub(crate) struct Report<'a> {
    base: String,
    out: &'a mut Vec<Diagnostic>,
}

impl<'a> Report<'a> {
    pub(crate) fn new(base: impl Into<String>, out: &'a mut Vec<Diagnostic>) -> Self {
        Self {
            base: base.into(),
            out,
        }
    }

    pub(crate) fn nested(&mut self, suffix: impl std::fmt::Display) -> Report<'_> {
        Report {
            base: format!("{}/{}", self.base, suffix),
            out: self.out,
        }
    }

    pub(crate) fn location(&self, suffix: impl std::fmt::Display) -> String {
        let suffix = suffix.to_string();
        if suffix.is_empty() {
            self.base.clone()
        } else {
            format!("{}/{suffix}", self.base)
        }
    }

    pub(crate) fn push(
        &mut self,
        code: DiagnosticCode,
        suffix: impl std::fmt::Display,
        message: impl Into<String>,
    ) {
        let suffix = suffix.to_string();
        let location = if suffix.is_empty() {
            self.base.clone()
        } else {
            format!("{}/{suffix}", self.base)
        };
        self.out.push(Diagnostic {
            code,
            location,
            message: message.into(),
        });
    }
}
