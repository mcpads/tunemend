//! Renders, presentations to the person, and the feedback they deliver.

use super::*;

/// Rendered audio, the chip-instrument and clip reports, and the instruments used.
pub(super) type RevisionAudio = (
    crate::render::RenderedAudio,
    crate::render::SourceReports,
    crate::render::ChipInstruments,
);

impl Service {
    fn render_source_preview(&mut self, input: RenderPreviewInput) -> Handled {
        if input.project.is_some() || input.revision.is_some() || input.voices.is_some() {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/source",
                "source playback cannot select a project, revision or voices; it plays the retained channel stream",
            ));
        }
        let Some(destination) = &input.destination else {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/destination",
                "source playback requires a new destination folder",
            ));
        };
        let source = input.source.as_deref().expect("source branch");
        let bytes = self.midi_source_bytes(source)?;
        let sequence = input.sequence.map(|s| s as usize);
        let sequences = crate::render::type_2_sequences(&bytes);
        let selection_problem = match (sequences, sequence) {
            (Some(count), None) => Some(format!(
                "this SMF type 2 file holds {count} independent sequences; choose one with sequence (0–{})",
                count.saturating_sub(1)
            )),
            (Some(count), Some(selected)) if selected >= count => Some(format!(
                "sequence {selected} does not exist; this type 2 file holds {count} sequences"
            )),
            (None, Some(_)) => Some(
                "only SMF type 2 holds independent sequences; omit sequence for type 0/1 and RMID"
                    .into(),
            ),
            _ => None,
        };
        if let Some(problem) = selection_problem {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/sequence",
                problem,
            ));
        }
        if let Some(gain) = input.gain_db
            && !(crate::mix::MIN_GAIN_DB..=crate::mix::MAX_GAIN_DB).contains(&gain.value())
        {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/gain_db",
                "gain_db must be -120.0–12.0",
            ));
        }
        if input
            .lead_in_ms
            .is_some_and(|ms| !(1..=10_000).contains(&ms))
        {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/lead_in_ms",
                "lead_in_ms must be 1–10000",
            ));
        }
        let mut rendered = match crate::render::render_midi_sequence(
            &bytes,
            sequence,
            &input.layout,
            &reference_soundfont(),
            input.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE),
        ) {
            Ok(rendered) => rendered,
            Err(crate::render::RenderError::MidiEvents(diagnostics)) => {
                return done(ToolResponse::blocked(
                    "source playback cannot interpret these events",
                    diagnostics,
                ));
            }
            Err(error) => {
                return done(ToolResponse::refused(
                    Code::UnsupportedMidi,
                    "/source",
                    error.to_string(),
                ));
            }
        };
        let peak_before_gain = input
            .gain_db
            .map(|gain| source_review::apply_gain(&mut rendered.audio, gain));
        let lead_in = input.lead_in_ms.map(|ms| {
            let samples = source_review::lead_in_samples(ms, rendered.audio.sample_rate);
            source_review::prepend_silence(&mut rendered.audio, samples);
            (ms, samples)
        });
        let audio = &rendered.audio;
        let peak = audio
            .left
            .iter()
            .chain(&audio.right)
            .fold(0.0f32, |peak, value| peak.max(value.abs()));
        let over_full_scale = audio
            .left
            .iter()
            .chain(&audio.right)
            .filter(|v| v.abs() > 1.0)
            .count();
        let mut report = json!({
            "source": source, "source_sha256": sha256_hex(&bytes), "layout": input.layout,
            "sample_rate": audio.sample_rate, "content_samples": audio.content_samples,
            "length_samples": audio.left.len(), "tail_truncated": audio.tail_truncated,
            "format": "float32", "sample_peak": peak, "samples_over_full_scale": over_full_scale,
            "clamped_samples": 0, "sound_source": REFERENCE_SOUND_SOURCE,
            "playback": "single MIDI receiver; equal-tick events in track order; stacked key releases end the earliest held instance of that channel and key; excerpt preroll from zero",
            "transport_end": "release held notes and pedals at the selected end; decay up to 3 seconds",
            "source_modified": false, "editable_projection_required": false,
            "files": {"original": "original.bin", "audio": "preview.wav", "report": "render.json"}
        });
        // A single-receiver type 0/1 report keeps its earlier form and bytes.
        if rendered.receivers.len() > 1 {
            report["playback"] = json!(
                "independent MIDI receivers, one per port (port 0 before a track's first port event); each keeps its own channel, program, controller, pedal and note state; outputs sum in port order; equal-tick events in track order; stacked key releases end the earliest held instance of that channel and key on that receiver; excerpt preroll from zero"
            );
            report["transport_end"] = json!(
                "release held notes and pedals on every receiver at the selected end; decay of the sum up to 3 seconds"
            );
        }
        if rendered.receivers.len() > 1 || sequence.is_some() {
            report["receivers"] = json!(rendered.receivers);
        }
        if let Some(sequence) = sequence {
            report["sequence"] = json!({"index": sequence, "count": sequences,
                "clock": "this sequence's own tempo events from its tick 0; other sequences are not played or merged"});
        }
        if let (Some(gain), Some(before)) = (input.gain_db, peak_before_gain) {
            report["gain_db"] = json!(gain);
            report["sample_peak_before_gain"] = json!(before);
            report["gain"] = json!(
                "applied to every float sample after rendering and the tail check; the MIDI and its playback are unchanged"
            );
        }
        if let Some((ms, samples)) = lead_in {
            report["lead_in_ms"] = json!(ms);
            report["lead_in_samples"] = json!(samples);
            report["lead_in"] = json!(
                "silence placed before the first rendered sample; length_samples includes it and content_samples does not. Marks subtract it before mapping to source ticks; the MIDI is unchanged"
            );
        }
        let headroom = (over_full_scale > 0).then(|| json!({
            "samples_over_full_scale": over_full_scale, "sample_peak": peak,
            "suggested_gain_db": source_review::gain_for_peak(peak, input.gain_db),
            "note": "players, browsers and integer conversion clip samples above full scale; render again with gain_db at or below the suggestion (another job_id) to hear the original without clipping. The MIDI is not changed"}));
        let wav = crate::render::wav_bytes_in(
            audio.sample_rate,
            &audio.left,
            &audio.right,
            crate::review::WavFormat::Float32,
        );
        let kept = input.keep_for_review.then(|| wav.clone());
        let mut response = self.export_sound_files(
            &input.job_id,
            destination,
            vec![
                ("original.bin".into(), bytes),
                ("preview.wav".into(), wav),
                (
                    "render.json".into(),
                    serde_json::to_vec_pretty(&report).expect("serializes"),
                ),
            ],
        )?;
        response.summary =
            "rendered retained MIDI events; original bytes and unclamped float WAV exported".into();
        response.result["render"] = report;
        if let Some(headroom) = headroom {
            response.result["headroom"] = headroom;
        }
        let Some(wav) = kept else {
            return done(response);
        };
        let mut record =
            self.keep_source_render(source, sequence, input.layout, &rendered, &wav)?;
        record.gain_db = input.gain_db;
        record.lead_in_samples = lead_in.map(|(_, samples)| samples);
        response.summary = format!("{}; kept as {} for listening", response.summary, record.id);
        response.result["source_render"] = json!(record);
        response.result["review"] = json!(
            "present_review with this source and render ID; marks map back to source ticks and retained event IDs, not to a revision"
        );
        let owner = source_review::source_owner(source);
        commit(response, move |writer| {
            writer.put_document(&owner, source_review::SOURCE_RENDERS, &record.id, &record)
        })
    }
    /// Listener addresses belong to this process, not to the stored job result.
    pub(super) fn refresh_review_url(&self, response: &mut ToolResponse, project: &str) {
        let Some(id) = response
            .result
            .pointer("/presentation/id")
            .and_then(Value::as_str)
        else {
            return;
        };
        let url = self
            .review_base_url
            .as_ref()
            .map(|base| format!("{base}/review/{project}/{id}"));
        response.summary = match &url {
            Some(url) => format!("{id} is ready at {url}"),
            None => format!(
                "{id} is stored; start `tunemend review` in this workspace, then retry this call for the listening link"
            ),
        };
        response.result["url"] = json!(url);
    }
    /// The preview audio of a revision in a layout, as render_preview makes it.
    pub(super) fn revision_audio(
        &mut self,
        info: &ProjectInfo,
        revision: &Revision,
        layout: &RenderLayout,
        voices: Option<&[String]>,
        sample_rate: u32,
        capture: Option<&crate::render::StemCapture>,
    ) -> Result<RevisionAudio, ToolResponse> {
        let composition = &revision.composition;
        let instruments = self.chip_instruments(&composition.mix, voices, "/mix")?;
        let clips = self.clip_sources(&composition.mix, voices, "/mix")?;
        let rendered = if let Some(source) = &info.midi_source {
            let imported = crate::midi::read_smf_preserving(&source.bytes)
                .map_err(|d| ToolResponse::blocked("invalid preserved MIDI", d))?;
            let plan = MidiExport {
                id: "preview".into(),
                revision: revision.id.clone(),
                timing: None,
                assignments: imported.assignments,
                sections_as_markers: false,
            };
            let bytes = crate::midi::write_preserving_declared(
                source,
                info.timebase,
                composition,
                &plan,
                true,
            )
            .map_err(|d| {
                ToolResponse::blocked("the revision cannot be preserved for preview", d)
            })?;
            let blockers = crate::midi::preview_blockers(&bytes);
            if !blockers.is_empty() {
                return Err(ToolResponse::blocked(
                    "MIDI export preserves these events, but reference playback cannot interpret them",
                    blockers,
                ));
            }
            crate::render::render_midi_with_sources(
                &bytes,
                layout,
                voices,
                &reference_soundfont(),
                sample_rate,
                &composition.mix,
                &instruments,
                &clips,
                capture,
            )
        } else {
            crate::render::render_with_sources(
                info.timebase,
                composition,
                layout,
                voices,
                &reference_soundfont(),
                sample_rate,
                &instruments,
                &clips,
                capture,
            )
        };
        let (audio, chip_reports) = match rendered {
            Ok(rendered) => rendered,
            Err(
                crate::render::RenderError::Instrument(diagnostic)
                | crate::render::RenderError::Clip(diagnostic),
            ) => {
                return Err(ToolResponse::blocked(
                    format!("the render was not made: {}", diagnostic.message),
                    vec![diagnostic],
                ));
            }
            Err(error) => {
                return Err(ToolResponse::refused(
                    Code::InvalidRenderLayout,
                    "",
                    error.to_string(),
                ));
            }
        };
        Ok((audio, chip_reports, instruments))
    }
    pub(super) fn render_preview(&mut self, input: RenderPreviewInput) -> Handled {
        if input.source.is_some() {
            return self.render_source_preview(input);
        }
        if input.sequence.is_some() {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/sequence",
                "sequence selects a type 2 sequence of a retained MIDI source; project renders have none",
            ));
        }
        if input.lead_in_ms.is_some() {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/lead_in_ms",
                "lead_in_ms adds silence before a source render; project renders follow the composition's own timeline",
            ));
        }
        if input.gain_db.is_some() {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/gain_db",
                "gain_db is the source playback output gain; project renders take headroom from the mixer's master_volume_db",
            ));
        }
        if input.keep_for_review {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/keep_for_review",
                "project renders are always kept; keep_for_review applies to source playback",
            ));
        }
        let Some(project) = input.project.as_deref() else {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/project",
                "choose a project or source",
            ));
        };
        if input.destination.is_some() {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/destination",
                "destination is only used for source playback; export_project delivers project renders",
            ));
        }
        let (info, revision) = self.revision(project, input.revision.as_deref())?;
        let composition = &revision.composition;
        if let Some(problem) = layout_problem(composition, &input.layout) {
            return done(ToolResponse::refused(
                Code::InvalidRenderLayout,
                "/layout",
                problem,
            ));
        }
        let sample_rate = input.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE);
        if !(16_000..=192_000).contains(&sample_rate) {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/sample_rate",
                "sample rate must be 16000–192000",
            ));
        }
        let (audio, reports, instruments) = match self.revision_audio(
            &info,
            &revision,
            &input.layout,
            input.voices.as_deref(),
            sample_rate,
            None,
        ) {
            Ok(rendered) => rendered,
            Err(response) => return done(response),
        };
        let wav = wav_bytes(&audio);
        let store = self.store_ref()?;
        let object = store.put_object(&wav)?;
        let id = format!("render-{}", next_render_number(&store.reader(), project)?);
        let render = Render {
            id: id.clone(),
            revision: revision.id.clone(),
            layout: input.layout,
            voices: input.voices,
            sound_source: REFERENCE_SOUND_SOURCE.into(),
            played_by: played_by(composition, &instruments, &reports.clips),
            sample_rate,
            length_samples: audio.left.len() as u64,
            tail_truncated: audio.tail_truncated,
            sha256: Some(object.clone()),
            region: None,
            part: None,
            pre_master: false,
            format: Default::default(),
            tail_cut: false,
        };
        let record = RenderRecord {
            render: render.clone(),
            object: object.clone(),
        };
        let content = content_seconds(info.timebase, composition, &input.layout);
        let total_seconds = audio.left.len() as f64 / f64::from(sample_rate);
        let mut timing = json!({"content_seconds": rational_seconds(content), "total_seconds": total_seconds,
            "tail_seconds": total_seconds - rational_seconds(content), "tail_truncated": audio.tail_truncated,
            "max_tail_seconds": MAX_TAIL_SECONDS});
        add_loop_timing(&mut timing, info.timebase, composition, &input.layout);
        let path = store.object_path(&object);
        let levels =
            super::batch::render_levels(&audio.left, &audio.right, crate::review::WavFormat::Pcm16);
        let warning =
            super::batch::level_warning(&levels, crate::review::WavFormat::Pcm16, "/levels");
        let mut summary = format!(
            "{id}: {} of {} ({total_seconds:.2} s)",
            layout_label(&input.layout),
            revision.id
        );
        if warning.is_some() {
            summary.push_str(&format!(
                "; {} samples clamped by 16-bit encoding (see warnings)",
                levels["clamped_samples"]
            ));
        }
        let mut response = ToolResponse::ok(
            summary,
            json!({"render": render, "timing": timing, "levels": levels, "limits": render_limits(!instruments.is_empty(), !composition.mix.clips.is_empty())}),
        );
        if let Some(warning) = warning {
            response.result["warnings"] = json!([warning]);
        }
        if !composition.mix.is_empty() {
            response.result["mix"] = json!({"applied": true, "voices_heard": composition.voices.iter()
                .filter(|v| record.render.voices.as_ref().is_none_or(|selected| selected.contains(&v.id)) && composition.mix.audible(&v.id))
                .map(|v| &v.id).collect::<Vec<_>>(),
                "scope": "volume, pan, mute, solo, buses, sends, master gain and their automation, applied after the reference synthesizer"});
            if !composition.mix.effects.is_empty() {
                response.result["mix"]["effects"] = json!({
                    "chains": composition.mix.effects.iter().map(|c| json!({"target": c.target,
                        "processing": c.effects.iter().filter(|e| !e.bypass).map(|e| &e.id).collect::<Vec<_>>()})).collect::<Vec<_>>(),
                    "note": "insert effects ran offline in chain order; a sidechain source not heard in this render keys nothing"});
            }
        }
        if !instruments.is_empty() {
            response.result["instruments"] = json!({
                "voices": reports.instruments,
                "not_heard": instruments.keys().filter(|v| !composition.mix.audible(v)).collect::<Vec<_>>(),
                "note": "each instrument voice played its notes on its own chip model at the native rate, converted to the render rate before its mixer strip; the listed controls were not applied"});
        }
        if !composition.mix.clips.is_empty() {
            let selected = |voice: &str| {
                record
                    .render
                    .voices
                    .as_ref()
                    .is_none_or(|v| v.iter().any(|x| x == voice))
            };
            response.result["clips"] = json!({
                "played": reports.clips,
                "not_heard": composition.mix.clips.iter()
                    .filter(|c| selected(&c.voice) && !composition.mix.audible(&c.voice))
                    .map(|c| &c.id).collect::<Vec<_>>(),
                "note": "each clip's source range was mapped to stereo, resampled to the render rate when the rates differ, scaled by its gain and fades, and added to its voice's dry signal before the voice's effects, fader, sends and output"});
        }
        response.artifacts.push(Artifact {
            kind: "wav".into(),
            path: path.display().to_string(),
            sha256: object,
        });
        let project = input.project.expect("checked project");
        commit(response, move |writer| {
            writer.put_document(&project, "render", &id, &record)
        })
    }
    pub(super) fn present_review(&mut self, input: PresentReviewInput) -> Handled {
        let owner = self.review_owner(input.project.as_deref(), input.source.as_deref())?;
        if input.renders.is_empty() {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/renders",
                "present at least one render",
            ));
        }
        let mut warnings = Vec::new();
        for (i, item) in input.renders.iter().enumerate() {
            let location = format!("/renders/{i}/render");
            match self.listened_render(&owner, &item.render)? {
                None => {
                    return done(ToolResponse::refused(
                        Code::UnknownReference,
                        &location,
                        match source_review::reviewed_source(&owner) {
                            Some(_) => format!(
                                "source render `{}` does not exist; render_preview with keep_for_review keeps one",
                                item.render
                            ),
                            None => format!("render `{}` does not exist", item.render),
                        },
                    ));
                }
                Some(render) if !self.store_ref()?.object_path(&render.object).is_file() => {
                    return done(match render.source {
                        Some(_) => ToolResponse::refused(
                            Code::MissingRenderAudio,
                            &location,
                            format!(
                                "source render `{}` was imported without audio; import the delivery again with missing_renders=regenerate",
                                item.render
                            ),
                        ),
                        None => absent_render_audio(&owner, &item.render, &location),
                    });
                }
                Some(render) => {
                    let path = self.store_ref()?.object_path(&render.object);
                    if let Some(mut warning) = std::fs::read(path)
                        .ok()
                        .and_then(|wav| source_review::full_scale_use(&wav))
                    {
                        warning["render"] = json!(item.render);
                        warning["remedy"] = json!(match render.source {
                            Some(_) =>
                                "render the source again with gain_db for headroom and present that render",
                            None =>
                                "lower master_volume_db (or the loud voices) in a new revision and render again",
                        });
                        warnings.push(warning);
                    }
                }
            }
        }
        let reader = self.store_ref()?.reader();
        let presentations: Vec<Presentation> = reader.documents(&owner, "presentation")?;
        let latest = presentations.last().map(|p| p.id.clone());
        if let Some(expected) = &input.replaces
            && latest.as_ref() != Some(expected)
        {
            return done(ToolResponse::refused(
                Code::StaleRevision,
                "/replaces",
                format!("the latest presentation is {latest:?}, not `{expected}`"),
            ));
        }
        let id = format!("review-{}", presentations.len() + 1);
        let presentation = Presentation {
            id: id.clone(),
            title: input.title,
            renders: input.renders,
        };
        let stored = presentation.clone();
        let project = owner;
        let mut response = ToolResponse::ok(
            "presentation stored",
            json!({
                "presentation": presentation,
                "guidance_for_person": "Listen, mark moments or ranges that sound wrong, then choose Send to AI. Comments are optional.",
            }),
        );
        if !warnings.is_empty() {
            response.result["warnings"] = json!(warnings);
        }
        self.refresh_review_url(&mut response, &project);
        commit(response, move |writer| {
            writer.put_document(&project, "presentation", &stored.id, &stored)
        })
    }
    pub(super) fn read_feedback(
        &mut self,
        input: ReadFeedbackInput,
    ) -> Result<ToolResponse, ToolResponse> {
        let owner = self.review_owner(input.project.as_deref(), input.source.as_deref())?;
        let source_bytes = match source_review::reviewed_source(&owner) {
            Some(source) => Some(self.midi_source_bytes(source)?),
            None => None,
        };
        let (timebase, held) = match source_bytes {
            Some(_) => (Timebase::Effect, vec![]),
            None => {
                let info = self.project(&owner)?.0;
                // Retained never-released notes sound in project renders too.
                let held = match &info.midi_source {
                    Some(source) => crate::midi::held_retained_notes(source)
                        .map_err(|d| ToolResponse::blocked("invalid preserved MIDI", d))?,
                    None => vec![],
                };
                (info.timebase, held)
            }
        };
        let store = self.store_ref()?;
        let reader = store.reader();
        let after = input.after_sequence.unwrap_or(0);
        let delivered: Vec<Feedback> = reader.documents(&owner, "feedback")?;
        let presentations: Vec<Presentation> = reader.documents(&owner, "presentation")?;
        let mut items = Vec::new();
        let mut cursor = after;
        for feedback in delivered {
            let FeedbackState::Delivered { sequence } = feedback.state else {
                continue;
            };
            if sequence <= after {
                continue;
            }
            cursor = cursor.max(sequence);
            let mut marks = Vec::new();
            for mark in &feedback.marks {
                let label = feedback
                    .presentation
                    .as_ref()
                    .and_then(|p| presentations.iter().find(|x| &x.id == p))
                    .and_then(|p| p.renders.iter().find(|r| r.render == mark.render))
                    .map(|r| r.label.clone());
                if let Some(bytes) = &source_bytes {
                    let Some(render) = reader.document::<crate::review::SourceRender>(
                        &owner,
                        source_review::SOURCE_RENDERS,
                        &mark.render,
                    )?
                    else {
                        continue;
                    };
                    marks.push(json!({
                        "render": mark.render, "mark": mark.label, "render_label": label,
                        "source": render.source, "sequence": render.sequence,
                        "start": source_review::locate_mark(bytes, &render, mark.start_sample),
                        "end": mark.end_sample.map(|s| source_review::locate_mark(bytes, &render, s)),
                    }));
                    continue;
                }
                let Some(record) =
                    reader.document::<RenderRecord>(&owner, "render", &mark.render)?
                else {
                    continue;
                };
                let Some(revision) = reader.revision(&owner, &record.render.revision)? else {
                    continue;
                };
                marks.push(json!({
                    "render": mark.render, "mark": mark.label, "render_label": label, "revision": revision.id, "voice": mark.voice,
                    "start": resolve_sample(timebase, &revision.composition, &held, &record.render, mark.start_sample),
                    "end": mark.end_sample.map(|s| resolve_sample(timebase, &revision.composition, &held, &record.render, s)),
                }));
            }
            items.push(json!({"feedback": feedback.id, "sequence": sequence, "presentation": feedback.presentation,
                "text": feedback.text, "marks": marks}));
        }
        let drafts: Vec<Feedback> = reader.documents(&owner, DRAFTS)?;
        let pending = drafts
            .iter()
            .filter(|d| !d.marks.is_empty() || d.text.as_deref().is_some_and(|t| !t.is_empty()))
            .count();
        let mut response = ToolResponse::ok(
            format!(
                "{} new feedback; {pending} undelivered drafts on the screen",
                items.len()
            ),
            json!({"feedback": items, "next_cursor": cursor, "undelivered_drafts": pending,
                "note": "Marks are observations, not edit permissions or approval. A mark in a later loop pass is about that pass."}),
        );
        if source_bytes.is_some() {
            response.result["note"] = json!(
                "Marks are observations, not edit permissions or approval. Source marks name source ticks and retained events held on each receiver (inspect_midi IDs), never a revision or voice; relating them to a project revision is your interpretation through the shared source tick."
            );
        }
        Ok(response)
    }
    pub(super) fn record_feedback(&mut self, input: RecordFeedbackInput) -> Handled {
        let owner = self.review_owner(input.project.as_deref(), input.source.as_deref())?;
        let feedback = Feedback {
            id: String::new(),
            presentation: input.presentation,
            state: FeedbackState::Draft,
            marks: input.marks,
            text: input.text,
        };
        self.deliver(&owner, feedback)
    }
    /// Freezes feedback as the next delivery after checking its references.
    pub(super) fn deliver(&mut self, project: &str, mut feedback: Feedback) -> Handled {
        if feedback.marks.is_empty() && feedback.text.as_deref().is_none_or(str::is_empty) {
            return done(ToolResponse::refused(
                Code::EmptyDeliveredFeedback,
                "",
                "there is nothing to deliver yet",
            ));
        }
        let presentation = match &feedback.presentation {
            Some(id) => Some(
                self.store_ref()?
                    .reader()
                    .document::<Presentation>(project, "presentation", id)?
                    .ok_or_else(|| {
                        ToolResponse::refused(
                            Code::UnknownReference,
                            "/presentation",
                            format!("presentation `{id}` does not exist"),
                        )
                    })?,
            ),
            None => None,
        };
        for (i, mark) in feedback.marks.iter().enumerate() {
            let Some(record) = self.listened_render(project, &mark.render)? else {
                return done(ToolResponse::refused(
                    Code::UnknownReference,
                    &format!("/marks/{i}/render"),
                    format!("render `{}` does not exist", mark.render),
                ));
            };
            if record.source.is_some() && mark.voice.is_some() {
                return done(ToolResponse::refused(
                    Code::InvalidRange,
                    &format!("/marks/{i}/voice"),
                    "source renders have no voices; describe the part in text instead",
                ));
            }
            if presentation
                .as_ref()
                .is_some_and(|p| !p.renders.iter().any(|r| r.render == mark.render))
            {
                return done(ToolResponse::refused(
                    Code::UnknownReference,
                    &format!("/marks/{i}/render"),
                    "the render is not part of the presentation",
                ));
            }
            let length = record.length_samples;
            if mark.start_sample >= length
                || mark
                    .end_sample
                    .is_some_and(|e| e <= mark.start_sample || e > length)
            {
                return done(ToolResponse::refused(
                    Code::MarkOutsideRender,
                    &format!("/marks/{i}"),
                    format!(
                        "the render has {length} samples ({:.3} s at {} Hz); a mark must lie within it. Do not move or clamp it: record the words without this mark and ask the person to mark the spot again",
                        length as f64 / f64::from(record.sample_rate),
                        record.sample_rate
                    ),
                ));
            }
        }
        let delivered: Vec<Feedback> = self.store_ref()?.reader().documents(project, "feedback")?;
        let sequence = delivered.len() as u64 + 1;
        feedback.id = format!("feedback-{sequence}");
        feedback.state = FeedbackState::Delivered { sequence };
        let response = ToolResponse::ok(
            format!("{} delivered", feedback.id),
            json!({"feedback": feedback}),
        );
        let project = project.to_owned();
        commit(response, move |writer| {
            writer.put_document(&project, "feedback", &feedback.id, &feedback)
        })
    }
    /// Saves the person's unfinished marks for a presentation.
    pub fn save_draft(
        &mut self,
        project: &str,
        presentation: &str,
        marks: Vec<Mark>,
        text: Option<String>,
    ) -> ToolResponse {
        let result: Result<ToolResponse, ToolResponse> = (|| {
            self.check_review_owner(project)?;
            let store = self.store_ref()?;
            if store
                .reader()
                .document::<Presentation>(project, "presentation", presentation)?
                .is_none()
            {
                return Ok(ToolResponse::refused(
                    Code::UnknownReference,
                    "/presentation",
                    "unknown presentation",
                ));
            }
            let draft = Feedback {
                id: format!("draft-{presentation}"),
                presentation: Some(presentation.into()),
                state: FeedbackState::Draft,
                marks,
                text,
            };
            store.write(|writer| writer.put_document(project, DRAFTS, &draft.id, &draft))?;
            Ok(ToolResponse::ok("draft saved", json!({"draft": draft})))
        })();
        result.unwrap_or_else(|response| response)
    }
    pub fn draft(&mut self, project: &str, presentation: &str) -> Option<Feedback> {
        let store = self.store_mut(false).ok()?;
        store
            .reader()
            .document(project, DRAFTS, &format!("draft-{presentation}"))
            .ok()?
    }
    /// Delivers the saved draft of a presentation. `delivery_id` makes a
    /// repeated click or network retry return the first delivery.
    pub fn deliver_draft(
        &mut self,
        project: &str,
        presentation: &str,
        delivery_id: &str,
    ) -> ToolResponse {
        let job_id = format!("screen-delivery-{delivery_id}");
        let request_hash = sha256_hex(format!("{project}/{presentation}").as_bytes());
        if let Some(replay) = self.replay("deliver_draft", &job_id, &[&request_hash]) {
            return replay;
        }
        let Some(draft) = self.draft(project, presentation) else {
            return ToolResponse::refused(
                Code::EmptyDeliveredFeedback,
                "",
                "there is nothing to deliver yet",
            );
        };
        let (mut response, pending) = match self.deliver(project, draft) {
            Ok(handled) => handled,
            Err(response) => (response, None),
        };
        response.job_id = Some(job_id.clone());
        let Some(pending) = pending else {
            return response;
        };
        let record = serde_json::to_value(&response).expect("serializes");
        let draft_id = format!("draft-{presentation}");
        let project = project.to_owned();
        let committed = self.store_mut(false).and_then(|store| {
            store.write(|writer| {
                if writer.job(&job_id)?.is_some() {
                    return Ok(());
                }
                pending(writer)?;
                writer.delete_document(&project, DRAFTS, &draft_id)?;
                writer.record_job(&job_id, "deliver_draft", &request_hash, &record)
            })
        });
        match committed {
            Ok(()) => response,
            Err(error) => ToolResponse::error(error.0),
        }
    }
    /// Presentation, its renders and audio paths for the listening screen.
    pub fn review_page(
        &mut self,
        project: &str,
        presentation: &str,
    ) -> Result<Value, ToolResponse> {
        if source_review::reviewed_source(project).is_some() {
            return self.source_review_page(project, presentation);
        }
        let (info, _) = self.project(project)?;
        let store = self.store_ref()?;
        let reader = store.reader();
        let presentation: Presentation = reader
            .document(project, "presentation", presentation)?
            .ok_or_else(|| {
                ToolResponse::refused(
                    Code::UnknownReference,
                    "/presentation",
                    "unknown presentation",
                )
            })?;
        let mut items = Vec::new();
        for item in &presentation.renders {
            let Some(record) = reader.document::<RenderRecord>(project, "render", &item.render)?
            else {
                continue;
            };
            let revision = reader.revision(project, &record.render.revision)?;
            let voices: Vec<Value> = revision
                .map(|r| {
                    r.composition
                        .voices
                        .iter()
                        .map(|v| json!({"id": v.id, "role": v.role}))
                        .collect()
                })
                .unwrap_or_default();
            items.push(json!({"render": item.render, "label": item.label, "sample_rate": record.render.sample_rate,
                "length_samples": record.render.length_samples, "object": record.object, "voices": voices}));
        }
        let latest: Vec<Presentation> = reader.documents(project, "presentation")?;
        let is_latest = latest.last().is_some_and(|p| p.id == presentation.id);
        Ok(
            json!({"project": project, "kind": match info.timebase { Timebase::Music { .. } => "music", Timebase::Effect => "effect" },
            "presentation": presentation.id, "title": presentation.title, "items": items, "is_latest": is_latest}),
        )
    }
    pub fn object_file(&mut self, hash: &str) -> Option<PathBuf> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let store = self.store_mut(false).ok()?;
        let path = store.object_path(hash);
        path.is_file().then_some(path)
    }
}

/// Why a layout cannot be rendered from a composition, if it cannot.
pub(super) fn layout_problem(
    composition: &Composition,
    layout: &RenderLayout,
) -> Option<&'static str> {
    match *layout {
        RenderLayout::Full { loop_passes: 0 } => Some("play the content at least once"),
        RenderLayout::Full { loop_passes }
            if loop_passes > 1 && composition.playback.loop_start.is_none() =>
        {
            Some("only a composition with a loop can repeat")
        }
        RenderLayout::Excerpt { start, end } if start >= end || end > composition.playback.end => {
            Some("an excerpt is a non-empty range within the playback end")
        }
        _ => None,
    }
}

/// What a render's sound stands for, by what played in it: the reference
/// synthesizer alone, with chip instruments, or with audio clips.
pub(super) fn render_limits(instruments: bool, clips: bool) -> &'static str {
    match (instruments, clips) {
        (_, true) => {
            "voices use reference waveforms unless an instrument plays them; audio clips play their retained WAV sources through their voices' strips"
        }
        (true, false) => {
            "voices without an instrument use reference waveforms; instrument voices use Tunemend's chip models, not the game's sound driver or hardware audio"
        }
        (false, false) => {
            "reference waveforms show structure and timing, not the target chip's sound"
        }
    }
}

/// Adds the intro and loop lengths to a full render's timing when the
/// revision loops.
pub(super) fn add_loop_timing(
    timing: &mut Value,
    timebase: Timebase,
    composition: &Composition,
    layout: &RenderLayout,
) {
    if let (RenderLayout::Full { loop_passes }, Some(loop_start)) =
        (layout, composition.playback.loop_start)
    {
        let intro = seconds_at(
            timebase,
            composition,
            Rational::from_integer(loop_start.into()),
        );
        let end = seconds_at(
            timebase,
            composition,
            Rational::from_integer(composition.playback.end.into()),
        );
        timing["intro_seconds"] = json!(rational_seconds(intro));
        timing["loop_seconds"] = json!(rational_seconds(end - intro));
        timing["loop_passes"] = json!(loop_passes);
    }
}

pub(super) fn layout_label(layout: &RenderLayout) -> String {
    match layout {
        RenderLayout::Full { loop_passes: 1 } => "whole piece".into(),
        RenderLayout::Full { loop_passes } => format!("whole piece with {loop_passes} loop passes"),
        RenderLayout::Excerpt { start, end } => format!("excerpt {start}–{end}"),
    }
}

pub(super) fn resolve_sample(
    timebase: Timebase,
    composition: &Composition,
    held: &[crate::midi::HeldRetainedNote],
    render: &Render,
    sample: u64,
) -> Value {
    let render_seconds = Rational::new(i128::from(sample), i128::from(render.sample_rate));
    let ids: Vec<String> = composition.voices.iter().map(|v| v.id.clone()).collect();
    let part_voices = render
        .part
        .as_ref()
        .filter(|part| **part != crate::review::RenderPart::Mixdown)
        .map(|part| crate::mix::part_voices(&composition.mix, &ids, part));
    let shown = part_voices.as_deref().or(render.voices.as_deref());
    match locate_in_render(timebase, composition, &render.layout, render_seconds) {
        RenderPosition::Tail {
            seconds_after_content,
        } => json!({
            "render_seconds": rational_seconds(render_seconds),
            "in_tail": true,
            "seconds_after_content": rational_seconds(seconds_after_content),
            "sounding_at_end": sounding(composition, held, composition.playback.end.saturating_sub(1), shown),
        }),
        RenderPosition::Content {
            source_position,
            source_seconds,
            loop_pass,
        } => {
            let at = source_position.floor().to_integer() as u64;
            json!({
                "render_seconds": rational_seconds(render_seconds),
                "source_position": rational_text(source_position),
                "source_seconds": rational_seconds(source_seconds),
                "position": position_label(timebase, composition, source_position),
                "loop_pass": loop_pass,
                "sounding": sounding(composition, held, at, shown),
            })
        }
    }
}

/// Notes sounding at `at`, then retained never-released source notes, which
/// have no note id and sound until the content end releases them.
pub(super) fn sounding(
    composition: &Composition,
    held: &[crate::midi::HeldRetainedNote],
    at: u64,
    voices: Option<&[String]>,
) -> Vec<Value> {
    let notes = composition
        .notes
        .iter()
        .filter(|n| n.start <= at && at < n.start + n.duration)
        .filter(|n| voices.is_none_or(|v| v.contains(&n.voice)))
        .map(|n: &Note| json!({"id": n.id, "voice": n.voice, "pitch": n.pitch, "start": n.start, "duration": n.duration}));
    let retained = held
        .iter()
        .filter(|h| h.start <= at && at < composition.playback.end)
        .filter(|h| voices.is_none_or(|v| v.contains(&h.voice)))
        .map(|h| json!({"source_event": h.source_event, "voice": h.voice, "pitch": h.pitch, "start": h.start,
            "retained": "never_released", "sounds_until": "content end"}));
    notes.chain(retained).collect()
}

/// What played besides the reference synthesizer: the instruments of heard
/// voices and the clips that wrote samples, in that order.
pub(super) fn played_by(
    composition: &Composition,
    instruments: &crate::render::ChipInstruments,
    clips: &[Value],
) -> Vec<crate::review::PlayedSource> {
    use crate::voice_instruments::VoiceInstrument;
    let mut out = Vec::new();
    for (voice, chip) in instruments {
        if !composition.mix.audible(voice) {
            continue;
        }
        let instrument = &composition.mix.instruments[chip.assignment].instrument;
        let source = match instrument {
            VoiceInstrument::SnesBrr(i) => i.bank.clone().or_else(|| i.plan.clone()),
            VoiceInstrument::Fm(i) => i.bank.clone(),
        };
        out.push(crate::review::PlayedSource {
            voice: voice.clone(),
            kind: instrument.kind().into(),
            source,
            clip: None,
        });
    }
    for report in clips {
        let wrote = report["placements"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p["samples_written"].as_u64() > Some(0)));
        if wrote {
            out.push(crate::review::PlayedSource {
                voice: report["voice"].as_str().unwrap_or_default().into(),
                kind: "audio_clip".into(),
                source: report["source"].as_str().map(Into::into),
                clip: report["clip"].as_str().map(Into::into),
            });
        }
    }
    out
}
