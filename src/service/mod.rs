//! Tool operations shared by the CLI and the MCP server.
//!
//! Every tool takes one JSON object and returns a [`ToolResponse`]. Tools
//! with a `job_id` change stored work; repeating a job returns its recorded
//! response instead of running again.

// Handlers return the finished response early as `Err`; it is built once per call.
#![allow(clippy::result_large_err)]

mod adx;
pub use adx::{
    AdxArithmetic, AdxChange, AdxFrameRange, AdxLoopFrames, AdxSpan, AdxView, EncodeAdxInput,
    InspectAdxInput, RankAdxLoopsInput, RenderAdxInput,
};
mod audition;
pub use audition::{DspEnvelope, DspPitch, RenderInstrumentInput, StereoVolume};
mod batch;
pub use batch::{
    MAX_BATCH_RENDERS, RenderBatchInput, RenderSpan, StemMaster, StemSelection, VoiceGroup,
};
mod delivery;
mod editing;
mod fm;
pub use fm::{CopyFmPatchInput, ExportFmBankInput, InspectFmBankInput, LoadFmBankInput};
mod fm_audition;
pub use fm_audition::RenderFmPatchInput;
mod inspection;
mod instrument_sources;
mod measurement;
mod nds_sound;
pub use measurement::{AudioInput, AudioRange, CompareAudioInput, MeasureAudioInput};
pub use nds_sound::{
    ExportNdsSampleInput, InspectNdsSoundInput, NdsEncoding, NdsView, ReplaceNdsWaveInput,
};
mod instruments;
pub use instrument_sources::{
    ExportInstrumentSourceInput, InspectInstrumentSourceInput, LoadInstrumentSourceInput,
};
mod observations;
pub use observations::{
    AttachObservationInput, ExportObservationInput, InspectObservationInput,
    ObservationExpectation, ObservationSubject,
};
mod positions;
mod psg;
pub use psg::{
    ChannelSelection, ChipFamily, ChipVariant, InspectVgmInput, PsgChip, RenderPsgInput, VgmRange,
    VgmView,
};
mod projects;
mod psx_seq;
pub use psx_seq::{
    ExportPsxSequenceInput, InspectPsxSequenceInput, LoadPsxSequenceInput, PsxSequenceView,
    RenderPsxSequenceInput,
};
mod review;
mod samples;
mod score;
pub use instruments::{
    CopyInstrumentInput, ExportInstrumentBankInput, InspectInstrumentBankInput,
    LoadInstrumentBankInput,
};
pub use samples::{ExportSampleInput, InspectSampleInput, PrepareSampleInput, SampleDetail};
mod source_review;
mod sources;
pub use score::{ExportTargetInput, InspectTargetInput, PrepareTargetInput};
mod audio_clips;
mod referenced_data;
mod surface;
mod tools;
pub use surface::{operation_group, surface_definitions};
mod transformation;
pub use transformation::TransformVgmInput;
mod vab;
pub use vab::{
    ExportVabBankInput, InspectVabBankInput, LoadVabBankInput, RenderVabToneInput,
    ReplaceVabSampleInput, VabFraming, VabLayout, VabPart, VabToneUpdate, VabView,
};
mod voice_instruments;

use inspection::capabilities;
pub use positions::bar_beat;
use positions::{nearest_label, position_label, rational_seconds, rational_text};
pub use tools::*;

use std::path::{Path, PathBuf};

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::bundle::{ProjectBrief, ProjectBundle, ProjectInfo, Revision, check_project_bundle};
use crate::compare::compare_compositions;
use crate::composition::{Composition, Note, Playback, TempoChange, TimeSignature, Timebase};
use crate::diagnostic::{Diagnostic, DiagnosticCode as Code};
use crate::edit::{EditRequest, TimeRange};
use crate::export::{MidiExport, MidiTiming, VoiceAssignment};
use crate::midi::write_smf;
use crate::profile::{ConstraintProfile, builtin_profiles, check_profile};
use crate::render::{MAX_TAIL_SECONDS, wav_bytes};
use crate::review::{
    Feedback, FeedbackState, Mark, Presentation, PresentedRender, Render, RenderLayout,
};
use crate::soundfont::{REFERENCE_SOUND_SOURCE, reference_soundfont};
use crate::store::{Store, StoreError, Writer, sha256_hex};
use crate::timeline::{Rational, RenderPosition, content_seconds, locate_in_render, seconds_at};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The tool did what was asked.
    Ok,
    /// The request conflicts with the contract; nothing was changed.
    Blocked,
    /// The environment failed, such as a missing file; retrying may help.
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResponse {
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// The job had already run; this is its recorded response.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replayed: bool,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    /// Files the tool wrote, as absolute paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    pub kind: String,
    pub path: String,
    pub sha256: String,
}

impl ToolResponse {
    fn ok(summary: impl Into<String>, result: Value) -> Self {
        Self {
            status: Status::Ok,
            job_id: None,
            replayed: false,
            summary: summary.into(),
            result,
            diagnostics: vec![],
            artifacts: vec![],
        }
    }

    fn blocked(summary: impl Into<String>, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            status: Status::Blocked,
            diagnostics,
            ..Self::ok(summary, Value::Null)
        }
    }

    fn refused(code: Code, location: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self::blocked(
            message.clone(),
            vec![Diagnostic {
                code,
                location: location.into(),
                message,
            }],
        )
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            status: Status::Error,
            ..Self::ok(message, Value::Null)
        }
    }
}

impl From<StoreError> for ToolResponse {
    fn from(error: StoreError) -> Self {
        Self::error(error.0)
    }
}

/// Store writes a tool commits together with its job record.
type Pending = Box<dyn FnOnce(&Writer) -> Result<(), StoreError>>;
type Handled = Result<(ToolResponse, Option<Pending>), ToolResponse>;

fn done(response: ToolResponse) -> Handled {
    Ok((response, None))
}

fn commit(
    response: ToolResponse,
    pending: impl FnOnce(&Writer) -> Result<(), StoreError> + 'static,
) -> Handled {
    Ok((response, Some(Box::new(pending))))
}

/// Rendered file and its record, as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenderRecord {
    render: Render,
    /// SHA-256 of the WAV, which names its object. A record imported with
    /// `missing_renders: without_audio` keeps the hash while the object is
    /// absent; `render_batch` `regenerate_renders` restores it.
    object: String,
}

impl RenderRecord {
    fn audio_present(&self, store: &Store) -> bool {
        store.object_path(&self.object).is_file()
    }
}

/// Refusal for a render whose audio is absent from this workspace.
fn absent_render_audio(project: &str, render: &str, location: &str) -> ToolResponse {
    ToolResponse::refused(
        Code::MissingRenderAudio,
        location,
        format!(
            "render `{render}` of `{project}` has no audio in this workspace: it was imported as a record without audio, keeping its recorded hash. Restore it with render_batch {{\"project\": \"{project}\", \"regenerate_renders\": [\"{render}\"]}}, which renders it again and checks the hash, or use another render"
        ),
    )
}

/// Number for the next `render-N` ID: one past the highest stored number,
/// which is the count plus one while the IDs are contiguous.
fn next_render_number(reader: &crate::store::Reader, project: &str) -> Result<u64, StoreError> {
    let records: Vec<RenderRecord> = reader.documents(project, "render")?;
    let highest = records
        .iter()
        .filter_map(|r| r.render.id.strip_prefix("render-")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    Ok(highest.max(records.len() as u64) + 1)
}

const PERCUSSION_CHANNEL: u8 = 9;
const DEFAULT_SAMPLE_RATE: u32 = 44_100;
const MAX_PAGE: u32 = 500;
const DRAFTS: &str = "draft";

pub struct Service {
    workspace: PathBuf,
    store: Option<Store>,
    review_base_url: Option<String>,
    /// Verified records of a bundle being imported, by (collection, id),
    /// readable before the import commits them.
    staged: std::collections::BTreeMap<(String, String), Value>,
}

type Canonicalize = fn(&Value) -> Option<Value>;

/// Parses a changing tool's arguments as the tool understands them, with
/// defaults filled in, so requests that mean the same thing get the same job hash.
fn canonicalizer(tool: &str) -> Option<Canonicalize> {
    fn typed<T: DeserializeOwned + Serialize>(arguments: &Value) -> Option<Value> {
        serde_json::from_value::<T>(arguments.clone())
            .ok()
            .and_then(|t| serde_json::to_value(t).ok())
    }
    Some(match tool {
        "prepare_sample" => typed::<PrepareSampleInput>,
        "export_sample" => typed::<ExportSampleInput>,
        "attach_observation" => typed::<AttachObservationInput>,
        "export_observation" => typed::<ExportObservationInput>,
        "load_fm_bank" => typed::<LoadFmBankInput>,
        "copy_fm_patch" => typed::<CopyFmPatchInput>,
        "export_fm_bank" => typed::<ExportFmBankInput>,
        "render_fm_patch" => typed::<RenderFmPatchInput>,
        "load_vab_bank" => typed::<LoadVabBankInput>,
        "replace_vab_sample" => typed::<ReplaceVabSampleInput>,
        "export_vab_bank" => typed::<ExportVabBankInput>,
        "render_vab_tone" => typed::<RenderVabToneInput>,
        "load_psx_sequence" => typed::<LoadPsxSequenceInput>,
        "export_psx_sequence" => typed::<ExportPsxSequenceInput>,
        "render_psx_sequence" => typed::<RenderPsxSequenceInput>,
        "render_psg" => typed::<RenderPsgInput>,
        "transform_vgm" => typed::<TransformVgmInput>,
        "render_adx" => typed::<RenderAdxInput>,
        "encode_adx" => typed::<EncodeAdxInput>,
        "export_nds_sample" => typed::<ExportNdsSampleInput>,
        "replace_nds_wave" => typed::<ReplaceNdsWaveInput>,
        "load_instrument_source" => typed::<LoadInstrumentSourceInput>,
        "export_instrument_source" => typed::<ExportInstrumentSourceInput>,
        "export_instrument_bank" => typed::<ExportInstrumentBankInput>,
        "load_instrument_bank" => typed::<LoadInstrumentBankInput>,
        "copy_instrument" => typed::<CopyInstrumentInput>,
        "render_instrument" => typed::<RenderInstrumentInput>,
        "prepare_target" => typed::<PrepareTargetInput>,
        "export_target" => typed::<ExportTargetInput>,
        "load_midi" => typed::<LoadMidiInput>,
        "export_midi_source" => typed::<ExportMidiSourceInput>,
        "create_project" => typed::<CreateProjectInput>,
        "import_midi" => typed::<ImportMidiInput>,
        "update_brief" => typed::<UpdateBriefInput>,
        "set_head" => typed::<SetHeadInput>,
        "edit_project" => typed::<EditProjectInput>,
        "render_preview" => typed::<RenderPreviewInput>,
        "render_batch" => typed::<RenderBatchInput>,
        "present_review" => typed::<PresentReviewInput>,
        "record_feedback" => typed::<RecordFeedbackInput>,
        "export_project" => typed::<ExportProjectInput>,
        "import_project" => typed::<ImportProjectInput>,
        "convert_for_target" => typed::<ConvertForTargetInput>,
        _ => return None,
    })
}

fn canonical_arguments(tool: &str, arguments: &Value) -> Value {
    canonicalizer(tool)
        .and_then(|canonicalize| canonicalize(arguments))
        .unwrap_or_else(|| arguments.clone())
}

fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, ToolResponse> {
    serde_json::from_value(arguments).map_err(|error| {
        ToolResponse::refused(
            Code::SchemaMismatch,
            "",
            format!("the arguments do not match the tool schema: {error}"),
        )
    })
}

impl Service {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            store: None,
            review_base_url: None,
            staged: Default::default(),
        }
    }
    /// A workspace-wide document (collection, id), including records an
    /// import has verified but not yet committed.
    fn shared_document<T: DeserializeOwned>(
        &mut self,
        collection: &str,
        id: &str,
    ) -> Result<Option<T>, ToolResponse> {
        if let Some(staged) = self.staged.get(&(collection.to_owned(), id.to_owned())) {
            return serde_json::from_value(staged.clone())
                .map(Some)
                .map_err(|e| ToolResponse::error(format!("staged {collection} {id}: {e}")));
        }
        Ok(self.store_ref()?.reader().document("", collection, id)?)
    }
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    /// Base URL of a running listening screen, such as `http://127.0.0.1:4100`.
    pub fn set_review_base_url(&mut self, url: Option<String>) {
        self.review_base_url = url;
    }
    pub fn call(&mut self, tool: &str, arguments: Value) -> ToolResponse {
        let arguments = if arguments.is_null() {
            json!({})
        } else {
            arguments
        };
        if let Some(response) = self.route(tool, &arguments) {
            return response;
        }
        let request_hash = sha256_hex(canonical_arguments(tool, &arguments).to_string().as_bytes());
        // Jobs recorded before a tool gained typed canonicalization hashed raw arguments.
        let raw_hash = sha256_hex(arguments.to_string().as_bytes());
        let job_id = arguments
            .get("job_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                (tool == "edit_project")
                    .then(|| {
                        arguments
                            .pointer("/request/job_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .flatten()
            });
        let read_only = tool_definitions()
            .iter()
            .find(|d| d.name == tool)
            .map(|d| d.read_only);
        let Some(read_only) = read_only else {
            return ToolResponse::refused(
                Code::UnknownReference,
                "",
                format!("unknown tool `{tool}`"),
            );
        };
        if !read_only {
            let Some(job_id) = job_id.clone() else {
                return ToolResponse::refused(
                    Code::SchemaMismatch,
                    if tool == "edit_project" {
                        "/request/job_id"
                    } else {
                        "/job_id"
                    },
                    "tools that change work need a job_id",
                );
            };
            if let Some(mut replay) = self.replay(tool, &job_id, &[&request_hash, &raw_hash]) {
                if tool == "present_review"
                    && replay.status == Status::Ok
                    && let Some(owner) = arguments
                        .get("project")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            arguments
                                .get("source")
                                .and_then(Value::as_str)
                                .map(source_review::source_owner)
                        })
                {
                    self.refresh_review_url(&mut replay, &owner);
                }
                return replay;
            }
        }
        let (mut response, pending) = match self.dispatch(tool, arguments) {
            Ok(handled) => handled,
            Err(response) => (response, None),
        };
        let Some(job_id) = job_id.filter(|_| !read_only) else {
            return response;
        };
        response.job_id = Some(job_id.clone());
        if response.status == Status::Error {
            return response;
        }
        // Blocked outcomes are recorded too, so a retry gets the same answer.
        let record = serde_json::to_value(&response).expect("responses serialize");
        let committed = self.store_mut(true).and_then(|store| {
            store.write(|writer| {
                if writer.job(&job_id)?.is_some() {
                    return Ok(());
                }
                if let Some(pending) = pending {
                    pending(writer)?;
                }
                writer.record_job(&job_id, tool, &request_hash, &record)
            })
        });
        match committed {
            Ok(()) => response,
            Err(error) => ToolResponse {
                job_id: Some(job_id),
                ..ToolResponse::error(error.0)
            },
        }
    }
    fn replay(
        &mut self,
        tool: &str,
        job_id: &str,
        request_hashes: &[&str],
    ) -> Option<ToolResponse> {
        let store = self.store_mut(false).ok()?;
        let record = store.job(job_id).ok()??;
        if record.tool != tool || !request_hashes.contains(&record.request_hash.as_str()) {
            return Some(ToolResponse {
                job_id: Some(job_id.to_owned()),
                ..ToolResponse::refused(
                    Code::JobConflict,
                    "/job_id",
                    format!(
                        "job `{job_id}` already ran `{}` with different arguments; use a new job_id",
                        record.tool
                    ),
                )
            });
        }
        let mut response: ToolResponse = serde_json::from_value(record.response).ok()?;
        response.replayed = true;
        Some(response)
    }
    fn store_mut(&mut self, create: bool) -> Result<&mut Store, StoreError> {
        if self.store.is_none() {
            self.store = Some(Store::open(&self.workspace, create)?);
        }
        Ok(self.store.as_mut().expect("opened above"))
    }
    fn store_ref(&mut self) -> Result<&mut Store, ToolResponse> {
        self.store_mut(false).map_err(|_| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/project",
                "no projects exist in this workspace yet",
            )
        })
    }
    fn dispatch(&mut self, tool: &str, arguments: Value) -> Handled {
        match tool {
            "get_capabilities" => capabilities(parse(arguments)?),
            "prepare_sample" => self.prepare_sample(parse(arguments)?),
            "inspect_sample" => self.inspect_sample(parse(arguments)?),
            "export_sample" => self.export_sample(parse(arguments)?),
            "attach_observation" => self.attach_observation(parse(arguments)?),
            "inspect_observation" => self.inspect_observation(parse(arguments)?),
            "export_observation" => self.export_observation(parse(arguments)?),
            "load_fm_bank" => self.load_fm_bank(parse(arguments)?),
            "inspect_fm_bank" => self.inspect_fm_bank(parse(arguments)?),
            "copy_fm_patch" => self.copy_fm_patch(parse(arguments)?),
            "export_fm_bank" => self.export_fm_bank(parse(arguments)?),
            "render_fm_patch" => self.render_fm_patch(parse(arguments)?),
            "load_vab_bank" => self.load_vab_bank(parse(arguments)?),
            "inspect_vab_bank" => self.inspect_vab_bank(parse(arguments)?),
            "replace_vab_sample" => self.replace_vab_sample(parse(arguments)?),
            "export_vab_bank" => self.export_vab_bank(parse(arguments)?),
            "render_vab_tone" => self.render_vab_tone(parse(arguments)?),
            "load_psx_sequence" => self.load_psx_sequence(parse(arguments)?),
            "inspect_psx_sequence" => self.inspect_psx_sequence(parse(arguments)?),
            "export_psx_sequence" => self.export_psx_sequence(parse(arguments)?),
            "render_psx_sequence" => self.render_psx_sequence(parse(arguments)?),
            "inspect_vgm" => self.inspect_vgm(parse(arguments)?),
            "render_psg" => self.render_psg(parse(arguments)?),
            "transform_vgm" => self.transform_vgm(parse(arguments)?),
            "inspect_adx" => self.inspect_adx(parse(arguments)?),
            "render_adx" => self.render_adx(parse(arguments)?),
            "encode_adx" => self.encode_adx(parse(arguments)?),
            "rank_adx_loops" => self.rank_adx_loops(parse(arguments)?),
            "inspect_nds_sound" => self.inspect_nds_sound(parse(arguments)?),
            "export_nds_sample" => self.export_nds_sample(parse(arguments)?),
            "replace_nds_wave" => self.replace_nds_wave(parse(arguments)?),
            "load_instrument_source" => self.load_instrument_source(parse(arguments)?),
            "inspect_instrument_source" => self.inspect_instrument_source(parse(arguments)?),
            "export_instrument_source" => self.export_instrument_source(parse(arguments)?),
            "export_instrument_bank" => self.export_instrument_bank(parse(arguments)?),
            "load_instrument_bank" => self.load_instrument_bank(parse(arguments)?),
            "inspect_instrument_bank" => self.inspect_instrument_bank(parse(arguments)?),
            "copy_instrument" => self.copy_instrument(parse(arguments)?),
            "render_instrument" => self.render_instrument(parse(arguments)?),
            "prepare_target" => self.prepare_target(parse(arguments)?),
            "inspect_target" => self.inspect_target(parse(arguments)?),
            "export_target" => self.export_target(parse(arguments)?),
            "load_midi" => self.load_midi(parse(arguments)?),
            "inspect_midi" => self.inspect_midi(parse(arguments)?),
            "export_midi_source" => self.export_midi_source(parse(arguments)?),
            "create_project" => self.create_project(parse(arguments)?),
            "compare_midi" => self.compare_midi(parse(arguments)?),
            "analyze_midi" => self.analyze_midi(parse(arguments)?),
            "import_midi" => self.import_midi(parse(arguments)?),
            "update_brief" => self.update_brief(parse(arguments)?),
            "set_head" => self.set_head(parse(arguments)?),
            "inspect_project" => self.inspect(parse(arguments)?).map(|r| (r, None)),
            "edit_project" => self.edit_project(parse(arguments)?),
            "check_project" => self.check_project(parse(arguments)?).map(|r| (r, None)),
            "compare_revisions" => self.compare(parse(arguments)?).map(|r| (r, None)),
            "render_preview" => self.render_preview(parse(arguments)?),
            "render_batch" => self.render_batch(parse(arguments)?),
            "measure_audio" => self.measure_audio(parse(arguments)?),
            "compare_audio" => self.compare_audio(parse(arguments)?),
            "present_review" => self.present_review(parse(arguments)?),
            "read_feedback" => self.read_feedback(parse(arguments)?).map(|r| (r, None)),
            "record_feedback" => self.record_feedback(parse(arguments)?),
            "export_project" => self.export_project(parse(arguments)?),
            "import_project" => self.import_project(parse(arguments)?),
            "convert_for_target" => self.convert_for_target(parse(arguments)?),
            _ => unreachable!("checked against tool_definitions"),
        }
    }
    fn project(&mut self, project: &str) -> Result<(ProjectInfo, Value), ToolResponse> {
        let store = self.store_ref()?;
        let (mut info, settings) = store.reader().project(project)?.ok_or_else(|| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/project",
                format!("project `{project}` does not exist"),
            )
        })?;
        // Earlier imports stored the original as an object, before bundles
        // carried it. Recover that evidence instead of normalizing old work.
        if info.midi_source.is_none()
            && let Some(hash) = settings.pointer("/source/sha256").and_then(Value::as_str)
        {
            let bytes = std::fs::read(store.object_path(hash)).map_err(|e| {
                ToolResponse::error(format!("the original MIDI object is unavailable: {e}"))
            })?;
            if sha256_hex(&bytes) != hash {
                return Err(ToolResponse::refused(
                    Code::UnsupportedMidi,
                    "/midi_source",
                    "the original MIDI object does not match its hash",
                ));
            }
            info.midi_source = Some(crate::midi::MidiSource {
                sha256: hash.into(),
                bytes,
                declared_ends: None,
                legacy_declared_ends: Some(false),
            });
        }
        if let Some(source) = &mut info.midi_source
            && source.declared_ends.is_some()
            && source.legacy_declared_ends.is_none()
            && let Some(root) = store
                .reader()
                .revisions(project)?
                .into_iter()
                .find(|r| r.parent.is_none())
        {
            crate::midi::resolve_declared_projection(source, &root.composition);
        }
        Ok((info, settings))
    }

    fn revision(
        &mut self,
        project: &str,
        revision: Option<&str>,
    ) -> Result<(ProjectInfo, Revision), ToolResponse> {
        let (info, _) = self.project(project)?;
        let id = revision.unwrap_or(&info.head_revision).to_owned();
        let store = self.store_ref()?;
        let revision = store.reader().revision(project, &id)?.ok_or_else(|| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/revision",
                format!("revision `{id}` does not exist"),
            )
        })?;
        Ok((info, revision))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_changing_tool_has_typed_job_canonicalization() {
        let missing: Vec<_> = tool_definitions()
            .iter()
            .filter(|d| !d.read_only && canonicalizer(d.name).is_none())
            .map(|d| d.name)
            .collect();
        assert!(missing.is_empty(), "{missing:?}");
    }
}
