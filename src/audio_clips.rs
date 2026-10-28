//! Audio clips: retained WAV sources placed on a voice's timeline.
//!
//! A clip is mixer data of its voice (`composition.mix.clips`). Its span
//! `[start, start + duration)` is in the project's timebase unit, like a note,
//! so scope, protections, loops and the playback end treat it like one. Its
//! audio is a range of a retained RIFF/WAVE sound source, chosen and mapped to
//! stereo by `channels`, resampled to the render rate when the rates differ,
//! scaled by `gain_db` and shaped by linear fades. Renders add it to the
//! voice's dry signal before the voice's effect chain and fader.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::composition::{Composition, Timebase};
use crate::diagnostic::{Diagnostic, DiagnosticCode as Code, Report};
use crate::edit::{EditOperation, EditRequest, check_change};
use crate::mix::{MAX_GAIN_DB, MIN_GAIN_DB, Tenths};
use crate::render::sample_at;
use crate::review::RenderLayout;
use crate::timeline::{Rational, seconds_at};

/// Longest fade-in or fade-out, in milliseconds.
pub const MAX_FADE_MS: f64 = 60_000.0;
/// Largest retained WAV a clip may use.
pub const MAX_SOURCE_BYTES: usize = 64 << 20;
/// Most clips one revision holds.
pub const MAX_CLIPS: usize = 1024;

/// WAV amplitude interpretation, retained in the revision for reproducible renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClipNormalization {
    /// Preserve historical PCM16 code scaling (divide by 32767).
    #[default]
    Pcm16Codes,
    /// Use normalized WAV amplitude (divide by 32768); suitable for float output.
    WavAmplitude,
}

impl ClipNormalization {
    fn is_legacy(&self) -> bool {
        *self == Self::Pcm16Codes
    }

    fn divisor(self) -> f64 {
        match self {
            Self::Pcm16Codes => 32767.0,
            Self::WavAmplitude => 32768.0,
        }
    }
}

/// A range of a retained WAV placed on a voice's timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioClip {
    /// Unique among the revision's clips.
    pub id: String,
    /// The voice whose mixer strip, effects, sends and output the clip uses.
    pub voice: String,
    /// SHA-256 of a RIFF/WAVE file retained by load_instrument_source.
    pub source: String,
    /// Timeline position of the clip's first sample, in the timebase unit.
    pub start: u64,
    /// Timeline length in the timebase unit. Audio stops at the span end,
    /// or earlier, followed by silence, when the source range runs out.
    pub duration: u64,
    /// First source frame played (trim in), 0-based at the source rate.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub source_start: u64,
    /// Frame after the last one played (trim out); absent means the file end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_end: Option<u64>,
    /// How source channels become the stereo clip signal.
    pub channels: ClipChannels,
    /// Source amplitude convention. Omission preserves historical PCM16 code scaling.
    #[serde(default, skip_serializing_if = "ClipNormalization::is_legacy")]
    pub normalization: ClipNormalization,
    /// Clip gain in dB, -120.0–12.0.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub gain_db: Tenths,
    /// Linear fade-in length in milliseconds, 0.0–60000.0.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub fade_in_ms: Tenths,
    /// Linear fade-out length in milliseconds, 0.0–60000.0, ending at the
    /// clip's last audible sample.
    #[serde(default, skip_serializing_if = "Tenths::is_zero")]
    pub fade_out_ms: Tenths,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl AudioClip {
    pub fn end(&self) -> Option<u64> {
        self.start.checked_add(self.duration)
    }
}

/// Explicit mapping of source channels to the stereo clip signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClipChannels {
    /// A two-channel source: channel 0 left, channel 1 right.
    Stereo,
    /// One source channel (0-based) on both sides at unity: a mono source
    /// becomes centered stereo with `channel: 0`.
    Channel { channel: u16 },
    /// The per-frame mean of every source channel on both sides; needs at
    /// least two channels.
    Downmix,
}

/// Fields of an existing clip to replace; the rest stay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClipUpdate {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_start: Option<u64>,
    /// Replaces the trim out; to play to the file end again, remove and add the clip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_end: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<ClipChannels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalization: Option<ClipNormalization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<Tenths>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_in_ms: Option<Tenths>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_out_ms: Option<Tenths>,
}

/// A decoded retained WAV: channel-major values in PCM16 units (integer
/// codes scaled by powers of two, float 1.0 = 32768), exact for every format.
#[derive(Debug, Clone)]
pub struct ClipSource {
    pub rate: u32,
    pub channels: Vec<Vec<f64>>,
    /// Header facts of the file, for reports.
    pub facts: Value,
}

impl ClipSource {
    /// Decodes a RIFF/WAVE file with the shared parser.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err(format!(
                "the source has {} bytes; clips use WAV files up to {MAX_SOURCE_BYTES} bytes",
                bytes.len()
            ));
        }
        let file = crate::wav::parse(bytes).map_err(|e| format!("not a usable WAV file: {e}"))?;
        if file.sample_rate == 0 {
            return Err("the WAV header declares a sample rate of 0".into());
        }
        let channels = crate::sample_wav::channel_values(&file).map_err(|e| e.to_string())?;
        let facts = json!({
            "format": if file.format == crate::wav::Format::Int { "pcm_integer" } else { "ieee_float" },
            "bits_per_sample": file.bits, "channels": file.channels,
            "sample_rate": file.sample_rate, "frames": file.frames,
        });
        Ok(Self {
            rate: file.sample_rate,
            channels,
            facts,
        })
    }

    pub fn frames(&self) -> u64 {
        self.channels.first().map_or(0, |c| c.len() as u64)
    }
}

/// Decoded sources by SHA-256.
pub type ClipSources = BTreeMap<String, ClipSource>;

/// Summed stereo clip signal per voice.
pub(crate) type ClipSignals = BTreeMap<String, [Vec<f32>; 2]>;

/// Checks a clip's trim and channel mapping against its decoded source.
pub fn check_against_source(clip: &AudioClip, source: &ClipSource) -> Result<(), String> {
    let (frames, count) = (source.frames(), source.channels.len());
    if clip.source_start >= frames {
        return Err(format!(
            "source_start {} is not before the source's {frames} frames",
            clip.source_start
        ));
    }
    if let Some(end) = clip.source_end
        && end > frames
    {
        return Err(format!(
            "source_end {end} is after the source's {frames} frames"
        ));
    }
    match clip.channels {
        ClipChannels::Stereo if count != 2 => Err(format!(
            "channels mode stereo needs a two-channel source; this one has {count}; use channel or downmix"
        )),
        ClipChannels::Channel { channel } if usize::from(channel) >= count => Err(format!(
            "channel {channel} is outside the source's {count} channels (0-based)"
        )),
        ClipChannels::Downmix if count < 2 => {
            Err("downmix needs at least two channels; use channel 0 for a mono source".into())
        }
        _ => Ok(()),
    }
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Composition checks of the clips: references, spans, loop and ranges.
pub(crate) fn check_clips(
    composition: &Composition,
    voice_ids: &HashSet<&str>,
    report: &mut Report,
) {
    let clips = &composition.mix.clips;
    let end = composition.playback.end;
    if clips.len() > MAX_CLIPS {
        report.push(
            Code::ValueOutOfRange,
            "mix/clips",
            format!("a revision holds at most {MAX_CLIPS} clips"),
        );
    }
    let mut ids = HashSet::new();
    for (i, clip) in clips.iter().enumerate() {
        let mut at = report.nested(format!("mix/clips/{i}"));
        if clip.id.is_empty() {
            at.push(Code::InvalidRange, "id", "clip id must not be empty");
        } else if !ids.insert(clip.id.as_str()) {
            at.push(
                Code::DuplicateId,
                "id",
                format!("clip `{}` is defined twice", clip.id),
            );
        }
        if !voice_ids.contains(clip.voice.as_str()) {
            at.push(
                Code::UnknownReference,
                "voice",
                format!("voice `{}` does not exist", clip.voice),
            );
        }
        if !is_sha256(&clip.source) {
            at.push(
                Code::InvalidRange,
                "source",
                "source is the lowercase SHA-256 that load_instrument_source returned",
            );
        }
        if let Some(source_end) = clip.source_end
            && source_end <= clip.source_start
        {
            at.push(
                Code::InvalidRange,
                "source_end",
                "source_end must be after source_start",
            );
        }
        if !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&clip.gain_db.value()) {
            at.push(
                Code::ValueOutOfRange,
                "gain_db",
                format!("gain must be {MIN_GAIN_DB}–{MAX_GAIN_DB} dB"),
            );
        }
        for (field, value) in [
            ("fade_in_ms", clip.fade_in_ms),
            ("fade_out_ms", clip.fade_out_ms),
        ] {
            if !(0.0..=MAX_FADE_MS).contains(&value.value()) {
                at.push(
                    Code::ValueOutOfRange,
                    field,
                    format!("a fade is 0.0–{MAX_FADE_MS} ms"),
                );
            }
        }
        if let ClipChannels::Channel { channel } = clip.channels
            && channel >= crate::wav::MAX_CHANNELS
        {
            at.push(
                Code::ValueOutOfRange,
                "channels/channel",
                format!(
                    "WAV files have at most {} channels",
                    crate::wav::MAX_CHANNELS
                ),
            );
        }
        if clip.duration == 0 {
            at.push(
                Code::NonPositiveDuration,
                "duration",
                "clip duration must be positive",
            );
            continue;
        }
        let Some(clip_end) = clip.end() else {
            at.push(Code::ValueOutOfRange, "duration", "clip end overflows");
            continue;
        };
        if clip_end > end {
            at.push(
                Code::EventBeyondEnd,
                "duration",
                format!("clip ends at {clip_end}, after the playback end {end}"),
            );
        }
        if let Some(loop_start) = composition.playback.loop_start
            && clip.start < loop_start
            && loop_start < clip_end
        {
            at.push(
                Code::ClipCrossesLoopStart,
                "",
                format!(
                    "clip `{}` sounds across the loop start {loop_start}; split it there or move it",
                    clip.id
                ),
            );
        }
    }
}

/// Applies `add_clip`, `update_clip` and `remove_clip`.
pub(crate) fn apply_clip_operation(
    composition: &mut Composition,
    operation: &EditOperation,
    report: &mut Report,
) {
    let clips = &mut composition.mix.clips;
    match operation {
        EditOperation::AddClip { clip } => {
            if clips.iter().any(|c| c.id == clip.id) {
                report.push(
                    Code::DuplicateId,
                    "clip/id",
                    format!("clip `{}` already exists; use update_clip", clip.id),
                );
                return;
            }
            clips.push(clip.clone());
        }
        EditOperation::UpdateClip { clip: update } => {
            let Some(clip) = clips.iter_mut().find(|c| c.id == update.id) else {
                report.push(
                    Code::UnknownReference,
                    "clip/id",
                    format!(
                        "clip `{}` does not exist at this point of the request",
                        update.id
                    ),
                );
                return;
            };
            let before = clip.clone();
            if let Some(voice) = &update.voice {
                clip.voice = voice.clone();
            }
            if let Some(source) = &update.source {
                clip.source = source.clone();
            }
            clip.start = update.start.unwrap_or(clip.start);
            clip.duration = update.duration.unwrap_or(clip.duration);
            clip.source_start = update.source_start.unwrap_or(clip.source_start);
            if update.source_end.is_some() {
                clip.source_end = update.source_end;
            }
            clip.channels = update.channels.unwrap_or(clip.channels);
            clip.gain_db = update.gain_db.unwrap_or(clip.gain_db);
            clip.fade_in_ms = update.fade_in_ms.unwrap_or(clip.fade_in_ms);
            clip.fade_out_ms = update.fade_out_ms.unwrap_or(clip.fade_out_ms);
            clip.normalization = update.normalization.unwrap_or(clip.normalization);
            if *clip == before {
                report.push(
                    Code::EmptyEdit,
                    "clip",
                    "set at least one field that changes the clip",
                );
            }
        }
        EditOperation::RemoveClip { id } => {
            let before = clips.len();
            clips.retain(|c| &c.id != id);
            if clips.len() == before {
                report.push(
                    Code::UnknownReference,
                    "id",
                    format!("clip `{id}` does not exist at this point of the request"),
                );
            }
        }
        _ => unreachable!("only clip operations are delegated here"),
    }
}

fn by_id(clips: &[AudioClip]) -> BTreeMap<&str, &AudioClip> {
    clips.iter().map(|c| (c.id.as_str(), c)).collect()
}

/// Ids of clips that differ between two clip lists.
pub(crate) fn changed_clips<'a>(before: &'a [AudioClip], after: &'a [AudioClip]) -> Vec<&'a str> {
    let (old, new) = (by_id(before), by_id(after));
    old.keys()
        .chain(new.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|id| old.get(**id) != new.get(**id))
        .copied()
        .collect()
}

/// A clip change is voice data located at the clip's span, before and after.
pub(crate) fn check_clip_bounds(
    before: &[AudioClip],
    after: &[AudioClip],
    request: &EditRequest,
    report: &mut Report,
) {
    let (old, new) = (by_id(before), by_id(after));
    for id in changed_clips(before, after) {
        let versions: Vec<(&str, Option<(u64, u64)>)> = old
            .get(id)
            .into_iter()
            .chain(new.get(id))
            .map(|c| {
                (
                    c.voice.as_str(),
                    Some((c.start, c.end().unwrap_or(u64::MAX))),
                )
            })
            .collect();
        check_change(request, &versions, report, format!("clip `{id}`"));
    }
}

/// Labels of clips that differ, with their voice, for `changed_mix`.
pub(crate) fn clip_labels(
    before: &[AudioClip],
    after: &[AudioClip],
) -> Vec<(Option<String>, String)> {
    let (old, new) = (by_id(before), by_id(after));
    changed_clips(before, after)
        .into_iter()
        .map(|id| {
            let clip = new.get(id).or_else(|| old.get(id)).expect("listed");
            (
                Some(clip.voice.clone()),
                format!("clip `{id}` on voice `{}`", clip.voice),
            )
        })
        .collect()
}

/// Where one clip plays in a render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    /// Index into `mix.clips`.
    pub clip: usize,
    /// 0-based loop pass of a full render.
    pub pass: u32,
    /// Render sample of the clip's first sample; negative when an excerpt
    /// starts inside the clip.
    pub at: i64,
    /// The clip span's length in samples on the full render's grid.
    pub span: u64,
    /// Render sample at which writing stops: the span end or the content end.
    pub limit: i64,
    /// Samples of the span's last tick: silence shorter than this is the
    /// rounding left by a span set to `duration_for_audio`.
    pub last_tick: u64,
}

/// Placements of the selected voices' clips in a full render of `passes`
/// passes, on that render's sample grid. Clips starting at or after the loop
/// start replay in each later pass; nothing is written at or after `limit`.
/// `composition` supplies the tempo map and playback; `clips` the clips.
pub(crate) fn full_placements(
    timebase: Timebase,
    composition: &Composition,
    clips: &[AudioClip],
    passes: u32,
    sample_rate: u32,
    limit: u64,
    voices: Option<&[String]>,
) -> Vec<Placement> {
    let seconds = |at: u64| seconds_at(timebase, composition, Rational::from_integer(at.into()));
    let loop_start = composition.playback.loop_start;
    let loop_length = seconds(composition.playback.end) - seconds(loop_start.unwrap_or(0));
    let mut out = Vec::new();
    for pass in 0..passes {
        for (index, clip) in clips.iter().enumerate() {
            if voices.is_some_and(|v| !v.contains(&clip.voice)) {
                continue;
            }
            if pass > 0 && !loop_start.is_some_and(|start| clip.start >= start) {
                continue;
            }
            let offset = loop_length * i128::from(pass);
            let first = sample_at(seconds(clip.start) + offset, sample_rate);
            let end = clip.end().unwrap_or(u64::MAX);
            let last = sample_at(seconds(end) + offset, sample_rate);
            let before_last = sample_at(seconds(end.saturating_sub(1)) + offset, sample_rate);
            if first >= limit {
                continue;
            }
            out.push(Placement {
                clip: index,
                pass,
                at: first as i64,
                span: last - first,
                limit: last.min(limit) as i64,
                last_tick: last - before_last,
            });
        }
    }
    out
}

/// Placements of a composed project's render layout. An excerpt crops the
/// clips exactly as the full render places them: clip sample `k` lands at
/// full-render sample `start + k`, shifted by the excerpt start's sample.
pub(crate) fn layout_placements(
    timebase: Timebase,
    composition: &Composition,
    layout: &RenderLayout,
    sample_rate: u32,
    content: u64,
    voices: Option<&[String]>,
) -> Vec<Placement> {
    let clips = &composition.mix.clips;
    match *layout {
        RenderLayout::Full { loop_passes } => full_placements(
            timebase,
            composition,
            clips,
            loop_passes,
            sample_rate,
            content,
            voices,
        ),
        RenderLayout::Excerpt { start, end } => {
            let seconds =
                |at: u64| seconds_at(timebase, composition, Rational::from_integer(at.into()));
            let shift = sample_at(seconds(start), sample_rate) as i64;
            let cut = sample_at(seconds(end), sample_rate);
            full_placements(timebase, composition, clips, 1, sample_rate, cut, voices)
                .into_iter()
                .filter(|p| {
                    let clip = &clips[p.clip];
                    clip.start < end && clip.end().unwrap_or(u64::MAX) > start
                })
                .map(|p| Placement {
                    at: p.at - shift,
                    limit: (p.limit - shift).min(content as i64),
                    ..p
                })
                .collect()
        }
    }
}

fn amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Samples of a fade: `ceil(ms × rate / 1000)`, computed exactly from tenths.
fn fade_samples(ms: Tenths, rate: u32) -> u64 {
    (ms.0.max(0) as u64 * u64::from(rate)).div_ceil(10_000)
}

/// One clip's audio at the render rate before gain and fades.
struct Prepared {
    stereo: [Vec<f64>; 2],
    report: Value,
}

fn prepare(clip: &AudioClip, source: &ClipSource, sample_rate: u32, longest: u64) -> Prepared {
    let first = clip.source_start as usize;
    let last = clip
        .source_end
        .map_or(source.frames() as usize, |e| e as usize);
    let pick = |channel: usize| source.channels[channel][first..last].to_vec();
    let (left, right) = match clip.channels {
        ClipChannels::Stereo => (pick(0), pick(1)),
        ClipChannels::Channel { channel } => {
            let values = pick(usize::from(channel));
            (values.clone(), values)
        }
        ClipChannels::Downmix => {
            let count = source.channels.len() as f64;
            let mean: Vec<f64> = (first..last)
                .map(|f| source.channels.iter().map(|c| c[f]).sum::<f64>() / count)
                .collect();
            (mean.clone(), mean)
        }
    };
    let frames = left.len() as u64;
    if source.rate == sample_rate {
        return Prepared {
            stereo: [left, right],
            report: json!({"resampled": false, "source_rate": source.rate, "render_rate": sample_rate}),
        };
    }
    let g = gcd(u64::from(source.rate), u64::from(sample_rate));
    let (p, q) = (u64::from(source.rate) / g, u64::from(sample_rate) / g);
    let natural = (frames * q).div_ceil(p);
    let count = natural.min(longest) as usize;
    let mut out =
        crate::sample_resample::stream_values(&[&left, &right], source.rate, sample_rate, count);
    let right = out.pop().expect("two channels");
    let left = out.pop().expect("two channels");
    Prepared {
        stereo: [left, right],
        report: json!({"resampled": true, "method": crate::sample_resample::METHOD,
            "source_rate": source.rate, "render_rate": sample_rate,
            "ratio": format!("{q}/{p}"), "source_frames": frames, "render_samples": natural,
            "computed_samples": count,
            "zero_crossings_per_side": crate::sample_resample::ZERO_CROSSINGS,
            "kaiser_beta": crate::sample_resample::KAISER_BETA,
            "cutoff_of_lower_nyquist": crate::sample_resample::CUTOFF,
            "boundary": "silence outside the trimmed source range",
            "rounding": "none; values enter the mixer unrounded"}),
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Clip signals per voice, `length` samples long, and one report per clip
/// that played. Each clip retains its amplitude convention; omitted values
/// preserve the historical PCM16 code scale.
pub(crate) fn clip_signals(
    clips: &[AudioClip],
    placements: &[Placement],
    sources: &ClipSources,
    sample_rate: u32,
    length: usize,
    audible: impl Fn(&str) -> bool,
) -> Result<(ClipSignals, Vec<Value>), Diagnostic> {
    let mut signals: BTreeMap<String, [Vec<f32>; 2]> = BTreeMap::new();
    let mut reports = Vec::new();
    let used: BTreeSet<usize> = placements.iter().map(|p| p.clip).collect();
    for index in used {
        let clip = &clips[index];
        if !audible(&clip.voice) {
            continue;
        }
        let at = format!("/mix/clips/{index}");
        let source = sources.get(&clip.source).ok_or_else(|| Diagnostic {
            code: Code::UnknownReference,
            location: format!("{at}/source"),
            message: format!("sound source {} was not loaded for the render", clip.source),
        })?;
        check_against_source(clip, source).map_err(|message| Diagnostic {
            code: Code::InvalidAudio,
            location: at.clone(),
            message: format!("clip `{}`: {message}", clip.id),
        })?;
        let own: Vec<&Placement> = placements.iter().filter(|p| p.clip == index).collect();
        let longest = own.iter().map(|p| p.span).max().unwrap_or(0);
        let prepared = prepare(clip, source, sample_rate, longest);
        let gain = amplitude(clip.gain_db.value());
        let fade_in = fade_samples(clip.fade_in_ms, sample_rate);
        let fade_out = fade_samples(clip.fade_out_ms, sample_rate);
        let available = prepared.stereo[0].len() as u64;
        let signal = signals
            .entry(clip.voice.clone())
            .or_insert_with(|| [vec![0.0; length], vec![0.0; length]]);
        let mut written = Vec::new();
        for placement in own {
            let audible_length = available.min(placement.span);
            let from = (-placement.at).max(0) as u64;
            let to = (placement.limit - placement.at)
                .clamp(0, audible_length as i64)
                .min(length as i64 - placement.at) as u64;
            for k in from..to {
                let mut g = gain;
                if k < fade_in {
                    g *= k as f64 / fade_in as f64;
                }
                if k + fade_out >= audible_length {
                    g *= (audible_length - k) as f64 / fade_out as f64;
                }
                let n = (placement.at + k as i64) as usize;
                for (side, values) in signal.iter_mut().zip(&prepared.stereo) {
                    let value = values[k as usize] * g / clip.normalization.divisor();
                    side[n] = (f64::from(side[n]) + value) as f32;
                }
            }
            written.push(json!({"pass": placement.pass + 1,
                "render_sample": placement.at, "first_clip_sample": from,
                "samples_written": to.saturating_sub(from),
                "audible_samples": audible_length,
                "silent_samples": placement.span - audible_length,
                "source_exhausted": placement.span - audible_length >= placement.last_tick.max(1)}));
        }
        reports.push(json!({"clip": clip.id, "voice": clip.voice, "source": clip.source,
            "source_file": source.facts, "channels": clip.channels,
            "source_range": {"start": clip.source_start, "end": clip.source_end.unwrap_or(source.frames())},
            "rate": prepared.report, "gain_db": clip.gain_db,
            "source_normalization": {"pcm16_units_per_full_scale": clip.normalization.divisor(),
                "gain_from_normalized_wav": 32768.0 / clip.normalization.divisor()},
            "fades": {"curve": "linear in amplitude", "fade_in_samples": fade_in, "fade_out_samples": fade_out},
            "placements": written}));
    }
    Ok((signals, reports))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(start: u64, duration: u64) -> AudioClip {
        AudioClip {
            id: "c".into(),
            voice: "v".into(),
            source: "0".repeat(64),
            start,
            duration,
            source_start: 0,
            source_end: None,
            channels: ClipChannels::Channel { channel: 0 },
            gain_db: Tenths(0),
            fade_in_ms: Tenths(0),
            fade_out_ms: Tenths(0),
            normalization: ClipNormalization::default(),
        }
    }

    fn source(values: Vec<f64>, rate: u32) -> ClipSource {
        ClipSource {
            rate,
            channels: vec![values],
            facts: Value::Null,
        }
    }

    fn render(
        clip: &AudioClip,
        source: &ClipSource,
        at: i64,
        span: u64,
        length: usize,
    ) -> Vec<f32> {
        let mut sources = ClipSources::new();
        sources.insert(clip.source.clone(), source.clone());
        let placement = Placement {
            clip: 0,
            pass: 0,
            at,
            span,
            limit: at + span as i64,
            last_tick: 1,
        };
        let (mut signals, _) = clip_signals(
            std::slice::from_ref(clip),
            &[placement],
            &sources,
            source.rate,
            length,
            |_| true,
        )
        .unwrap();
        let [left, right] = signals.remove("v").unwrap();
        assert_eq!(left, right);
        left
    }

    #[test]
    fn fades_are_linear_and_mirror_each_other() {
        let mut c = clip(0, 1);
        // 10 ms at 1000 Hz is 10 samples.
        c.fade_in_ms = Tenths(100);
        c.fade_out_ms = Tenths(100);
        let s = source(vec![32767.0; 40], 1000);
        let out = render(&c, &s, 0, 40, 40);
        for k in 0..10 {
            assert_eq!(out[k], (k as f64 / 10.0) as f32, "fade in {k}");
            assert_eq!(out[39 - k], ((k + 1) as f64 / 10.0) as f32, "fade out {k}");
        }
        assert_eq!(out[20], 1.0);
        assert_eq!(fade_samples(Tenths(1), 44100), 5);
    }

    #[test]
    fn excerpt_starts_inside_the_clip_and_the_span_cuts_the_audio() {
        let c = clip(0, 1);
        let s = source((0..20).map(f64::from).collect(), 1000);
        let out = render(&c, &s, -5, 10, 8);
        let expected: Vec<f32> = (5..10).map(|v| (v as f64 / 32767.0) as f32).collect();
        assert_eq!(&out[..5], &expected[..]);
        assert_eq!(&out[5..], &[0.0; 3]);
    }

    #[test]
    fn source_checks_name_the_problem() {
        let mut c = clip(0, 1);
        let s = source(vec![0.0; 4], 8000);
        c.channels = ClipChannels::Stereo;
        assert!(
            check_against_source(&c, &s)
                .unwrap_err()
                .contains("two-channel")
        );
        c.channels = ClipChannels::Downmix;
        assert!(
            check_against_source(&c, &s)
                .unwrap_err()
                .contains("downmix")
        );
        c.channels = ClipChannels::Channel { channel: 1 };
        assert!(
            check_against_source(&c, &s)
                .unwrap_err()
                .contains("outside")
        );
        c.channels = ClipChannels::Channel { channel: 0 };
        c.source_end = Some(5);
        assert!(check_against_source(&c, &s).unwrap_err().contains("after"));
        c.source_end = None;
        c.source_start = 4;
        assert!(
            check_against_source(&c, &s)
                .unwrap_err()
                .contains("not before")
        );
    }
}
