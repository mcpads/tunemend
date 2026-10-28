//! Creating, importing and continuing projects, and their briefs.

use super::*;

impl Service {
    pub(super) fn create_project(&mut self, input: CreateProjectInput) -> Handled {
        check_project_id(&input.project)?;
        if let Timebase::Music { ppq: 0 } = input.timebase {
            return Err(ToolResponse::refused(
                Code::InvalidTimebase,
                "/timebase/ppq",
                "PPQ must be positive",
            ));
        }
        let mut tool_notes = Vec::new();
        let composition = input.composition.unwrap_or_else(|| {
            tool_notes
                .push("started empty; set voices, tempo and length with edit_project".to_owned());
            empty_composition(input.timebase)
        });
        let mut diagnostics = Vec::new();
        crate::composition::check_composition(
            input.timebase,
            &composition,
            &mut crate::diagnostic::Report::new("/composition", &mut diagnostics),
        );
        if !diagnostics.is_empty() {
            return done(ToolResponse::blocked(
                "the initial composition breaks the contract",
                diagnostics,
            ));
        }
        let store = self.store_mut(true)?;
        if store.reader().project(&input.project)?.is_some() {
            return done(ToolResponse::refused(
                Code::DuplicateId,
                "/project",
                format!("project `{}` already exists", input.project),
            ));
        }
        let info = ProjectInfo {
            name: input.project.clone(),
            timebase: input.timebase,
            head_revision: "r1".into(),
            brief: input.brief,
            midi_source: None,
        };
        let revision = Revision {
            id: "r1".into(),
            parent: None,
            edit_request: None,
            composition,
        };
        let response = ToolResponse::ok(
            format!("created `{}` at revision r1", info.name),
            json!({"project": info.name, "revision": "r1", "brief": info.brief, "tool_notes": tool_notes}),
        );
        commit(response, move |writer| {
            writer.insert_project(&info, &json!({}))?;
            writer.insert_revision(&info.name, &revision)
        })
    }
    pub(super) fn compare_midi(&mut self, input: CompareMidiInput) -> Handled {
        let reference = self.midi_file_or_source(&input.reference, "/reference")?;
        let candidate = self.midi_file_or_source(&input.candidate, "/candidate")?;
        let result = crate::midi::compare_midi_page(
            &reference,
            &candidate,
            input.offset.unwrap_or(0),
            input
                .limit
                .map_or(crate::midi::DEFAULT_DIFFERENCE_PAGE, |l| l as usize),
        );
        done(ToolResponse::ok(
            "MIDI comparison; no files changed",
            result,
        ))
    }
    /// A workspace-relative (or absolute) file, else a loaded source ID.
    fn midi_file_or_source(&mut self, name: &str, location: &str) -> Result<Vec<u8>, ToolResponse> {
        let path = self.workspace.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.midi_source_bytes(name).map_err(|_| {
                    ToolResponse::refused(
                        Code::UnknownReference,
                        location,
                        format!(
                            "`{name}` is neither a file at {} nor a loaded MIDI source ID",
                            path.display()
                        ),
                    )
                })
            }
            Err(error) => Err(ToolResponse::error(format!("cannot read {name}: {error}"))),
        }
    }
    pub(super) fn analyze_midi(&mut self, input: AnalyzeMidiInput) -> Handled {
        let bytes = std::fs::read(self.workspace.join(&input.path))
            .map_err(|e| ToolResponse::error(format!("cannot read {}: {e}", input.path)))?;
        done(ToolResponse::ok(
            "MIDI inspection; no project created",
            crate::midi::analyze_smf(&bytes),
        ))
    }
    pub(super) fn import_midi(&mut self, input: ImportMidiInput) -> Handled {
        check_project_id(&input.project)?;
        let (bytes, source_name) = match (&input.path, &input.source) {
            (Some(path), None) => (
                std::fs::read(self.workspace.join(path))
                    .map_err(|e| ToolResponse::error(format!("cannot read {path}: {e}")))?,
                path.clone(),
            ),
            (None, Some(source)) => (self.midi_source_bytes(source)?, source.clone()),
            _ => {
                return done(ToolResponse::refused(
                    Code::InvalidRange,
                    "/source",
                    "supply exactly one of path or loaded source",
                ));
            }
        };
        let source_sha256 = sha256_hex(&bytes);
        if input
            .expected_sha256
            .as_ref()
            .is_some_and(|expected| expected != &source_sha256)
        {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/expected_sha256",
                "the MIDI changed since inspection; inspect the current file again",
            ));
        }
        if let Some(crate::midi::DeclaredEnd::Fixed { ticks: 0, .. }) = input.declared_ends {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/declared_ends/ticks",
                "a fixed declared end needs a positive length in ticks",
            ));
        }
        if let Some(rule) = &input.declared_ends {
            let targets = rule.apply_to();
            if targets.is_empty() || (1..targets.len()).any(|i| targets[..i].contains(&targets[i]))
            {
                return done(ToolResponse::refused(
                    Code::InvalidRange,
                    "/declared_ends/apply_to",
                    "apply_to names never_released, zero_length or both, each once",
                ));
            }
        }
        let imported = match crate::midi::read_smf_declaring(&bytes, input.declared_ends.clone()) {
            Ok(imported) => imported,
            Err(diagnostics) => {
                let mut response = ToolResponse::blocked(
                    format!(
                        "{} cannot be safely projected for editing; use analyze_midi for details",
                        source_name
                    ),
                    diagnostics,
                );
                response.result = crate::midi::analyze_smf(&bytes);
                return done(response);
            }
        };
        let store = self.store_mut(true)?;
        if store.reader().project(&input.project)?.is_some() {
            return done(ToolResponse::refused(
                Code::DuplicateId,
                "/project",
                format!("project `{}` already exists", input.project),
            ));
        }
        let original = store.put_object(&bytes)?;
        let info = ProjectInfo {
            name: input.project.clone(),
            timebase: imported.timebase,
            head_revision: "r1".into(),
            brief: input.brief,
            midi_source: Some(crate::midi::MidiSource {
                sha256: source_sha256,
                bytes: bytes.clone(),
                declared_ends: input.declared_ends.clone(),
                legacy_declared_ends: Some(false),
            }),
        };
        let revision = Revision {
            id: "r1".into(),
            parent: None,
            edit_request: None,
            composition: imported.composition.clone(),
        };
        let settings = json!({
            "midi_assignments": imported.assignments,
            "source": {"path": input.path, "sha256": original},
        });
        let mut response = ToolResponse::ok(
            format!(
                "imported {} as `{}` r1 with {} voices and {} notes",
                source_name,
                info.name,
                revision.composition.voices.len(),
                revision.composition.notes.len()
            ),
            json!({
                "project": info.name, "revision": "r1", "timebase": info.timebase,
                "voices": imported.assignments, "normalizations": imported.normalizations,
                "source_sha256": original, "retained": imported.retained,
                "editing": crate::midi::analyze_smf(&bytes)["editing"],
            }),
        );
        // The editing summary describes the projection this import made,
        // including any declared ends, not the default one.
        response.result["editing"]["retained"] = json!(imported.retained);
        response.result["editing"]["projection_normalizations"] = json!(imported.normalizations);
        if let Some(rule) = input.declared_ends {
            let notes = crate::midi::declared_notes(&imported);
            response.summary.push_str(&format!(
                "; {} notes have a declared end by the selected rule",
                notes.len()
            ));
            let left: Vec<_> = imported
                .retained
                .iter()
                .filter(|r| r.message.contains("selected"))
                .collect();
            response.result["declared_ends"] = json!({"rule": rule, "apply_to": rule.apply_to(), "notes": notes,
                "not_completed": left,
                "note": "the original bytes are unchanged; an edit that changes one of these notes writes its release, and export_project with write_declared_ends writes every declared end; both report it"});
        }
        response.artifacts.push(Artifact {
            kind: "original_midi".into(),
            path: store.object_path(&original).display().to_string(),
            sha256: original,
        });
        commit(response, move |writer| {
            writer.insert_project(&info, &settings)?;
            writer.insert_revision(&info.name, &revision)
        })
    }
    pub(super) fn update_brief(&mut self, input: UpdateBriefInput) -> Handled {
        let (mut info, settings) = self.project(&input.project)?;
        info.brief = Some(input.brief);
        let response = ToolResponse::ok("brief updated", json!({"brief": info.brief}));
        commit(response, move |writer| {
            writer.update_project(&info, &settings)
        })
    }
    pub(super) fn set_head(&mut self, input: SetHeadInput) -> Handled {
        let (mut info, settings) = self.project(&input.project)?;
        let (_, revision) = self.revision(&input.project, Some(&input.revision))?;
        let previous = std::mem::replace(&mut info.head_revision, revision.id.clone());
        let response = ToolResponse::ok(
            format!("head is now {} (was {previous})", revision.id),
            json!({"project": input.project, "head_revision": revision.id, "previous_head": previous}),
        );
        commit(response, move |writer| {
            writer.update_project(&info, &settings)
        })
    }
    pub(super) fn import_project(&mut self, input: ImportProjectInput) -> Handled {
        let path = self.workspace.join(&input.path);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ToolResponse::error(format!("cannot read {}: {e}", path.display())))?;
        let mut bundle: ProjectBundle = match serde_json::from_str(&text) {
            Ok(bundle) => bundle,
            Err(error) => {
                return done(ToolResponse::refused(
                    Code::InvalidRange,
                    "",
                    format!("not a project bundle: {error}"),
                ));
            }
        };
        if let Some(source) = &mut bundle.project.midi_source
            && let Some(root) = bundle.revisions.iter().find(|r| r.parent.is_none())
        {
            crate::midi::resolve_declared_projection(source, &root.composition);
        }
        let problems = check_project_bundle(&bundle);
        if !problems.is_empty() {
            return done(ToolResponse::blocked(
                "the bundle does not pass the contract checks; nothing was imported",
                problems,
            ));
        }
        check_project_id(&bundle.project.name)?;
        let parent = path.parent().unwrap_or(Path::new("."));
        let incoming = self.incoming_data(&bundle, &parent.join("objects"))?;
        if !incoming.records.is_empty() {
            let store = self.store_mut(true)?;
            for bytes in &incoming.objects {
                store.put_object(bytes)?;
            }
        }
        // Regeneration reads the carried records before the import commits them.
        self.staged = incoming
            .records
            .iter()
            .map(|r| ((r.collection.clone(), r.id.clone()), r.record.clone()))
            .collect();
        let gathered = self.import_render_audio(&bundle, &parent.join("renders"), &input);
        self.staged.clear();
        let (audio, regenerated) = gathered?;
        let source_audio = self.incoming_source_reviews(&bundle, parent, input.missing_renders)?;
        let store = self.store_mut(true)?;
        if store.reader().project(&bundle.project.name)?.is_some() {
            return done(ToolResponse::refused(
                Code::DuplicateId,
                "/project",
                format!("project `{}` already exists here", bundle.project.name),
            ));
        }
        let mut records = Vec::new();
        let mut without_audio = Vec::new();
        for (render, bytes) in audio {
            let object = match bytes {
                Some(bytes) => store.put_object(&bytes)?,
                None => {
                    without_audio.push(render.id.clone());
                    render
                        .sha256
                        .clone()
                        .expect("records kept without audio have a recorded hash")
                }
            };
            records.push(RenderRecord { render, object });
        }
        let source_reviews = bundle
            .source_reviews
            .as_ref()
            .map(|reviews| super::source_review::imported_summary(reviews, &source_audio));
        // The retained original becomes a loaded source with its own ID, so
        // source tools (export_midi_source, inspect_midi, render_preview) work.
        let original = bundle
            .project
            .midi_source
            .as_ref()
            .filter(|source| sha256_hex(&source.bytes) == source.sha256);
        if let Some(source) = original {
            store.put_object(&source.bytes)?;
        }
        for (_, wav) in &source_audio {
            if let Some(wav) = wav {
                store.put_object(wav)?;
            }
        }
        let name = bundle.project.name.clone();
        let response = ToolResponse::ok(
            format!(
                "imported `{name}` with {} revisions, head {}",
                bundle.revisions.len(),
                bundle.project.head_revision
            ),
            json!({"project": name, "head_revision": bundle.project.head_revision, "revisions": bundle.revisions.len(),
                "renders": records.len(), "regenerated_renders": regenerated.iter().map(|(id, _)| id).collect::<Vec<_>>(),
                "regenerated_checks": regenerated.iter().map(|(id, check)| json!({"render": id, "checked": check})).collect::<Vec<_>>(),
                "feedback": bundle.feedback.len()}),
        );
        let mut response = response;
        let registered = original.map(|source| (source.sha256.clone(), source.bytes.len()));
        if let Some((source, _)) = &registered {
            response.result["midi_source"] = json!({"source": source, "loaded": true,
                "note": "the project's retained original MIDI is a loaded source here under its original ID; export_midi_source returns its exact bytes and inspect_midi shows its events"});
        }
        if let Some(summary) = source_reviews {
            response.result["source_reviews"] = summary;
        }
        if !without_audio.is_empty() {
            response.result["renders_without_audio"] = json!(without_audio);
            response.result["renders_without_audio_note"] = json!(
                "these records keep their recorded hash but have no audio here; present_review, measure_audio, compare_audio and export_project refuse them with missing_render_audio until render_batch regenerate_renders restores them"
            );
        }
        if !incoming.records.is_empty() {
            response.result["referenced_data"] = json!(
                incoming
                    .records
                    .iter()
                    .map(|r| json!({"collection": r.collection, "id": r.id,
                        "objects": referenced_data::carried_objects(r).unwrap_or_default()}))
                    .collect::<Vec<_>>()
            );
        }
        commit(response, move |writer| {
            writer.insert_project(
                &bundle.project,
                &json!({"midi_assignments": bundle.midi_exports.last().map(|e| &e.assignments)}),
            )?;
            for revision in &bundle.revisions {
                writer.insert_revision(&name, revision)?;
            }
            for record in &records {
                writer.put_document(&name, "render", &record.render.id, record)?;
            }
            for presentation in &bundle.presentations {
                writer.put_document(&name, "presentation", &presentation.id, presentation)?;
            }
            for feedback in &bundle.feedback {
                let collection = if matches!(feedback.state, FeedbackState::Draft) {
                    DRAFTS
                } else {
                    "feedback"
                };
                writer.put_document(&name, collection, &feedback.id, feedback)?;
            }
            for export in &bundle.midi_exports {
                writer.put_document(&name, "midi_export", &export.id, export)?;
            }
            if let Some((source, length)) = &registered {
                super::sources::register_source(writer, source, *length, &format!("{name}.mid"))?;
            }
            super::source_review::write_source_reviews(writer, &bundle, &format!("{name}.mid"))?;
            // Records already retained here keep their stored form, such as a source name.
            for record in &incoming.records {
                if writer
                    .document::<Value>("", &record.collection, &record.id)?
                    .is_none()
                {
                    writer.put_document("", &record.collection, &record.id, &record.record)?;
                }
            }
            Ok(())
        })
    }
    /// Render audio of a bundle: read from `folder`, regenerated and checked
    /// against the recorded hash or length, or (`without_audio`) left absent.
    fn import_render_audio(
        &mut self,
        bundle: &ProjectBundle,
        folder: &Path,
        input: &ImportProjectInput,
    ) -> Result<(RenderFiles, Vec<RegeneratedRender>), ToolResponse> {
        let missing_renders = input.missing_renders;
        let listed: Vec<&str> = input
            .regenerate_renders
            .iter()
            .flatten()
            .map(String::as_str)
            .collect();
        if input.regenerate_renders.is_some() && missing_renders == MissingRenders::Regenerate {
            return Err(ToolResponse::refused(
                Code::InvalidRange,
                "/regenerate_renders",
                "missing_renders regenerate already renders every missing WAV again; list renders with refuse or without_audio",
            ));
        }
        if let Some((i, id)) = listed
            .iter()
            .enumerate()
            .find(|(_, id)| !bundle.renders.iter().any(|r| r.id == **id))
        {
            return Err(ToolResponse::refused(
                Code::UnknownReference,
                &format!("/regenerate_renders/{i}"),
                format!("render `{id}` is not in the bundle"),
            ));
        }
        let file_of = |render: &Render| folder.join(format!("{}.wav", render.id));
        let left_missing: Vec<&Render> = bundle
            .renders
            .iter()
            .filter(|r| !listed.contains(&r.id.as_str()) && !file_of(r).exists())
            .collect();
        match missing_renders {
            MissingRenders::Refuse => {
                if let Some(first) = left_missing.first() {
                    let file = file_of(first);
                    let e = std::fs::read(&file)
                        .err()
                        .map_or_else(|| "unreadable".to_owned(), |e| e.to_string());
                    return Err(ToolResponse::refused(
                        Code::MissingRenderAudio,
                        "/missing_renders",
                        format!(
                            "{} of {} render WAVs are missing, first {} ({e}); a delivery exported with render_audio listed, presented or none leaves them out on purpose. Pass missing_renders=without_audio to keep them as records without audio (add regenerate_renders to restore only the renders you need), or missing_renders=regenerate to render every missing one again; regenerated audio is checked against its recorded hash",
                            left_missing.len(),
                            bundle.renders.len(),
                            file.display()
                        ),
                    ));
                }
            }
            MissingRenders::WithoutAudio => {
                if let Some(render) = left_missing.iter().find(|r| r.sha256.is_none()) {
                    return Err(ToolResponse::refused(
                        Code::MissingRenderAudio,
                        "/regenerate_renders",
                        format!(
                            "render `{}` has no WAV and the bundle recorded no hash for it, so it cannot be kept without audio; add it to regenerate_renders",
                            render.id
                        ),
                    ));
                }
                let presented: Vec<&str> = left_missing
                    .iter()
                    .map(|r| r.id.as_str())
                    .filter(|id| {
                        bundle
                            .presentations
                            .iter()
                            .any(|p| p.renders.iter().any(|item| item.render == *id))
                    })
                    .collect();
                if !presented.is_empty() {
                    return Err(ToolResponse::refused(
                        Code::MissingRenderAudio,
                        "/regenerate_renders",
                        format!(
                            "presented renders need their audio for the listening screen; add {} to regenerate_renders",
                            presented
                                .iter()
                                .map(|id| format!("`{id}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                }
            }
            MissingRenders::Regenerate => {}
        }
        let mut audio = Vec::new();
        let mut regenerated = Vec::new();
        for render in &bundle.renders {
            let file = file_of(render);
            let bytes = match std::fs::read(&file) {
                Ok(bytes) => bytes,
                Err(_)
                    if missing_renders == MissingRenders::WithoutAudio
                        && !listed.contains(&render.id.as_str()) =>
                {
                    audio.push((render.clone(), None));
                    continue;
                }
                Err(_) => {
                    let revision = bundle
                        .revisions
                        .iter()
                        .find(|r| r.id == render.revision)
                        .expect("bundle checks resolve render revisions");
                    let (bytes, length) = self.record_audio(&bundle.project, revision, render)?;
                    let matches = match &render.sha256 {
                        Some(hash) => sha256_hex(&bytes) == *hash,
                        None => length == render.length_samples,
                    };
                    if !matches {
                        return Err(ToolResponse::refused(
                            Code::RevisionContentMismatch,
                            "/missing_renders",
                            format!(
                                "regenerated {} differs from the recorded render (this renderer is not the one that made it); nothing was imported",
                                render.id
                            ),
                        ));
                    }
                    regenerated.push((
                        render.id.clone(),
                        if render.sha256.is_some() {
                            "sha256 equals the recorded render"
                        } else {
                            "length only: the bundle recorded no hash for this render"
                        },
                    ));
                    bytes
                }
            };
            if render
                .sha256
                .as_ref()
                .is_some_and(|h| *h != sha256_hex(&bytes))
            {
                return Err(ToolResponse::refused(
                    Code::RevisionContentMismatch,
                    "/path",
                    format!(
                        "render audio {} differs from its recorded hash",
                        file.display()
                    ),
                ));
            }
            audio.push((render.clone(), Some(bytes)));
        }
        Ok((audio, regenerated))
    }
}

/// Render records with their WAV bytes, `None` when kept without audio.
type RenderFiles = Vec<(Render, Option<Vec<u8>>)>;

pub(super) fn check_project_id(id: &str) -> Result<(), ToolResponse> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-');
    if valid {
        Ok(())
    } else {
        Err(ToolResponse::refused(
            Code::InvalidRange,
            "/project",
            "project ids use lowercase letters, digits and hyphens",
        ))
    }
}

pub(super) fn empty_composition(timebase: Timebase) -> Composition {
    match timebase {
        Timebase::Music { ppq } => Composition {
            tempo_map: vec![TempoChange {
                tick: 0,
                microseconds_per_quarter: 500_000,
            }],
            time_signatures: vec![TimeSignature {
                tick: 0,
                numerator: 4,
                denominator: 4,
            }],
            key_signatures: vec![],
            markers: vec![],
            voices: vec![],
            notes: vec![],
            control_lanes: vec![],
            mix: Default::default(),
            sections: vec![],
            playback: Playback {
                end: u64::from(ppq) * 16,
                loop_start: None,
            },
        },
        Timebase::Effect => Composition {
            tempo_map: vec![],
            time_signatures: vec![],
            key_signatures: vec![],
            markers: vec![],
            voices: vec![],
            notes: vec![],
            control_lanes: vec![],
            mix: Default::default(),
            sections: vec![],
            playback: Playback {
                end: 500_000,
                loop_start: None,
            },
        },
    }
}

/// A regenerated render's ID and the check its audio passed.
type RegeneratedRender = (String, &'static str);
