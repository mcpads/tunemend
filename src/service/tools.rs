//! Tool names, descriptions and input schemas.

use super::*;

pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub input_schema: Schema,
}

fn definition<T: JsonSchema>(
    name: &'static str,
    read_only: bool,
    description: &'static str,
) -> ToolDefinition {
    ToolDefinition {
        name,
        description,
        read_only,
        input_schema: schemars::schema_for!(T),
    }
}

/// Every tool with its input schema, in the order an agent usually needs them.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        definition::<CapabilitiesInput>(
            "get_capabilities",
            true,
            "Discover supported operations and limits. Start with the summary, then request the sections needed for the task.",
        ),
        definition::<LoadMidiInput>(
            "load_midi",
            false,
            "Retain exact MIDI bytes and events, including embedded streams. Loading is independent of editing and playback support.",
        ),
        definition::<InspectMidiInput>(
            "inspect_midi",
            true,
            "List retained MIDI sources or inspect headers, ordered events and raw bytes. Follow next_cursor as offset.",
        ),
        definition::<ExportMidiSourceInput>(
            "export_midi_source",
            false,
            "Export retained MIDI bytes to a new file and verify exact readback.",
        ),
        definition::<CreateProjectInput>(
            "create_project",
            false,
            "Create a music or sound-effect project. Record requirements and assumptions separately in its brief.",
        ),
        definition::<CompareMidiInput>(
            "compare_midi",
            true,
            "Compare byte identity and decoded MIDI events, including timing, ordering and opaque data.",
        ),
        definition::<AnalyzeMidiInput>(
            "analyze_midi",
            true,
            "Inspect a MIDI file without storing it: identity, editable projection and playback limits.",
        ),
        definition::<ImportMidiInput>(
            "import_midi",
            false,
            "Create an editable project from retained MIDI or a file. Preserves original bytes; unsupported edits are refused. Use load_midi first for exact retention.",
        ),
        definition::<UpdateBriefInput>(
            "update_brief",
            false,
            "Replace the project brief: purpose, requirements and assumptions.",
        ),
        definition::<SetHeadInput>(
            "set_head",
            false,
            "Resume an existing revision without deleting history. Later edits and exports use this head.",
        ),
        definition::<InspectInput>(
            "inspect_project",
            true,
            "Inspect projects, revision summaries, paged events, history or reviews. Event pagination uses next_cursor as cursor.",
        ),
        definition::<EditProjectInput>(
            "edit_project",
            false,
            "Apply atomic edits within scope and protections: notes, timeline, mixer, effects, instruments and clips. Every successful edit becomes the head, including branches. Uses request.job_id.",
        ),
        definition::<CheckProjectInput>(
            "check_project",
            true,
            "Check declared constraints and report located violations and unchecked items. Profiles describe limits, not device playback.",
        ),
        definition::<CompareInput>(
            "compare_revisions",
            true,
            "Compare notes, controls, timeline, mixer and clips. filter_applies_to identifies which differences a range or voice filter narrows.",
        ),
        definition::<RenderPreviewInput>(
            "render_preview",
            false,
            "Render a revision, excerpt or voice selection to WAV through its instruments, clips, mixer and effects.",
        ),
        definition::<RenderBatchInput>(
            "render_batch",
            false,
            "Render mixdowns and stems by span or section. Read sum_check for summation limits; tail=cut removes decay tails. regenerate_renders restores missing audio with hash checks.",
        ),
        definition::<MeasureAudioInput>(
            "measure_audio",
            true,
            "Measure a render or WAV: peaks, loudness, silence and spectrum. Optional pitch tracks a monophonic fundamental, not individual notes in chords.",
        ),
        definition::<CompareAudioInput>(
            "compare_audio",
            true,
            "Compare same-rate, same-channel renders or WAVs: levels, alignment, residual and optional pitch. Separate input ranges allow loop-pass comparisons.",
        ),
        definition::<PresentReviewInput>(
            "present_review",
            false,
            "Present renders on the listening screen. MCP starts it automatically; keep the connection open. An identical retry after reconnect returns the same presentation with its live URL.",
        ),
        definition::<ReadFeedbackInput>(
            "read_feedback",
            true,
            "Read delivered marks with their original revision, loop pass and sounding notes. Continue with next_cursor as after_sequence.",
        ),
        definition::<RecordFeedbackInput>(
            "record_feedback",
            false,
            "Record the person’s conversational feedback verbatim, optionally with marks on a presentation.",
        ),
        definition::<ExportProjectInput>(
            "export_project",
            false,
            "Deliver MIDI, an editable bundle, reports and selected WAVs to a new folder. render_records=delivered keeps selected render records; named_copies adds readable filenames. Review history must retain its referenced renders.",
        ),
        definition::<PrepareSampleInput>(
            "prepare_sample",
            false,
            "Prepare BRR, PCM, WAV or PSX ADPCM/VAG conversion. Reports changes, loop fit and seam diagnostics before export; original bytes stay retained.",
        ),
        definition::<InspectSampleInput>(
            "inspect_sample",
            true,
            "Inspect a retained conversion plan and the change kinds required for export. Start with the summary; detail=full includes the complete report and identities.",
        ),
        definition::<ExportSampleInput>(
            "export_sample",
            false,
            "Export the prepared sample, originals, previews and report. Accept exactly the inspected change kinds compatible with the request; this is not a human approval gate.",
        ),
        definition::<AttachObservationInput>(
            "attach_observation",
            false,
            "Retain an external capture folder with a portable receipt and subject links. Refuses identity mismatches; remaining limitations need explicit acceptance.",
        ),
        definition::<InspectObservationInput>(
            "inspect_observation",
            true,
            "List observations or inspect receipts, limitations and retained files. Verifies stored identities independently of the original folder.",
        ),
        definition::<ExportObservationInput>(
            "export_observation",
            false,
            "Export exact retained capture files and their receipt with readback checks.",
        ),
        definition::<LoadFmBankInput>(
            "load_fm_bank",
            false,
            "Retain an OPN FM container using a verified record layout. inspect_fm_bank view=layouts describes supported packing.",
        ),
        definition::<InspectFmBankInput>(
            "inspect_fm_bank",
            true,
            "Inspect FM layouts, banks and paged raw or decoded patches. Static registers do not establish driver behavior or audio equality.",
        ),
        definition::<CopyFmPatchInput>(
            "copy_fm_patch",
            false,
            "Copy static OPN patch parameters into an explicit recipient, preserving unselected bits. Replacement affects every reference to that record.",
        ),
        definition::<ExportFmBankInput>(
            "export_fm_bank",
            false,
            "Export the complete FM container, decoded patches and provenance with byte readback.",
        ),
        definition::<RenderFmPatchInput>(
            "render_fm_patch",
            false,
            "Audition an OPN patch with declared chip, clock and playback context. Returns WAVs and register writes; unsupported chip features are reported or refused.",
        ),
        definition::<LoadInstrumentSourceInput>(
            "load_instrument_source",
            false,
            "Retain a sound file exactly and discover structural Compile modules when present. Inspection limits do not block retention.",
        ),
        definition::<InspectInstrumentSourceInput>(
            "inspect_instrument_source",
            true,
            "Inspect retained sources, module layouts and paged bytes. WAV summaries include rate, channels, frames, loops and root note.",
        ),
        definition::<ExportInstrumentSourceInput>(
            "export_instrument_source",
            false,
            "Export the exact retained sound source independently of interpretation.",
        ),
        definition::<InspectVgmInput>(
            "inspect_vgm",
            true,
            "Inspect retained VGM headers, commands, chip/channel usage and loops. Reports other chips and malformed offsets. Decompress VGZ first.",
        ),
        definition::<RenderPsgInput>(
            "render_psg",
            false,
            "Render VGM or explicit writes/notes on supported PSG-family models. Returns WAVs and applied/skipped commands; capability sections define each chip’s limits.",
        ),
        definition::<TransformVgmInput>(
            "transform_vgm",
            false,
            "Derive a VGM by same-family edits or SN76489-to-AY mapping. Preserves timing and loop; accept_losses must name each reported loss compatible with the request.",
        ),
        definition::<ExportInstrumentBankInput>(
            "export_instrument_bank",
            false,
            "Export a native bank and its module context with exact readback. Game insertion and playback remain consumer operations.",
        ),
        definition::<LoadInstrumentBankInput>(
            "load_instrument_bank",
            false,
            "Retain native bank modules with an explicit layout. Unsupported sample interpretation does not discard supplied bytes.",
        ),
        definition::<InspectInstrumentBankInput>(
            "inspect_instrument_bank",
            true,
            "Inspect samples, loops, tuning, track bindings, references and capacity before copying or allocating native data.",
        ),
        definition::<CopyInstrumentInput>(
            "copy_instrument",
            false,
            "Copy exact BRR and referenced settings into a new bank. Explicit allocation must respect references and reserved slots; receiving transpose and mix are preserved by default.",
        ),
        definition::<RenderInstrumentInput>(
            "render_instrument",
            false,
            "Audition one S-DSP voice from a BRR plan or bank slot. Returns a WAV and measure_with input, not a project render; driver programs and multi-voice DSP behavior are excluded.",
        ),
        definition::<PrepareTargetInput>(
            "prepare_target",
            false,
            "Prepare a Compile score using explicit mappings, a supplied encoder and assets. Accounts for events and changes and verifies score readback before export.",
        ),
        definition::<InspectTargetInput>(
            "inspect_target",
            true,
            "Inspect a prepared score, paged event mappings and reported changes before selecting them for export.",
        ),
        definition::<ExportTargetInput>(
            "export_target",
            false,
            "Export the prepared score, source and assets after selecting every reported change kind. Verifies hashes and readback; does not establish game playback.",
        ),
        definition::<ConvertForTargetInput>(
            "convert_for_target",
            false,
            "Optional Retro Music Converter handoff for newly composed revisions. Requires explicit mappings; refuses imported MIDI and known losses.",
        ),
        definition::<ImportProjectInput>(
            "import_project",
            false,
            "Resume a .project.json delivery with verified objects and history. Missing WAVs refuse by default; missing_renders selects regeneration or records without audio.",
        ),
    ]
}

/// How much of the capability description to return.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDetail {
    /// Version, timebases, limits and one line per section.
    #[default]
    Summary,
    /// Every section in full.
    Full,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesInput {
    /// Sections to return in full, by the names the summary lists, such as
    /// `mixer`, `render_batch`, `psg` or `measurement`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<String>>,
    /// `summary` (default) lists every section with one line; `full` returns
    /// every section in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<CapabilityDetail>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateProjectInput {
    pub job_id: String,
    /// Project id: lowercase letters, digits and hyphens.
    pub project: String,
    pub timebase: Timebase,
    #[serde(default)]
    pub brief: Option<ProjectBrief>,
    /// Initial content. Without it, music starts as four empty 4/4 bars at
    /// 120 BPM and an effect as 0.5 s of silence.
    #[serde(default)]
    pub composition: Option<Composition>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeMidiInput {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportMidiInput {
    /// Optional inspected source hash; refuses import if the file has changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    pub job_id: String,
    pub project: String,
    /// Supply a file path or a loaded source ID, exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub brief: Option<ProjectBrief>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetHeadInput {
    pub job_id: String,
    pub project: String,
    pub revision: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateBriefInput {
    pub job_id: String,
    pub project: String,
    pub brief: ProjectBrief,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InspectView {
    Projects,
    Summary,
    Events,
    History,
    Reviews,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectInput {
    pub view: InspectView,
    #[serde(default)]
    pub project: Option<String>,
    /// Defaults to the project's head revision.
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub range: Option<TimeRange>,
    #[serde(default)]
    pub voices: Option<Vec<String>>,
    /// `next_cursor` from the previous page of events.
    #[serde(default)]
    pub cursor: Option<u64>,
    /// Events per page, at most 500; default 200.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditProjectInput {
    pub project: String,
    /// `request.job_id` identifies the job.
    pub request: EditRequest,
    /// Allow editing a revision other than the head; the result becomes the head.
    #[serde(default)]
    pub branch: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ProfileChoice {
    /// A built-in profile name from `get_capabilities`.
    Named(String),
    Inline(ConstraintProfile),
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckProjectInput {
    pub project: String,
    #[serde(default)]
    pub revision: Option<String>,
    pub profile: ProfileChoice,
    /// Replaces the profile's `reserved_for_effects`: notes kept free for
    /// sound effects, such as 2 with `snes-dsp-voices` to leave 6 S-DSP
    /// voices for the music. Applies to music timebases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_for_effects: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareInput {
    pub project: String,
    pub from: String,
    pub to: String,
    /// Only notes, controls and clips touching this range; voices, the
    /// timeline and the mixer are compared whole.
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// Only notes, controls, clips and mixer items of these voices; added and
    /// removed voices, the timeline, buses and the master are compared whole.
    /// The response's `filter_applies_to` lists the filtered categories.
    #[serde(default)]
    pub voices: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderPreviewInput {
    pub job_id: String,
    pub project: String,
    #[serde(default)]
    pub revision: Option<String>,
    pub layout: RenderLayout,
    #[serde(default)]
    pub voices: Option<Vec<String>>,
    /// Default 44100.
    #[serde(default)]
    pub sample_rate: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresentReviewInput {
    pub job_id: String,
    pub project: String,
    /// Shown to the person, such as `Village BGM · Revised`.
    #[serde(default)]
    pub title: Option<String>,
    pub renders: Vec<PresentedRender>,
    /// The presentation you expect to be the latest; blocks if another was
    /// shown meanwhile.
    #[serde(default)]
    pub replaces: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFeedbackInput {
    pub project: String,
    /// Only feedback delivered after this sequence; use `next_cursor`.
    #[serde(default)]
    pub after_sequence: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordFeedbackInput {
    pub job_id: String,
    pub project: String,
    #[serde(default)]
    pub presentation: Option<String>,
    #[serde(default)]
    pub marks: Vec<Mark>,
    /// The person's words, verbatim.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConvertForTargetInput {
    pub job_id: String,
    pub project: String,
    #[serde(default)]
    pub revision: Option<String>,
    /// The Retro Music Converter executable (`retro-music-converter`),
    /// relative to the workspace or absolute.
    pub converter: String,
    pub mapping: crate::target::TargetMapping,
    /// Converter capability profile of the target driver, relative to the workspace.
    #[serde(default)]
    pub capability_profile: Option<String>,
    /// New folder under `out/` or `work/` in the workspace.
    pub destination: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportProjectInput {
    pub job_id: String,
    /// Path of the `.project.json` file, relative to the workspace or absolute
    /// (it may lie outside the workspace; it is only read). Render WAVs
    /// are read from the `renders/` folder next to it, and referenced data
    /// from `objects/`.
    pub path: String,
    /// What to do when `renders/` lacks a render's WAV; default `refuse`.
    #[serde(default, skip_serializing_if = "MissingRenders::is_refuse")]
    pub missing_renders: MissingRenders,
    /// Render IDs to render again when their WAV is missing, checked against
    /// the recorded hash; renders whose WAV is present are read from it. The
    /// other missing renders follow `missing_renders` (`refuse` or
    /// `without_audio`). Use it to restore only the renders you will use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regenerate_renders: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportProjectInput {
    pub job_id: String,
    pub project: String,
    #[serde(default)]
    pub revision: Option<String>,
    /// New or empty folder relative to the workspace, such as `out/village`.
    pub destination: String,
    /// Track and channel per voice; defaults to imported assignments, then
    /// one channel per voice in order, skipping channel 10.
    #[serde(default)]
    pub assignments: Option<Vec<VoiceAssignment>>,
    /// Effects only; default PPQ 1000 at 1,000,000 µs per quarter, a 1 ms grid.
    #[serde(default)]
    pub timing: Option<MidiTiming>,
    /// Which render WAVs go into `renders/`: `all` (default), `presented` (those
    /// in a presentation), `listed` (exactly `render_ids`) or `none`. Every
    /// render's record stays in the bundle unless `render_records` is
    /// `delivered`; import_project with `missing_renders: regenerate` (or
    /// `regenerate_renders`) recreates omitted audio.
    #[serde(default, skip_serializing_if = "RenderAudio::is_all")]
    pub render_audio: RenderAudio,
    /// Render IDs whose WAVs go into `renders/`: exactly these with
    /// `render_audio: listed`, or in addition to `presented` or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_ids: Option<Vec<String>>,
    /// Which render records go into the bundle: `all` (default), so an import
    /// can regenerate omitted audio, or `delivered`: only the renders whose
    /// WAVs are written. `delivered` is refused while a presentation or a
    /// feedback mark refers to a render that is not delivered.
    #[serde(default, skip_serializing_if = "RenderRecords::is_all")]
    pub render_records: RenderRecords,
    /// Whether the bytes of referenced data (clip sources, instrument and FM
    /// bank containers, sample plan files) are written to `objects/`:
    /// `include` (default) or `omit`. Their records always stay in the bundle.
    #[serde(default, skip_serializing_if = "ReferencedData::is_include")]
    pub referenced_data: ReferencedData,
    /// Also write each section's id as a marker meta event at its start.
    /// MIDI has no region event, so section ends stay in the bundle only.
    /// Refused for source-backed projects, whose markers are original bytes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sections_as_markers: bool,
    /// Add byte-identical copies under `mix/<span>.wav` or
    /// `stems/<span>/<part>.wav`. Names include voice filters and pre-master
    /// state; collisions add `-<render id>`. The report maps each copy to its
    /// render. Canonical `renders/<id>.wav` files remain for import.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named_copies: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReferencedData {
    #[default]
    Include,
    /// Leave the bytes out; an import needs them retained in its workspace.
    Omit,
}

impl ReferencedData {
    fn is_include(&self) -> bool {
        *self == Self::Include
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RenderRecords {
    /// Every render's record, with or without its WAV.
    #[default]
    All,
    /// Only the records of renders whose WAVs are delivered.
    Delivered,
}

impl RenderRecords {
    fn is_all(&self) -> bool {
        *self == Self::All
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RenderAudio {
    #[default]
    All,
    Presented,
    None,
    /// Exactly the renders in `render_ids`.
    Listed,
}

impl RenderAudio {
    fn is_all(&self) -> bool {
        *self == Self::All
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissingRenders {
    /// Refuse a bundle whose `renders/` folder lacks a listed render.
    #[default]
    Refuse,
    /// Render missing audio again from its revision and settings; a recorded
    /// hash must match, otherwise the length must.
    Regenerate,
    /// Import missing renders as records without audio, keeping their
    /// recorded hash. Tools that need the audio refuse them with
    /// `missing_render_audio` until `render_batch` `regenerate_renders`
    /// restores it. Needs a recorded hash; presented renders need their audio.
    WithoutAudio,
}

impl MissingRenders {
    fn is_refuse(&self) -> bool {
        *self == Self::Refuse
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareMidiInput {
    pub reference: String,
    pub candidate: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadMidiInput {
    pub job_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MidiSourceView {
    Sources,
    #[default]
    Summary,
    Events,
    Bytes,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectMidiInput {
    #[serde(default)]
    pub view: MidiSourceView,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub track: Option<u32>,
    /// Event cursor in events view; byte offset in bytes view.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Events default 100, maximum 500. Bytes default 256, maximum 4096.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportMidiSourceInput {
    pub job_id: String,
    pub source: String,
    /// New file path relative to the workspace.
    pub destination: String,
}
