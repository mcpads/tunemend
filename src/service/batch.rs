//! Batch rendering: regions, stems and WAV formats in one call.

use super::review::{add_loop_timing, layout_label, layout_problem, played_by, render_limits};
use super::*;
use crate::mix::part_voices;
use crate::render::{StemCapture, wav_bytes_in};
use crate::review::{RenderPart, WavFormat};

/// Most renders one `render_batch` call writes.
pub const MAX_BATCH_RENDERS: usize = 64;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderBatchInput {
    pub job_id: String,
    pub project: String,
    #[serde(default)]
    pub revision: Option<String>,
    /// What to render; each span gives its own mixdown and stems. Default:
    /// the whole piece once.
    #[serde(default = "whole_piece")]
    pub spans: Vec<RenderSpan>,
    /// Write each span's mixdown. Default true.
    #[serde(default = "yes")]
    pub mixdown: bool,
    /// Split each span into stems that sum to its mixdown.
    #[serde(default)]
    pub stems: Option<StemSelection>,
    /// Where stems stop. Default `through_master`.
    #[serde(default)]
    pub stem_master: StemMaster,
    /// Default 44100.
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// Sample encoding of every WAV. Default `pcm16`.
    #[serde(default)]
    pub format: WavFormat,
    /// What follows each span's content. `keep` (default) keeps the decay
    /// tail, so a section render runs past the section end and overlaps the
    /// next section when placed at its start; `cut` ends every output at the
    /// span's content end (section or excerpt end, end of the last loop pass).
    #[serde(default, skip_serializing_if = "TailPolicy::is_keep")]
    pub tail: TailPolicy,
    /// Instead of new renders, restore the audio of these existing records
    /// (such as renders imported with `missing_renders: without_audio`):
    /// each is rendered again from its own revision and settings and must
    /// equal its recorded hash. No record changes; leave the other fields out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regenerate_renders: Option<Vec<String>>,
}

/// What a batch render keeps after the content of its span.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TailPolicy {
    /// The decay after the content, as `render_preview` renders it (until
    /// it falls silent, at most the reported `max_tail_seconds`).
    #[default]
    Keep,
    /// No tail: every output ends at the content end, so notes still
    /// sounding there stop abruptly.
    Cut,
}

impl TailPolicy {
    fn is_keep(&self) -> bool {
        *self == Self::Keep
    }
}

fn whole_piece() -> Vec<RenderSpan> {
    vec![RenderSpan::Full { loop_passes: 1 }]
}

fn yes() -> bool {
    true
}

/// A stretch of the revision to render.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RenderSpan {
    /// The intro once, then the loop region `loop_passes` times, as in
    /// `render_preview`.
    Full { loop_passes: u32 },
    /// The source range `[start, end)` once, in timebase units.
    Excerpt { start: u64, end: u64 },
    /// One section (region) of the revision: an excerpt of its bounds.
    Section { id: String },
    /// Every section of the revision, in listed order.
    Sections,
}

/// How to split the mix into stems.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "by", rename_all = "snake_case", deny_unknown_fields)]
pub enum StemSelection {
    /// One stem per voice: everything the voice contributes, through its
    /// own chain, fader, sends and every bus and master stage it reaches.
    /// `voices` limits which; default every heard voice.
    Voice {
        #[serde(default)]
        voices: Option<Vec<String>>,
    },
    /// One stem per unmuted bus, plus `direct`: the voices routed straight
    /// to the master.
    Bus,
    /// One stem per group, the sum of its voices' stems. A voice may be in
    /// at most one group.
    Group { groups: Vec<VoiceGroup> },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceGroup {
    pub id: String,
    pub voices: Vec<String>,
}

/// Where stems leave the mix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StemMaster {
    /// Through the master volume and master chain. Compressors and limiters
    /// on buses and the master apply the gain they computed from the whole
    /// mix to every stem, so stems sum to the mixdown.
    #[default]
    ThroughMaster,
    /// Before the master volume and master chain: stems sum to the master
    /// input, not to the mixdown, for mastering elsewhere.
    PreMaster,
}

impl Service {
    pub(super) fn render_batch(&mut self, input: RenderBatchInput) -> Handled {
        if let Some(ids) = &input.regenerate_renders {
            let defaults = input.revision.is_none()
                && matches!(
                    input.spans.as_slice(),
                    [RenderSpan::Full { loop_passes: 1 }]
                )
                && input.mixdown
                && input.stems.is_none()
                && input.stem_master == StemMaster::ThroughMaster
                && input.sample_rate.is_none()
                && input.format == WavFormat::Pcm16
                && input.tail == TailPolicy::Keep;
            if !defaults {
                return done(ToolResponse::refused(
                    Code::InvalidRange,
                    "/regenerate_renders",
                    "regenerate_renders restores existing records from their own settings; leave revision, spans, mixdown, stems, stem_master, sample_rate, format and tail out",
                ));
            }
            return self.regenerate_renders(&input.project, ids);
        }
        let (info, revision) = self.revision(&input.project, input.revision.as_deref())?;
        let composition = &revision.composition;
        let sample_rate = input.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE);
        if !(16_000..=192_000).contains(&sample_rate) {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/sample_rate",
                "sample rate must be 16000–192000",
            ));
        }
        if input.spans.is_empty() {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/spans",
                "list at least one span",
            ));
        }
        let section_ids = || {
            composition
                .sections
                .iter()
                .map(|s| format!("`{}`", s.id))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut layouts: Vec<(usize, RenderLayout, Option<String>)> = Vec::new();
        for (i, span) in input.spans.iter().enumerate() {
            match span {
                RenderSpan::Full { loop_passes } => layouts.push((
                    i,
                    RenderLayout::Full {
                        loop_passes: *loop_passes,
                    },
                    None,
                )),
                RenderSpan::Excerpt { start, end } => layouts.push((
                    i,
                    RenderLayout::Excerpt {
                        start: *start,
                        end: *end,
                    },
                    None,
                )),
                RenderSpan::Section { id } => {
                    let Some(section) = composition.sections.iter().find(|s| &s.id == id) else {
                        return done(ToolResponse::refused(
                            Code::UnknownReference,
                            &format!("/spans/{i}/id"),
                            if composition.sections.is_empty() {
                                format!(
                                    "section `{id}` does not exist; the revision has no sections (declare them with set_sections)"
                                )
                            } else {
                                format!(
                                    "section `{id}` does not exist; the revision has {}",
                                    section_ids()
                                )
                            },
                        ));
                    };
                    layouts.push((
                        i,
                        RenderLayout::Excerpt {
                            start: section.start,
                            end: section.end,
                        },
                        Some(section.id.clone()),
                    ));
                }
                RenderSpan::Sections => {
                    if composition.sections.is_empty() {
                        return done(ToolResponse::refused(
                            Code::UnknownReference,
                            &format!("/spans/{i}"),
                            "the revision has no sections; declare them with set_sections",
                        ));
                    }
                    for section in &composition.sections {
                        layouts.push((
                            i,
                            RenderLayout::Excerpt {
                                start: section.start,
                                end: section.end,
                            },
                            Some(section.id.clone()),
                        ));
                    }
                }
            }
        }
        for (i, layout, _) in &layouts {
            if let Some(problem) = layout_problem(composition, layout) {
                return done(ToolResponse::refused(
                    Code::InvalidRenderLayout,
                    &format!("/spans/{i}"),
                    problem,
                ));
            }
        }

        let mix = &composition.mix;
        let voice_ids: Vec<String> = composition.voices.iter().map(|v| v.id.clone()).collect();
        let known = |voice: &String| voice_ids.contains(voice);
        let heard: Vec<String> = voice_ids
            .iter()
            .filter(|v| mix.audible(v))
            .cloned()
            .collect();
        let mut parts = Vec::new();
        let mut not_heard = Vec::new();
        let mut covered: Vec<String> = Vec::new();
        match &input.stems {
            None => {}
            Some(StemSelection::Voice { voices }) => {
                let selected = voices.clone().unwrap_or_else(|| voice_ids.clone());
                for (j, voice) in selected.iter().enumerate() {
                    if !known(voice) {
                        return done(ToolResponse::refused(
                            Code::UnknownReference,
                            &format!("/stems/voices/{j}"),
                            format!("voice `{voice}` does not exist"),
                        ));
                    }
                    if selected[..j].contains(voice) {
                        return done(ToolResponse::refused(
                            Code::DuplicateId,
                            &format!("/stems/voices/{j}"),
                            format!("voice `{voice}` is listed twice"),
                        ));
                    }
                    if mix.audible(voice) {
                        parts.push(RenderPart::Voice { id: voice.clone() });
                        covered.push(voice.clone());
                    } else {
                        not_heard.push(json!({"voice": voice}));
                    }
                }
            }
            Some(StemSelection::Bus) => {
                for bus in &mix.buses {
                    if bus.mute {
                        not_heard.push(json!({"bus": bus.id}));
                    } else {
                        parts.push(RenderPart::Bus { id: bus.id.clone() });
                    }
                }
                parts.push(RenderPart::Direct);
                covered = heard.clone();
            }
            Some(StemSelection::Group { groups }) => {
                if groups.is_empty() {
                    return done(ToolResponse::refused(
                        Code::InvalidRange,
                        "/stems/groups",
                        "declare at least one group",
                    ));
                }
                let mut grouped: Vec<&String> = Vec::new();
                for (g, group) in groups.iter().enumerate() {
                    let at = format!("/stems/groups/{g}");
                    if group.id.is_empty() || groups[..g].iter().any(|o| o.id == group.id) {
                        return done(ToolResponse::refused(
                            Code::DuplicateId,
                            &format!("{at}/id"),
                            format!("group ids must be unique and not empty: `{}`", group.id),
                        ));
                    }
                    if group.voices.is_empty() {
                        return done(ToolResponse::refused(
                            Code::InvalidRange,
                            &format!("{at}/voices"),
                            "a group needs at least one voice",
                        ));
                    }
                    for (j, voice) in group.voices.iter().enumerate() {
                        if !known(voice) {
                            return done(ToolResponse::refused(
                                Code::UnknownReference,
                                &format!("{at}/voices/{j}"),
                                format!("voice `{voice}` does not exist"),
                            ));
                        }
                        if grouped.contains(&voice) {
                            return done(ToolResponse::refused(
                                Code::DuplicateId,
                                &format!("{at}/voices/{j}"),
                                format!(
                                    "voice `{voice}` is already in another group or listed twice; stems that share a voice would not sum to the mixdown"
                                ),
                            ));
                        }
                        grouped.push(voice);
                    }
                    if group.voices.iter().any(|v| mix.audible(v)) {
                        parts.push(RenderPart::Group {
                            id: group.id.clone(),
                            voices: group.voices.clone(),
                        });
                        covered.extend(group.voices.iter().cloned());
                    } else {
                        not_heard.push(json!({"group": group.id}));
                    }
                }
            }
        }
        if !input.mixdown && parts.is_empty() {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/stems",
                "nothing to render: request the mixdown or stems of heard voices or buses",
            ));
        }
        let per_span = parts.len() + usize::from(input.mixdown);
        if per_span * layouts.len() > MAX_BATCH_RENDERS {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/spans",
                format!(
                    "{} spans × {per_span} renders exceed the limit of {MAX_BATCH_RENDERS} renders per call; split the batch",
                    layouts.len()
                ),
            ));
        }
        let pre_master = input.stem_master == StemMaster::PreMaster;
        let capture = (!parts.is_empty()).then(|| StemCapture {
            parts: parts.clone(),
            pre_master,
        });
        let uncovered: Vec<&String> = heard.iter().filter(|v| !covered.contains(v)).collect();

        let store = self.store_ref()?;
        let mut next = next_render_number(&store.reader(), &input.project)?;
        let mut records: Vec<RenderRecord> = Vec::new();
        let mut listed = Vec::new();
        let mut spans = Vec::new();
        let mut artifacts = Vec::new();
        let mut warnings = Vec::new();
        for (i, layout, region) in &layouts {
            let (mut audio, reports, instruments) = match self.revision_audio(
                &info,
                &revision,
                layout,
                None,
                sample_rate,
                capture.as_ref(),
            ) {
                Ok(rendered) => rendered,
                Err(response) => return done(response),
            };
            let tail_cut = input.tail == TailPolicy::Cut;
            if tail_cut {
                cut_tail(
                    &mut audio,
                    content_length(info.timebase, composition, layout, sample_rate),
                );
            }
            let all_played = played_by(composition, &instruments, &reports.clips);
            let mut outputs: Vec<(Option<RenderPart>, &[f32], &[f32])> = Vec::new();
            if input.mixdown {
                outputs.push((
                    capture.as_ref().map(|_| RenderPart::Mixdown),
                    &audio.left,
                    &audio.right,
                ));
            }
            for stem in &audio.stems {
                outputs.push((Some(stem.part.clone()), &stem.left, &stem.right));
            }
            let store = self.store_ref()?;
            let mut ids = Vec::new();
            for (part, left, right) in outputs {
                let wav = wav_bytes_in(sample_rate, left, right, input.format);
                let object = store.put_object(&wav)?;
                let id = format!("render-{next}");
                next += 1;
                let voices_of_part = part
                    .as_ref()
                    .filter(|p| **p != RenderPart::Mixdown)
                    .map(|p| part_voices(mix, &voice_ids, p));
                let stem = voices_of_part.is_some();
                let render = Render {
                    id: id.clone(),
                    revision: revision.id.clone(),
                    layout: *layout,
                    voices: None,
                    sound_source: REFERENCE_SOUND_SOURCE.into(),
                    played_by: all_played
                        .iter()
                        .filter(|p| voices_of_part.as_ref().is_none_or(|v| v.contains(&p.voice)))
                        .cloned()
                        .collect(),
                    sample_rate,
                    length_samples: left.len() as u64,
                    tail_truncated: audio.tail_truncated,
                    sha256: Some(object.clone()),
                    region: region.clone(),
                    part,
                    pre_master: stem && pre_master,
                    format: input.format,
                    tail_cut,
                };
                let path = store.object_path(&object);
                artifacts.push(Artifact {
                    kind: "wav".into(),
                    path: path.display().to_string(),
                    sha256: object.clone(),
                });
                let levels = render_levels(left, right, input.format);
                if let Some(warning) = level_warning(
                    &levels,
                    input.format,
                    &format!("/renders/{}/levels", listed.len()),
                ) {
                    warnings.push(warning);
                }
                let mut entry = json!({"render": render, "path": path, "levels": levels});
                if silent(left, right, input.format) {
                    entry["silent"] = json!(true);
                }
                listed.push(entry);
                ids.push(id);
                records.push(RenderRecord { render, object });
            }
            let content = content_seconds(info.timebase, composition, layout);
            let total_seconds = audio.left.len() as f64 / f64::from(sample_rate);
            let mut entry = json!({
                "span": input.spans[*i], "region": region, "layout": layout,
                "label": layout_label(layout), "renders": ids,
                "timing": {"content_seconds": rational_seconds(content), "total_seconds": total_seconds,
                    "tail_seconds": total_seconds - rational_seconds(content),
                    "tail_truncated": audio.tail_truncated, "max_tail_seconds": MAX_TAIL_SECONDS},
            });
            if !reports.clips.is_empty() {
                entry["clips"] = json!(reports.clips);
            }
            add_loop_timing(&mut entry["timing"], info.timebase, composition, layout);
            if tail_cut {
                entry["timing"]["tail"] = json!("cut");
            }
            if !audio.stems.is_empty() {
                entry["sum_check"] = sum_check(&audio, pre_master, &uncovered, input.format);
            }
            spans.push(entry);
        }

        let mut result = json!({
            "renders": listed,
            "spans": spans,
            "format": {"encoding": input.format, "conversion": match input.format {
                WavFormat::Pcm16 => "16-bit PCM: round(clamp(x, -1, 1) × 32767), as render_preview writes",
                WavFormat::Pcm24 => "24-bit PCM: round(clamp(x, -1, 1) × 8388607), computed in f64",
                WavFormat::Float32 => "32-bit IEEE float (format tag 3, fact chunk): the renderer's f32 samples unchanged, not clamped",
            }},
            "limits": render_limits(!composition.mix.instruments.is_empty(), !composition.mix.clips.is_empty()),
        });
        if let Some(stems) = &input.stems {
            result["stems"] = json!({
                "by": stems, "stem_master": input.stem_master, "not_heard": not_heard,
                "rendering": "every voice rendered once with its own synthesizer or chip instrument, as render_preview does for a mixer; each stem follows its share of the signal through the mixer, so a stem never depends on which other stems were requested",
            });
        }
        let ids: Vec<String> = records.iter().map(|r| r.render.id.clone()).collect();
        let mut summary = format!(
            "{} renders of {} ({}): {}",
            records.len(),
            revision.id,
            match &input.stems {
                None => "mixdowns".to_owned(),
                Some(_) => format!("{} stems per span", parts.len()),
            },
            ids.join(", ")
        );
        if !warnings.is_empty() {
            summary.push_str(&format!(
                "; {} renders exceed full scale (see warnings)",
                warnings.len()
            ));
            result["warnings"] = json!(warnings);
        }
        let mut response = ToolResponse::ok(summary, result);
        response.artifacts = artifacts;
        let project = input.project;
        commit(response, move |writer| {
            for record in &records {
                writer.put_document(&project, "render", &record.render.id, record)?;
            }
            Ok(())
        })
    }

    /// Restores the absent audio of existing render records, checked
    /// against their recorded hashes.
    fn regenerate_renders(&mut self, project: &str, ids: &[String]) -> Handled {
        let (info, _) = self.project(project)?;
        if ids.is_empty() {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/regenerate_renders",
                "list at least one render",
            ));
        }
        let mut wanted = Vec::new();
        {
            let store = self.store_ref()?;
            let reader = store.reader();
            for (i, id) in ids.iter().enumerate() {
                let Some(record) = reader.document::<RenderRecord>(project, "render", id)? else {
                    return done(ToolResponse::refused(
                        Code::UnknownReference,
                        &format!("/regenerate_renders/{i}"),
                        format!("render `{id}` does not exist in `{project}`"),
                    ));
                };
                let revision = reader
                    .revision(project, &record.render.revision)?
                    .ok_or_else(|| ToolResponse::error("the render's revision is missing"))?;
                wanted.push((i, record, revision));
            }
        }
        let (mut restored, mut present, mut artifacts) = (Vec::new(), Vec::new(), Vec::new());
        for (i, record, revision) in wanted {
            if record.audio_present(self.store_ref()?) {
                present.push(record.render.id.clone());
                continue;
            }
            let (bytes, _) = self.record_audio(&info, &revision, &record.render)?;
            if sha256_hex(&bytes) != record.object {
                return done(ToolResponse::refused(
                    Code::RevisionContentMismatch,
                    &format!("/regenerate_renders/{i}"),
                    format!(
                        "the audio rendered again for `{}` differs from its recorded hash (this renderer did not make it); its record still has no audio. Render the revision anew with render_preview or render_batch instead",
                        record.render.id
                    ),
                ));
            }
            let store = self.store_ref()?;
            let object = store.put_object(&bytes)?;
            let path = store.object_path(&object);
            artifacts.push(Artifact {
                kind: "wav".into(),
                path: path.display().to_string(),
                sha256: object.clone(),
            });
            restored.push(json!({"render": record.render.id, "sha256": object, "path": path}));
        }
        let mut response = ToolResponse::ok(
            format!(
                "restored the audio of {} renders; {} already had audio",
                restored.len(),
                present.len()
            ),
            json!({"regenerated": restored, "already_present": present,
                "checked": "each WAV rendered again from its record equals the recorded sha256; no record changed"}),
        );
        response.artifacts = artifacts;
        done(response)
    }

    /// The WAV bytes of a render record, rendered again from its revision:
    /// the same layout, voices, part, stem policy, rate and format.
    pub(super) fn record_audio(
        &mut self,
        info: &ProjectInfo,
        revision: &Revision,
        render: &Render,
    ) -> Result<(Vec<u8>, u64), ToolResponse> {
        let capture = render.part.as_ref().map(|part| StemCapture {
            parts: vec![part.clone()],
            pre_master: render.pre_master,
        });
        let (mut audio, _, _) = self.revision_audio(
            info,
            revision,
            &render.layout,
            render.voices.as_deref(),
            render.sample_rate,
            capture.as_ref(),
        )?;
        if render.tail_cut {
            cut_tail(
                &mut audio,
                content_length(
                    info.timebase,
                    &revision.composition,
                    &render.layout,
                    render.sample_rate,
                ),
            );
        }
        let (left, right) = match audio.stems.first() {
            Some(stem) => (&stem.left, &stem.right),
            None => (&audio.left, &audio.right),
        };
        Ok((
            wav_bytes_in(render.sample_rate, left, right, render.format),
            left.len() as u64,
        ))
    }
}

/// Samples of content in `layout` at `rate`, rounded up: where a
/// `tail: cut` render ends, and the least a stored render must hold.
fn content_length(
    timebase: Timebase,
    composition: &Composition,
    layout: &RenderLayout,
    rate: u32,
) -> usize {
    (content_seconds(timebase, composition, layout) * i128::from(rate))
        .ceil()
        .to_integer() as usize
}

/// Ends the mixdown, its stems and the master input at `length` samples.
fn cut_tail(audio: &mut crate::render::RenderedAudio, length: usize) {
    for side in [&mut audio.left, &mut audio.right]
        .into_iter()
        .chain(
            audio
                .stems
                .iter_mut()
                .flat_map(|s| [&mut s.left, &mut s.right]),
        )
        .chain(audio.pre_master_mix.iter_mut().flatten())
    {
        side.truncate(length);
    }
    audio.tail_truncated = false;
}

/// The integer code an integer `format` writes for `sample`, computed as
/// `wav_bytes_in` computes it; `None` for 32-bit float.
fn pcm_code(sample: f32, format: WavFormat) -> Option<i64> {
    match format {
        WavFormat::Pcm16 => Some((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i64),
        WavFormat::Pcm24 => Some((f64::from(sample.clamp(-1.0, 1.0)) * 8_388_607.0).round() as i64),
        WavFormat::Float32 => None,
    }
}

/// The levels of one render before and after encoding: the f32 peak and the
/// samples beyond full scale, kept apart from the samples the integer
/// encoding clamped. Measured on the samples written; nothing is scaled.
pub(super) fn render_levels(left: &[f32], right: &[f32], format: WavFormat) -> Value {
    let (mut peak, mut over) = (0.0f32, 0usize);
    for sample in left.iter().chain(right) {
        peak = peak.max(sample.abs());
        over += usize::from(sample.abs() > 1.0);
    }
    let clamped = if format == WavFormat::Float32 {
        0
    } else {
        over
    };
    let peak_dbfs = (peak > 0.0).then(|| 20.0 * f64::from(peak).log10());
    json!({"sample_peak": peak, "sample_peak_dbfs": peak_dbfs,
        "samples_over_full_scale": over, "clamped_samples": clamped})
}

/// A warning for a render whose samples exceed full scale, located at its
/// `levels`; `None` when none do. The audio is unchanged either way.
pub(super) fn level_warning(levels: &Value, format: WavFormat, location: &str) -> Option<Value> {
    let over = levels["samples_over_full_scale"]
        .as_u64()
        .filter(|&n| n > 0)?;
    let peak = levels["sample_peak"].as_f64().unwrap_or(0.0);
    let advice = "No level was changed. For headroom, lower a mixer or master volume or add a master limiter in a new revision (the MIDI is unaffected), then render again";
    Some(if format == WavFormat::Float32 {
        json!({"code": "samples_over_full_scale", "location": format!("{location}/samples_over_full_scale"),
            "message": format!("{over} samples exceed full scale (peak {peak:.4}); the float file keeps them, but playback or integer conversion will clip them. {advice}")})
    } else {
        json!({"code": "integer_clipping", "location": format!("{location}/clamped_samples"),
            "message": format!("{over} samples exceed full scale (peak {peak:.4}) and were clamped to the full-scale code by the integer encoding. {advice}, or render float32, which keeps the levels")})
    })
}

/// Every sample of the file is zero in `format`.
fn silent(left: &[f32], right: &[f32], format: WavFormat) -> bool {
    left.iter()
        .chain(right)
        .all(|&s| pcm_code(s, format).map_or(s == 0.0, |code| code == 0))
}

/// How closely the stems of one span sum to their reference: on the f32
/// samples before WAV encoding, and on the codes each file holds.
fn sum_check(
    audio: &crate::render::RenderedAudio,
    pre_master: bool,
    uncovered: &[&String],
    format: WavFormat,
) -> Value {
    let reference: [&[f32]; 2] = match (&audio.pre_master_mix, pre_master) {
        (Some([left, right]), true) => [left, right],
        _ => [&audio.left, &audio.right],
    };
    let mut difference = 0.0f64;
    let mut peak = 0.0f64;
    let (mut code_difference, mut clamped) = (0i64, 0usize);
    for (side, samples) in reference.iter().enumerate() {
        let stem = |s: &crate::render::StemAudio, n: usize| {
            if side == 0 { s.left[n] } else { s.right[n] }
        };
        for (n, value) in samples.iter().enumerate() {
            let sum: f64 = audio.stems.iter().map(|s| f64::from(stem(s, n))).sum();
            difference = difference.max((sum - f64::from(*value)).abs());
            peak = peak.max(f64::from(value.abs()));
            if let Some(reference_code) = pcm_code(*value, format) {
                let codes: i64 = audio
                    .stems
                    .iter()
                    .filter_map(|s| pcm_code(stem(s, n), format))
                    .sum();
                code_difference = code_difference.max((codes - reference_code).abs());
                clamped += audio
                    .stems
                    .iter()
                    .map(|s| stem(s, n))
                    .chain([*value])
                    .filter(|x| x.abs() > 1.0)
                    .count();
            }
        }
    }
    let mut approximated: Vec<&String> = audio
        .stems
        .iter()
        .flat_map(|s| &s.approximated_by)
        .collect();
    approximated.sort();
    approximated.dedup();
    let sums_within_rounding = uncovered.is_empty() && approximated.is_empty();
    let encoded = match format {
        WavFormat::Float32 => json!({
            "format": format, "max_abs_difference": difference,
            "note": "float files hold the measured f32 samples unchanged, so max_abs_difference applies to the files themselves",
        }),
        WavFormat::Pcm16 | WavFormat::Pcm24 => {
            // 16-bit codes are computed in f32, whose product with 32767 can
            // be off by half an f32 step (2^-9 at full scale).
            let (full_scale, product_error) = match format {
                WavFormat::Pcm16 => (f64::from(i16::MAX), 2f64.powi(-10)),
                _ => (8_388_607.0, 0.0),
            };
            let files = (audio.stems.len() + 1) as f64;
            let bound = files * (0.5 + product_error) + difference * full_scale;
            json!({
                "format": format, "full_scale_code": full_scale as i64,
                "max_abs_difference_codes": code_difference,
                "rounding_bound_codes": bound.floor() as i64,
                "clamped_samples": clamped,
                "note": format!(
                    "the summed codes of the {} stem files against the {}; each file rounds to the nearest code on its own, so with no clamped samples the difference stays within rounding_bound_codes: half a code for every stem and the reference, plus max_abs_difference times full_scale_code",
                    audio.stems.len(),
                    if pre_master { "codes the unwritten master input would have" } else { "mixdown file's codes" },
                ),
            })
        }
    };
    let mut check = json!({
        "reference": if pre_master { "the master input before the master volume and master chain (not written)" } else { "the mixdown" },
        "covers_mix": uncovered.is_empty(),
        "uncovered_voices": uncovered,
        "max_abs_difference": difference,
        "reference_peak": peak,
        "approximated_by": approximated,
        "sums_within_rounding": sums_within_rounding,
        "sums_to_mixdown": sums_within_rounding,
        "encoded": encoded,
        "note": "max_abs_difference and reference_peak are measured on the f32 samples before WAV encoding; encoded measures the written codes. Integer formats round each file separately and clamp samples beyond full scale, which is not distributed to stems",
    });
    if pre_master {
        let master_change = match &audio.pre_master_mix {
            Some([left, right]) => left
                .iter()
                .zip(&audio.left)
                .chain(right.iter().zip(&audio.right))
                .map(|(a, b)| (f64::from(*a) - f64::from(*b)).abs())
                .fold(0.0, f64::max),
            None => 0.0,
        };
        let identity = master_change == 0.0;
        check["master_is_identity"] = json!(identity);
        check["mixdown_vs_reference_max_abs_difference"] = json!(master_change);
        check["sums_to_mixdown"] = json!(sums_within_rounding && identity);
        if !identity {
            check["sums_to"] = json!(
                "the master input only: the master volume or master chain changes this render, so the stems do not sum to the written mixdown; sums_within_rounding refers to the master input"
            );
        }
    }
    check
}
