//! Listening on source-only renders. Their records, presentations and
//! feedback belong to one retained MIDI source, never to a project revision.
use super::*;
use crate::bundle::SourceReviews;
use crate::review::SourceRender;

/// Store owner prefix of a source's listening records. Project IDs use only
/// lowercase letters, digits and hyphens, so they never collide with it.
const SOURCE_OWNER_PREFIX: &str = "source:";
pub(super) const SOURCE_RENDERS: &str = "source_render";
/// Folder of source render WAVs in a delivery.
pub(super) const SOURCE_RENDER_FOLDER: &str = "source-renders";

pub(super) fn source_owner(source: &str) -> String {
    format!("{SOURCE_OWNER_PREFIX}{source}")
}

/// The source whose listening records `owner` holds, if it is a source owner.
pub(super) fn reviewed_source(owner: &str) -> Option<&str> {
    owner.strip_prefix(SOURCE_OWNER_PREFIX)
}

pub(super) fn source_render_file(id: &str) -> PathBuf {
    Path::new(SOURCE_RENDER_FOLDER).join(format!("{id}.wav"))
}

/// A bundled source render and its WAV, absent when imported without audio.
pub(super) type IncomingSourceRender = (SourceRender, Option<Vec<u8>>);

/// A presented render as listening needs it, from either kind of record.
pub(super) struct ListenedRender {
    pub length_samples: u64,
    pub sample_rate: u32,
    pub object: String,
    pub source: Option<SourceRender>,
}

impl Service {
    /// Who owns a review call's records: a project, or a loaded source.
    pub(super) fn review_owner(
        &mut self,
        project: Option<&str>,
        source: Option<&str>,
    ) -> Result<String, ToolResponse> {
        match (project, source) {
            (Some(project), None) => {
                self.project(project)?;
                Ok(project.to_owned())
            }
            (None, Some(source)) => {
                self.midi_source_bytes(source)?;
                Ok(source_owner(source))
            }
            (Some(_), Some(_)) => Err(ToolResponse::refused(
                Code::InvalidRange,
                "/source",
                "choose a project or a loaded source, not both",
            )),
            (None, None) => Err(ToolResponse::refused(
                Code::InvalidRange,
                "/project",
                "choose a project, or a loaded source for source renders",
            )),
        }
    }

    /// Checks an owner that arrives through the listening screen's URL.
    pub(super) fn check_review_owner(&mut self, owner: &str) -> Result<(), ToolResponse> {
        match reviewed_source(owner) {
            Some(source) => self.midi_source_bytes(source).map(|_| ()),
            None => self.project(owner).map(|_| ()),
        }
    }

    pub(super) fn listened_render(
        &mut self,
        owner: &str,
        render: &str,
    ) -> Result<Option<ListenedRender>, ToolResponse> {
        let reader = self.store_ref()?.reader();
        Ok(if reviewed_source(owner).is_some() {
            reader
                .document::<SourceRender>(owner, SOURCE_RENDERS, render)?
                .map(|r| ListenedRender {
                    length_samples: r.length_samples,
                    sample_rate: r.sample_rate,
                    object: r.sha256.clone(),
                    source: Some(r),
                })
        } else {
            reader
                .document::<RenderRecord>(owner, "render", render)?
                .map(|r| ListenedRender {
                    length_samples: r.render.length_samples,
                    sample_rate: r.render.sample_rate,
                    object: r.object,
                    source: None,
                })
        })
    }

    /// The listening screen's data for a source presentation. Source renders
    /// have no voices to single out.
    pub(super) fn source_review_page(
        &mut self,
        owner: &str,
        presentation: &str,
    ) -> Result<Value, ToolResponse> {
        self.check_review_owner(owner)?;
        let reader = self.store_ref()?.reader();
        let presentation: Presentation = reader
            .document(owner, "presentation", presentation)?
            .ok_or_else(|| {
                ToolResponse::refused(
                    Code::UnknownReference,
                    "/presentation",
                    "unknown presentation",
                )
            })?;
        let mut items = Vec::new();
        for item in &presentation.renders {
            if let Some(record) =
                reader.document::<SourceRender>(owner, SOURCE_RENDERS, &item.render)?
            {
                items.push(json!({"render": item.render, "label": item.label, "sample_rate": record.sample_rate,
                    "length_samples": record.length_samples, "object": record.sha256, "voices": []}));
            }
        }
        let latest: Vec<Presentation> = reader.documents(owner, "presentation")?;
        let is_latest = latest.last().is_some_and(|p| p.id == presentation.id);
        Ok(
            json!({"project": owner, "source": reviewed_source(owner), "kind": "source",
            "presentation": presentation.id, "title": presentation.title, "items": items, "is_latest": is_latest}),
        )
    }

    /// Keeps a source preview's WAV and a record of it for listening.
    pub(super) fn keep_source_render(
        &mut self,
        source: &str,
        sequence: Option<usize>,
        layout: RenderLayout,
        rendered: &crate::render::SourceRender,
        wav: &[u8],
    ) -> Result<SourceRender, ToolResponse> {
        let owner = source_owner(source);
        let store = self.store_mut(true)?;
        let sha256 = store.put_object(wav)?;
        let count = store.reader().document_count(&owner, SOURCE_RENDERS)?;
        Ok(SourceRender {
            id: format!("source-render-{}", count + 1),
            source: source.to_owned(),
            sequence: sequence.map(|s| s as u32),
            layout,
            sound_source: REFERENCE_SOUND_SOURCE.into(),
            sample_rate: rendered.audio.sample_rate,
            content_samples: rendered.audio.content_samples,
            length_samples: rendered.audio.left.len() as u64,
            tail_truncated: rendered.audio.tail_truncated,
            format: crate::review::WavFormat::Float32,
            ports: rendered.receivers.iter().map(|r| r.port).collect(),
            gain_db: None,
            lead_in_samples: None,
            sha256,
        })
    }

    /// Re-renders a source render whose WAV a delivery left out and checks
    /// the recorded hash.
    fn regenerate_source_render(
        &self,
        bytes: &[u8],
        render: &SourceRender,
    ) -> Result<Vec<u8>, String> {
        let rendered = crate::render::render_midi_sequence(
            bytes,
            render.sequence.map(|s| s as usize),
            &render.layout,
            &reference_soundfont(),
            render.sample_rate,
        )
        .map_err(|e| e.to_string())?;
        let mut rendered = rendered;
        if let Some(gain) = render.gain_db {
            apply_gain(&mut rendered.audio, gain);
        }
        if let Some(samples) = render.lead_in_samples {
            prepend_silence(&mut rendered.audio, samples);
        }
        let wav = crate::render::wav_bytes_in(
            render.sample_rate,
            &rendered.audio.left,
            &rendered.audio.right,
            crate::review::WavFormat::Float32,
        );
        if sha256_hex(&wav) != render.sha256 {
            return Err(format!(
                "regenerated audio of `{}` differs from its recorded hash",
                render.id
            ));
        }
        Ok(wav)
    }

    /// WAVs of a bundle's source reviews: read from the delivery, or under
    /// `missing_renders` regenerated with a hash check or left absent.
    pub(super) fn incoming_source_reviews(
        &mut self,
        bundle: &ProjectBundle,
        delivery: &Path,
        missing: MissingRenders,
    ) -> Result<Vec<IncomingSourceRender>, ToolResponse> {
        let (Some(reviews), Some(source)) = (&bundle.source_reviews, &bundle.project.midi_source)
        else {
            return Ok(Vec::new());
        };
        // The same source reviewed here already: identical records merge,
        // different ones under the same ID would mix two histories.
        if Store::exists(&self.workspace) {
            let owner = source_owner(&reviews.source);
            let reader = self.store_ref()?.reader();
            let mut incoming: Vec<(&str, &str, Value)> = Vec::new();
            for render in &reviews.renders {
                incoming.push((SOURCE_RENDERS, &render.id, json!(render)));
            }
            for presentation in &reviews.presentations {
                incoming.push(("presentation", &presentation.id, json!(presentation)));
            }
            for feedback in &reviews.feedback {
                let collection = match feedback.state {
                    FeedbackState::Draft => DRAFTS,
                    FeedbackState::Delivered { .. } => "feedback",
                };
                incoming.push((collection, &feedback.id, json!(feedback)));
            }
            for (collection, id, value) in incoming {
                if let Some(existing) = reader.document::<Value>(&owner, collection, id)?
                    && existing != value
                {
                    return Err(ToolResponse::refused(
                        Code::DuplicateId,
                        "/source_reviews",
                        format!(
                            "this workspace already holds a different `{id}` for source {}; import into a fresh workspace",
                            reviews.source
                        ),
                    ));
                }
            }
        }
        let mut audio = Vec::new();
        for (i, render) in reviews.renders.iter().enumerate() {
            let location = format!("/source_reviews/renders/{i}");
            let wav = match std::fs::read(delivery.join(source_render_file(&render.id))) {
                Ok(wav) if sha256_hex(&wav) == render.sha256 => Some(wav),
                Ok(_) => {
                    return Err(ToolResponse::refused(
                        Code::MissingRenderAudio,
                        &location,
                        format!("{} differs from its recorded hash", render.id),
                    ));
                }
                Err(_) => match missing {
                    MissingRenders::Refuse => {
                        return Err(ToolResponse::refused(
                            Code::MissingRenderAudio,
                            "/missing_renders",
                            format!(
                                "source render `{}` has no WAV in {}; pass missing_renders=regenerate to render it again with a hash check, or without_audio to keep its record",
                                render.id, SOURCE_RENDER_FOLDER
                            ),
                        ));
                    }
                    MissingRenders::WithoutAudio => None,
                    MissingRenders::Regenerate => Some(
                        self.regenerate_source_render(&source.bytes, render)
                            .map_err(|e| {
                                ToolResponse::refused(Code::MissingRenderAudio, &location, e)
                            })?,
                    ),
                },
            };
            audio.push((render.clone(), wav));
        }
        Ok(audio)
    }
}

/// Scales a source render by `gain` after rendering. Returns the peak before.
pub(super) fn apply_gain(
    audio: &mut crate::render::RenderedAudio,
    gain: crate::mix::Tenths,
) -> f32 {
    let before = audio
        .left
        .iter()
        .chain(&audio.right)
        .fold(0.0f32, |peak, value| peak.max(value.abs()));
    let factor = 10f64.powf(gain.value() / 20.0) as f32;
    for sample in audio.left.iter_mut().chain(audio.right.iter_mut()) {
        *sample *= factor;
    }
    before
}

/// Samples of `ms` milliseconds of lead-in: the first sample at or after it.
pub(super) fn lead_in_samples(ms: u32, sample_rate: u32) -> u64 {
    (u64::from(ms) * u64::from(sample_rate)).div_ceil(1000)
}

/// Places `samples` of silence before the rendered content.
pub(super) fn prepend_silence(audio: &mut crate::render::RenderedAudio, samples: u64) {
    let silence = vec![0.0f32; samples as usize];
    audio.left.splice(0..0, silence.iter().copied());
    audio.right.splice(0..0, silence);
}

/// The total gain, in tenths of a decibel rounded down, that brings `peak`
/// (heard with `gain`) to full scale.
pub(super) fn gain_for_peak(peak: f32, gain: Option<crate::mix::Tenths>) -> f64 {
    let total = gain.map_or(0.0, |g| g.value()) - 20.0 * f64::from(peak).log10();
    (total * 10.0).floor() / 10.0
}

/// Full-scale use of a stored WAV: float samples above 1.0, or integer
/// samples at the extreme codes, where the encoding may have clamped.
pub(super) fn full_scale_use(bytes: &[u8]) -> Option<Value> {
    let wav = crate::wav::parse(bytes).ok()?;
    let width = usize::from(wav.bits / 8).max(1);
    let mut peak = 0.0f64;
    let mut over = 0usize;
    for sample in wav.data.chunks_exact(width) {
        let value = crate::wav::normalized(wav.format, sample).abs();
        peak = peak.max(value);
        let at_limit = match wav.format {
            crate::wav::Format::Float => value > 1.0,
            crate::wav::Format::Int => {
                value >= 1.0 - 1.0 / f64::from(1u32 << (wav.bits - 1).min(31))
            }
        };
        over += usize::from(at_limit);
    }
    (over > 0).then(|| match wav.format {
        crate::wav::Format::Float => json!({"sample_peak": peak, "samples_over_full_scale": over,
            "note": "float samples above full scale clip in browsers and players; the person may hear distortion that is not in the music"}),
        crate::wav::Format::Int => json!({"sample_peak": peak, "samples_at_full_scale": over,
            "note": "integer samples at the extreme codes may have been clamped by the encoding"}),
    })
}

/// A marked sample of a source render, located on its source clock.
pub(super) fn locate_mark(bytes: &[u8], render: &SourceRender, sample: u64) -> Value {
    let lead_in = render.lead_in_samples.unwrap_or(0);
    if sample < lead_in {
        return json!({"render_seconds": sample as f64 / f64::from(render.sample_rate), "in_lead_in": true,
            "seconds_before_content": (lead_in - sample) as f64 / f64::from(render.sample_rate)});
    }
    let mut position = crate::render::locate_source_sample(
        bytes,
        render.sequence.map(|s| s as usize),
        &render.layout,
        render.sample_rate,
        sample - lead_in,
    )
    .unwrap_or_else(|error| json!({"unresolved": error.to_string()}));
    if lead_in > 0 && position.get("render_seconds").is_some() {
        position["render_seconds"] = json!(sample as f64 / f64::from(render.sample_rate));
        position["lead_in_samples"] = json!(lead_in);
    }
    position
}

/// The listening records of a project's own source, if any were kept.
pub(super) fn stored_source_reviews(
    reader: &crate::store::Reader,
    info: &ProjectInfo,
) -> Result<Option<SourceReviews>, StoreError> {
    let Some(source) = &info.midi_source else {
        return Ok(None);
    };
    let owner = source_owner(&source.sha256);
    let renders: Vec<SourceRender> = reader.documents(&owner, SOURCE_RENDERS)?;
    if renders.is_empty() {
        return Ok(None);
    }
    Ok(Some(SourceReviews {
        source: source.sha256.clone(),
        renders,
        presentations: reader.documents(&owner, "presentation")?,
        feedback: [
            reader.documents::<Feedback>(&owner, "feedback")?,
            reader.documents::<Feedback>(&owner, DRAFTS)?,
        ]
        .concat(),
    }))
}

/// A refusal when a kept source render has no audio in this workspace.
pub(super) fn absent_source_audio(store: &Store, bundle: &ProjectBundle) -> Option<ToolResponse> {
    let render = bundle
        .source_reviews
        .iter()
        .flat_map(|r| &r.renders)
        .find(|r| !store.object_path(&r.sha256).is_file())?;
    Some(ToolResponse::refused(
        Code::MissingRenderAudio,
        "/project",
        format!(
            "source render `{}` of this project's source has no audio here; import its delivery with missing_renders=regenerate first",
            render.id
        ),
    ))
}

pub(super) fn delivered_summary(reviews: &SourceReviews) -> Value {
    json!({"source": reviews.source,
        "renders": reviews.renders.iter().map(|r| &r.id).collect::<Vec<_>>(),
        "presentations": reviews.presentations.len(), "feedback": reviews.feedback.len(),
        "location": format!("{SOURCE_RENDER_FOLDER}/<source render id>.wav"),
        "note": "listening on source-only renders of the project's original MIDI; these are not project renders and name no revision. import_project restores them with the source"})
}

pub(super) fn imported_summary(reviews: &SourceReviews, audio: &[IncomingSourceRender]) -> Value {
    json!({"source": reviews.source,
        "renders": reviews.renders.iter().map(|r| &r.id).collect::<Vec<_>>(),
        "without_audio": audio.iter().filter(|(_, wav)| wav.is_none()).map(|(r, _)| &r.id).collect::<Vec<_>>(),
        "presentations": reviews.presentations.len(), "feedback": reviews.feedback.len(),
        "next_action": "the source is loaded here; present_review and read_feedback take it as source, with no project or revision"})
}

/// Writes imported source reviews. Records already here are kept, so a
/// workspace that reviewed the same source does not lose or duplicate them.
pub(super) fn write_source_reviews(
    writer: &Writer,
    bundle: &ProjectBundle,
    name: &str,
) -> Result<(), StoreError> {
    let (Some(reviews), Some(source)) = (&bundle.source_reviews, &bundle.project.midi_source)
    else {
        return Ok(());
    };
    super::sources::register_source(writer, &source.sha256, source.bytes.len(), name)?;
    let owner = source_owner(&reviews.source);
    for render in &reviews.renders {
        if writer
            .document::<Value>(&owner, SOURCE_RENDERS, &render.id)?
            .is_none()
        {
            writer.put_document(&owner, SOURCE_RENDERS, &render.id, render)?;
        }
    }
    for presentation in &reviews.presentations {
        if writer
            .document::<Value>(&owner, "presentation", &presentation.id)?
            .is_none()
        {
            writer.put_document(&owner, "presentation", &presentation.id, presentation)?;
        }
    }
    for feedback in &reviews.feedback {
        let collection = if matches!(feedback.state, FeedbackState::Draft) {
            DRAFTS
        } else {
            "feedback"
        };
        if writer
            .document::<Value>(&owner, collection, &feedback.id)?
            .is_none()
        {
            writer.put_document(&owner, collection, &feedback.id, feedback)?;
        }
    }
    Ok(())
}
