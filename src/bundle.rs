//! A self-contained project: its revisions and everything that refers to them.

use std::collections::{HashMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::composition::{Composition, StackedNotes, Timebase, check_composition_with};
use crate::diagnostic::{Diagnostic, DiagnosticCode as Code, Report};
use crate::edit::{EditRequest, apply_operations, check_edit_bounds};
use crate::export::{MidiExport, check_midi_export};
use crate::review::{Feedback, FeedbackState, Presentation, Render, RenderLayout, SourceRender};
use crate::timeline::{Rational, content_seconds};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectBundle {
    pub project: ProjectInfo,
    /// Immutable revisions, each listed after its parent.
    pub revisions: Vec<Revision>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renders: Vec<Render>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presentations: Vec<Presentation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<Feedback>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub midi_exports: Vec<MidiExport>,
    /// Workspace records the revisions refer to: sound sources of audio
    /// clips, and the instrument banks, FM banks and sample plans of chip
    /// instruments. The bytes they name travel in the delivery's `objects/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ReferencedRecord>,
    /// Listening on source-only renders of `project.midi_source`. These are
    /// not project renders and refer to no revision; their WAVs travel in
    /// the delivery's `source-renders/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_reviews: Option<SourceReviews>,
}

/// Source renders of one retained MIDI source, their presentations and the
/// feedback on them, including undelivered drafts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceReviews {
    /// SHA-256 of the source; equals the project's `midi_source`.
    pub source: String,
    pub renders: Vec<SourceRender>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presentations: Vec<Presentation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<Feedback>,
}

/// Collections a bundle can carry records of.
pub const REFERENCED_COLLECTIONS: [&str; 4] = [
    "instrument_source",
    "instrument_bank",
    "fm_bank",
    "sample_plan",
];

/// One stored record, exactly as the workspace keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferencedRecord {
    /// `instrument_source`, `instrument_bank`, `fm_bank` or `sample_plan`.
    pub collection: String,
    /// The record's identity: the source's SHA-256, or the SHA-256 of the
    /// record's canonical JSON for banks and plans.
    pub id: String,
    pub record: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectInfo {
    pub name: String,
    pub timebase: Timebase,
    /// The revision new edits build on by default.
    pub head_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<ProjectBrief>,
    /// Original MIDI events and byte identity; survives export and resumption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_source: Option<crate::midi::MidiSource>,
}

/// What the work is for. Requirements come from the request; assumptions are
/// choices the agent made where the request was silent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectBrief {
    pub purpose: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
}

/// A root revision has neither parent nor request. Every other revision
/// records the request that produced it from its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_request: Option<EditRequest>,
    pub composition: Composition,
}

/// Checks every document in the bundle and the references between them.
/// An empty result means the bundle satisfies the contract.
pub fn check_project_bundle(bundle: &ProjectBundle) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut report = Report::new("", &mut diagnostics);
    let timebase = bundle.project.timebase;
    let timebase_valid = !matches!(timebase, Timebase::Music { ppq: 0 });
    if !timebase_valid {
        report.push(
            Code::InvalidTimebase,
            "project/timebase/ppq",
            "PPQ must be positive",
        );
    }
    if bundle.project.name.is_empty() {
        report.push(
            Code::InvalidRange,
            "project/name",
            "project name must not be empty",
        );
    }

    let revisions = check_revisions(bundle, timebase_valid, &mut report);
    if timebase_valid && let Some(stored_source) = &bundle.project.midi_source {
        let mut source = stored_source.clone();
        if let Some(root) = bundle.revisions.iter().find(|r| r.parent.is_none()) {
            crate::midi::resolve_declared_projection(&mut source, &root.composition);
        }
        let source = &source;
        for (index, revision) in bundle.revisions.iter().enumerate() {
            if let Some((_, true)) = revisions.get(revision.id.as_str())
                && let Err(problems) =
                    crate::midi::check_source_revision(source, timebase, &revision.composition)
            {
                for problem in problems {
                    report.push(
                        problem.code,
                        format!("revisions/{index}{}", problem.location),
                        problem.message,
                    );
                }
            }
            if let (Some(parent), Some(request)) = (&revision.parent, &revision.edit_request)
                && let Some((base, true)) = revisions.get(parent.as_str())
                && let Err(problems) =
                    crate::midi::check_source_bounds(source, base, &revision.composition, request)
            {
                for problem in problems {
                    report.push(
                        problem.code,
                        format!("revisions/{index}/edit_request{}", problem.location),
                        problem.message,
                    );
                }
            }
        }
    }
    for (i, revision) in bundle.revisions.iter().enumerate() {
        let Some(request) = &revision.edit_request else {
            continue;
        };
        for (j, feedback) in request.feedback.iter().enumerate() {
            if !bundle.feedback.iter().any(|f| &f.id == feedback) {
                report.push(
                    Code::UnknownReference,
                    format!("revisions/{i}/edit_request/feedback/{j}"),
                    format!("feedback `{feedback}` does not exist"),
                );
            }
        }
    }
    if !bundle
        .revisions
        .iter()
        .any(|r| r.id == bundle.project.head_revision)
    {
        report.push(
            Code::UnknownReference,
            "project/head_revision",
            format!("revision `{}` does not exist", bundle.project.head_revision),
        );
    }

    let mut render_lengths = HashMap::new();
    check_unique_ids(
        bundle.renders.iter().map(|r| r.id.as_str()),
        "renders",
        &mut report,
    );
    for (i, render) in bundle.renders.iter().enumerate() {
        let mut report = report.nested(format!("renders/{i}"));
        render_lengths.insert(
            render.id.as_str(),
            (render.length_samples, render.revision.as_str()),
        );
        match revisions.get(render.revision.as_str()) {
            None => report.push(
                Code::UnknownReference,
                "revision",
                format!("revision `{}` does not exist", render.revision),
            ),
            Some(&(composition, valid)) => {
                check_render(timebase, composition, valid, render, &mut report)
            }
        }
    }

    check_unique_ids(
        bundle.presentations.iter().map(|p| p.id.as_str()),
        "presentations",
        &mut report,
    );
    for (i, presentation) in bundle.presentations.iter().enumerate() {
        if presentation.renders.is_empty() {
            report.push(
                Code::InvalidRange,
                format!("presentations/{i}/renders"),
                "a presentation shows at least one render",
            );
        }
        for (j, item) in presentation.renders.iter().enumerate() {
            if !render_lengths.contains_key(item.render.as_str()) {
                report.push(
                    Code::UnknownReference,
                    format!("presentations/{i}/renders/{j}/render"),
                    format!("render `{}` does not exist", item.render),
                );
            }
        }
    }

    check_unique_ids(
        bundle.feedback.iter().map(|f| f.id.as_str()),
        "feedback",
        &mut report,
    );
    let mut sequences = HashSet::new();
    for (i, feedback) in bundle.feedback.iter().enumerate() {
        let mut report = report.nested(format!("feedback/{i}"));
        if let FeedbackState::Delivered { sequence } = feedback.state {
            if !sequences.insert(sequence) {
                report.push(
                    Code::DuplicateId,
                    "state/sequence",
                    format!("delivery sequence {sequence} is used twice"),
                );
            }
            if feedback.marks.is_empty() && feedback.text.as_deref().is_none_or(str::is_empty) {
                report.push(
                    Code::EmptyDeliveredFeedback,
                    "",
                    "delivered feedback needs a mark or text",
                );
            }
        }
        let presentation = match &feedback.presentation {
            None => None,
            Some(id) => match bundle.presentations.iter().find(|p| &p.id == id) {
                Some(presentation) => Some(presentation),
                None => {
                    report.push(
                        Code::UnknownReference,
                        "presentation",
                        format!("presentation `{id}` does not exist"),
                    );
                    continue;
                }
            },
        };
        for (j, mark) in feedback.marks.iter().enumerate() {
            let mut report = report.nested(format!("marks/{j}"));
            let presented = |render: &str| {
                presentation.is_none_or(|p| p.renders.iter().any(|item| item.render == render))
            };
            let Some(&(length, revision)) = render_lengths
                .get(mark.render.as_str())
                .filter(|_| presented(&mark.render))
            else {
                report.push(
                    Code::UnknownReference,
                    "render",
                    format!(
                        "render `{}` does not exist or is not part of the presentation",
                        mark.render
                    ),
                );
                continue;
            };
            if mark.start_sample >= length {
                report.push(
                    Code::MarkOutsideRender,
                    "start_sample",
                    format!("the render has {length} samples"),
                );
            }
            if let Some(end) = mark.end_sample
                && (end <= mark.start_sample || end > length)
            {
                report.push(
                    Code::MarkOutsideRender,
                    "end_sample",
                    format!("a range must end after its start and within {length} samples"),
                );
            }
            if let Some(voice) = &mark.voice
                && revisions
                    .get(revision)
                    .is_some_and(|(c, _)| !c.voices.iter().any(|v| &v.id == voice))
            {
                report.push(
                    Code::UnknownReference,
                    "voice",
                    format!("voice `{voice}` is not in the rendered revision"),
                );
            }
        }
    }

    check_unique_ids(
        bundle.midi_exports.iter().map(|e| e.id.as_str()),
        "midi_exports",
        &mut report,
    );
    for (i, export) in bundle.midi_exports.iter().enumerate() {
        let mut report = report.nested(format!("midi_exports/{i}"));
        match revisions.get(export.revision.as_str()) {
            None => report.push(
                Code::UnknownReference,
                "revision",
                format!("revision `{}` does not exist", export.revision),
            ),
            Some(&(composition, true)) => check_midi_export(
                timebase,
                composition,
                export,
                bundle.project.midi_source.is_some(),
                &mut report,
            ),
            Some(_) => {}
        }
    }

    let mut references = HashSet::new();
    for (i, reference) in bundle.references.iter().enumerate() {
        if !REFERENCED_COLLECTIONS.contains(&reference.collection.as_str()) {
            report.push(
                Code::InvalidRange,
                format!("references/{i}/collection"),
                format!(
                    "a bundle carries records of {}",
                    REFERENCED_COLLECTIONS.join(", ")
                ),
            );
        }
        if !references.insert((reference.collection.as_str(), reference.id.as_str())) {
            report.push(
                Code::DuplicateId,
                format!("references/{i}"),
                format!(
                    "{} `{}` is carried twice",
                    reference.collection, reference.id
                ),
            );
        }
    }
    if let Some(reviews) = &bundle.source_reviews {
        let mut report = report.nested("source_reviews");
        check_source_reviews(bundle, reviews, &mut report);
    }

    diagnostics
}

/// Source reviews belong to the project's own source. Presentations and
/// marks refer only to its source renders; marks name no voice.
fn check_source_reviews(bundle: &ProjectBundle, reviews: &SourceReviews, report: &mut Report) {
    if bundle
        .project
        .midi_source
        .as_ref()
        .map(|s| s.sha256.as_str())
        != Some(&reviews.source)
    {
        report.push(
            Code::UnknownReference,
            "source",
            "source reviews must belong to the project's retained MIDI source",
        );
    }
    check_unique_ids(
        reviews.renders.iter().map(|r| r.id.as_str()),
        "renders",
        report,
    );
    let lengths: HashMap<&str, u64> = reviews
        .renders
        .iter()
        .map(|r| (r.id.as_str(), r.length_samples))
        .collect();
    for (i, render) in reviews.renders.iter().enumerate() {
        if render.source != reviews.source {
            report.push(
                Code::UnknownReference,
                format!("renders/{i}/source"),
                "a source render must play the reviewed source",
            );
        }
        if render.content_samples > render.length_samples {
            report.push(
                Code::InvalidRange,
                format!("renders/{i}/content_samples"),
                "content cannot exceed the render length",
            );
        }
    }
    check_unique_ids(
        reviews.presentations.iter().map(|p| p.id.as_str()),
        "presentations",
        report,
    );
    for (i, presentation) in reviews.presentations.iter().enumerate() {
        if presentation.renders.is_empty() {
            report.push(
                Code::InvalidRange,
                format!("presentations/{i}/renders"),
                "a presentation shows at least one render",
            );
        }
        for (j, item) in presentation.renders.iter().enumerate() {
            if !lengths.contains_key(item.render.as_str()) {
                report.push(
                    Code::UnknownReference,
                    format!("presentations/{i}/renders/{j}/render"),
                    format!("source render `{}` does not exist", item.render),
                );
            }
        }
    }
    check_unique_ids(
        reviews.feedback.iter().map(|f| f.id.as_str()),
        "feedback",
        report,
    );
    let mut sequences = HashSet::new();
    for (i, feedback) in reviews.feedback.iter().enumerate() {
        if let FeedbackState::Delivered { sequence } = feedback.state
            && !sequences.insert(sequence)
        {
            report.push(
                Code::DuplicateId,
                format!("feedback/{i}/state/sequence"),
                format!("delivery sequence {sequence} is used twice"),
            );
        }
        let presentation = match &feedback.presentation {
            None => None,
            Some(id) => match reviews.presentations.iter().find(|p| &p.id == id) {
                Some(presentation) => Some(presentation),
                None => {
                    report.push(
                        Code::UnknownReference,
                        format!("feedback/{i}/presentation"),
                        format!("presentation `{id}` does not exist"),
                    );
                    continue;
                }
            },
        };
        for (j, mark) in feedback.marks.iter().enumerate() {
            let presented = presentation
                .is_none_or(|p| p.renders.iter().any(|item| item.render == mark.render));
            let Some(&length) = lengths.get(mark.render.as_str()).filter(|_| presented) else {
                report.push(
                    Code::UnknownReference,
                    format!("feedback/{i}/marks/{j}/render"),
                    format!(
                        "source render `{}` does not exist or is not part of the presentation",
                        mark.render
                    ),
                );
                continue;
            };
            if mark.start_sample >= length
                || mark
                    .end_sample
                    .is_some_and(|e| e <= mark.start_sample || e > length)
            {
                report.push(
                    Code::MarkOutsideRender,
                    format!("feedback/{i}/marks/{j}"),
                    format!("the render has {length} samples"),
                );
            }
            if mark.voice.is_some() {
                report.push(
                    Code::InvalidRange,
                    format!("feedback/{i}/marks/{j}/voice"),
                    "source renders have no voices",
                );
            }
        }
    }
}

/// Returns each revision's composition and whether it passed its checks.
fn check_revisions<'a>(
    bundle: &'a ProjectBundle,
    timebase_valid: bool,
    report: &mut Report,
) -> HashMap<&'a str, (&'a Composition, bool)> {
    let mut checked: HashMap<&str, (&Composition, bool)> = HashMap::new();
    let mut job_ids = HashSet::new();
    if bundle.revisions.is_empty() {
        report.push(
            Code::RevisionLineage,
            "revisions",
            "a project has at least one revision",
        );
    }
    for (i, revision) in bundle.revisions.iter().enumerate() {
        let mut report = report.nested(format!("revisions/{i}"));
        if checked.contains_key(revision.id.as_str()) {
            report.push(
                Code::DuplicateId,
                "id",
                format!("revision `{}` is defined twice", revision.id),
            );
        }
        let mut composition_diagnostics = Vec::new();
        if timebase_valid {
            check_composition_with(
                bundle.project.timebase,
                &revision.composition,
                match bundle.project.midi_source {
                    Some(_) => StackedNotes::SourcePaired,
                    None => StackedNotes::Reject,
                },
                &mut Report::new("", &mut composition_diagnostics),
            );
        }
        let valid = timebase_valid && composition_diagnostics.is_empty();
        for diagnostic in composition_diagnostics {
            report.push(
                diagnostic.code,
                format!("composition{}", diagnostic.location),
                diagnostic.message,
            );
        }

        match (&revision.parent, &revision.edit_request) {
            (None, None) => {}
            (None, Some(_)) => report.push(
                Code::RevisionLineage,
                "edit_request",
                "a root revision has no edit request",
            ),
            (Some(_), None) => report.push(
                Code::RevisionLineage,
                "edit_request",
                "a derived revision records its edit request",
            ),
            (Some(parent), Some(request)) => {
                if !job_ids.insert(request.job_id.as_str()) {
                    report.push(
                        Code::DuplicateId,
                        "edit_request/job_id",
                        format!("job `{}` produced another revision", request.job_id),
                    );
                }
                if &request.base_revision != parent {
                    report.push(
                        Code::RevisionLineage,
                        "edit_request/base_revision",
                        "the request's base must be the parent revision",
                    );
                }
                match checked.get(parent.as_str()) {
                    None if bundle.revisions.iter().any(|r| &r.id == parent) => report.push(
                        Code::RevisionLineage,
                        "parent",
                        "a parent must be listed before its children",
                    ),
                    None => report.push(
                        Code::UnknownReference,
                        "parent",
                        format!("revision `{parent}` does not exist"),
                    ),
                    Some(&(_, false)) => {}
                    Some(&(base, true)) => check_derivation(base, revision, request, &mut report),
                }
            }
        }
        checked
            .entry(revision.id.as_str())
            .or_insert((&revision.composition, valid));
    }
    checked
}

fn check_derivation(
    base: &Composition,
    revision: &Revision,
    request: &EditRequest,
    report: &mut Report,
) {
    match apply_operations(base, request) {
        Err(diagnostics) => {
            for diagnostic in diagnostics {
                report.push(
                    diagnostic.code,
                    format!("edit_request{}", diagnostic.location),
                    diagnostic.message,
                );
            }
        }
        Ok(result) if result != revision.composition => report.push(
            Code::RevisionContentMismatch,
            "composition",
            "applying the edit request to the parent gives different content",
        ),
        Ok(_) => check_edit_bounds(
            base,
            &revision.composition,
            request,
            &mut report.nested("edit_request"),
        ),
    }
}

fn check_render(
    timebase: Timebase,
    composition: &Composition,
    revision_valid: bool,
    render: &Render,
    report: &mut Report,
) {
    if let Some(voices) = &render.voices {
        for (j, voice) in voices.iter().enumerate() {
            if !composition.voices.iter().any(|v| &v.id == voice) {
                report.push(
                    Code::UnknownReference,
                    format!("voices/{j}"),
                    format!("voice `{voice}` does not exist"),
                );
            }
        }
    }
    if render.sample_rate == 0 {
        report.push(
            Code::ValueOutOfRange,
            "sample_rate",
            "sample rate must be positive",
        );
    }
    let layout_valid = match render.layout {
        RenderLayout::Full { loop_passes: 0 } => {
            report.push(
                Code::InvalidRenderLayout,
                "layout/loop_passes",
                "play the content at least once",
            );
            false
        }
        RenderLayout::Full { loop_passes }
            if loop_passes > 1 && composition.playback.loop_start.is_none() =>
        {
            report.push(
                Code::InvalidRenderLayout,
                "layout/loop_passes",
                "only a composition with a loop can repeat",
            );
            false
        }
        RenderLayout::Excerpt { start, end } if start >= end || end > composition.playback.end => {
            report.push(
                Code::InvalidRenderLayout,
                "layout",
                "an excerpt is a non-empty range within the playback end",
            );
            false
        }
        _ => true,
    };
    if revision_valid && layout_valid && render.sample_rate > 0 {
        let needed = (content_seconds(timebase, composition, &render.layout)
            * i128::from(render.sample_rate))
        .ceil();
        if Rational::from_integer(i128::from(render.length_samples)) < needed {
            report.push(
                Code::RenderTooShort,
                "length_samples",
                format!("the layout needs at least {needed} samples of content"),
            );
        }
    }
}

fn check_unique_ids<'a>(ids: impl Iterator<Item = &'a str>, collection: &str, report: &mut Report) {
    let mut seen = HashSet::new();
    for (i, id) in ids.enumerate() {
        if id.is_empty() {
            report.push(
                Code::InvalidRange,
                format!("{collection}/{i}/id"),
                "id must not be empty",
            );
        } else if !seen.insert(id) {
            report.push(
                Code::DuplicateId,
                format!("{collection}/{i}/id"),
                format!("`{id}` is defined twice"),
            );
        }
    }
}
