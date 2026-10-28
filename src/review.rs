//! Renders that people listen to, what they were shown, and what they marked.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A source-only render kept for listening. It belongs to one retained MIDI
/// source and its playback settings, never to a project, revision or voice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceRender {
    /// `source-render-N`, numbered per source.
    pub id: String,
    /// SHA-256 of the retained MIDI source.
    pub source: String,
    /// The SMF type 2 sequence (track index) that played.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u32>,
    pub layout: RenderLayout,
    pub sound_source: String,
    pub sample_rate: u32,
    /// Samples of the selected range; the rest is the decay tail.
    pub content_samples: u64,
    pub length_samples: u64,
    pub tail_truncated: bool,
    pub format: WavFormat,
    /// MIDI ports of the receivers that played, ascending.
    pub ports: Vec<u8>,
    /// Output gain applied to the rendered samples; omitted means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<crate::mix::Tenths>,
    /// Silent samples before the rendered content, included in
    /// `length_samples`; omitted means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_in_samples: Option<u64>,
    /// SHA-256 of the WAV file.
    pub sha256: String,
}

/// A rendered PCM file of one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Render {
    pub id: String,
    pub revision: String,
    pub layout: RenderLayout,
    /// Voices included; omitted means all voices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voices: Option<Vec<String>>,
    /// Name of the synthesizer and sound source used for voices that
    /// `played_by` does not list.
    pub sound_source: String,
    /// Sound that played instead of, or besides, `sound_source`: chip
    /// instruments of heard voices and audio clips that wrote samples.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub played_by: Vec<PlayedSource>,
    pub sample_rate: u32,
    /// Length of the PCM data, including the decay tail after the content.
    pub length_samples: u64,
    /// The decay was still audible when the renderer cut the tail.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tail_truncated: bool,
    /// SHA-256 of the WAV, so a regenerated render can be checked exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The section whose bounds gave this excerpt, for region renders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The part of the mix this render holds: a stem, or the mixdown its
    /// stems sum to. Absent for an ordinary render of the whole mix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<RenderPart>,
    /// The stem stops before the master volume and master chain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pre_master: bool,
    /// Sample encoding of the WAV; 16-bit PCM when absent.
    #[serde(default, skip_serializing_if = "WavFormat::is_pcm16")]
    pub format: WavFormat,
    /// The render ends where its content ends (`render_batch` `tail: cut`):
    /// no decay tail after the excerpt, section or last loop pass.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tail_cut: bool,
}

/// A part of the mix rendered by `render_batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RenderPart {
    /// The whole mix, rendered with one synthesizer per voice like its stems.
    Mixdown,
    /// Everything one voice contributes: its own chain, fader and sends,
    /// through every bus and master stage it reaches.
    Voice { id: String },
    /// One bus's output through the master stage.
    Bus { id: String },
    /// The voices routed straight to the master, through the master stage;
    /// with the bus stems it completes the mix.
    Direct,
    /// The sum of the voice stems of a declared group of voices.
    Group { id: String, voices: Vec<String> },
}

/// Sample encoding of a render WAV.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WavFormat {
    /// 16-bit integer PCM: `round(clamp(x, -1, 1) × 32767)`.
    #[default]
    Pcm16,
    /// 24-bit integer PCM: `round(clamp(x, -1, 1) × 8388607)`.
    Pcm24,
    /// 32-bit IEEE float: the renderer's samples, unchanged and unclamped.
    Float32,
}

impl WavFormat {
    pub fn is_pcm16(&self) -> bool {
        *self == Self::Pcm16
    }
}

/// One voice's chip instrument or one audio clip heard in a render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayedSource {
    pub voice: String,
    /// `snes_brr`, `fm` or `audio_clip`.
    pub kind: String,
    /// The bank, plan or FM bank id, or the clip's sound-source SHA-256;
    /// absent for explicit FM registers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The clip id, for `audio_clip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RenderLayout {
    /// The intro once, then the loop region `loop_passes` times. Without a
    /// loop, `loop_passes` must be 1 and the whole content plays once.
    Full { loop_passes: u32 },
    /// The source range `[start, end)` once, in timebase units.
    Excerpt { start: u64, end: u64 },
}

/// The fixed set of renders shown to a person at one time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Presentation {
    pub id: String,
    /// Shown to the person, such as the cue name and which version it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub renders: Vec<PresentedRender>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresentedRender {
    pub render: String,
    /// Plain words for the person, such as `Revised` or `Previous version`.
    pub label: String,
}

/// A person's observation about what they heard. It never changes music data,
/// permissions or approval by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Feedback {
    pub id: String,
    /// What the person was listening to; absent for remarks made in
    /// conversation without a presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<String>,
    pub state: FeedbackState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<Mark>,
    /// The person's own words, kept verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FeedbackState {
    /// Saved while the person is still marking.
    Draft,
    /// Frozen and handed to the agent; `sequence` orders deliveries.
    Delivered { sequence: u64 },
}

/// A point (`end_sample` omitted) or range in one render's PCM samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    pub render: String,
    pub start_sample: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_sample: Option<u64>,
    /// Voice the person singled out, when they knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Name the screen shows next to the mark, such as `B`. It never changes
    /// when other marks are removed, so the person can refer to it in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
