//! Writing results out: MIDI and project bundles, and hand-off to a sound chip target.

use super::*;

impl Service {
    pub(super) fn convert_for_target(&mut self, input: ConvertForTargetInput) -> Handled {
        let (info, revision) = self.revision(&input.project, input.revision.as_deref())?;
        if info.midi_source.is_some() {
            return done(ToolResponse::refused(
                Code::TargetRefused,
                "/midi_source",
                "the frame-IR adapter cannot preserve the complete source MIDI event stream; use export_project for lossless MIDI delivery. Target encoding requires a representation and readback verifier for all source semantics",
            ));
        }
        let relative = Path::new(&input.destination);
        if !matches!(relative.components().next(), Some(std::path::Component::Normal(first)) if first == "out" || first == "work")
            || relative.components().count() < 2
        {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/destination",
                "the converter writes only to a new folder under out/ or work/",
            ));
        }
        let destination = self.workspace.join(relative);
        if destination.exists() {
            return done(ToolResponse::refused(
                Code::DuplicateId,
                "/destination",
                format!("{} already exists; choose a new folder", input.destination),
            ));
        }
        let converter = self.workspace.join(&input.converter);
        if !converter.is_file() {
            return Err(ToolResponse::error(format!(
                "no external frame converter executable at {}",
                converter.display()
            )));
        }
        let source_name = format!("{}@{}", input.project, revision.id);
        let composition_text = serde_json::to_string(&revision.composition).expect("serializes");
        let timeline = match crate::target::build_timeline(
            &source_name,
            (&source_name, &composition_text),
            info.timebase,
            &revision.composition,
            &input.mapping,
        ) {
            Ok(timeline) => timeline,
            Err(diagnostics) => {
                return done(ToolResponse::blocked(
                    "the revision does not fit the mapping; nothing was converted",
                    diagnostics,
                ));
            }
        };
        if !timeline.losses.is_empty() {
            let mut response = ToolResponse::refused(
                Code::TargetRefused,
                "/mapping",
                "the target mapping would lose information; no converter was run. Change the mapping or extend the target representation",
            );
            response.result = json!({"losses": timeline.losses});
            return done(response);
        }
        let profile = input
            .capability_profile
            .as_ref()
            .map(|p| self.workspace.join(p));
        let steps = crate::target::run_converter(
            &self.workspace,
            &converter,
            &timeline,
            profile.as_deref(),
            &destination,
        )
        .map_err(|e| ToolResponse::error(format!("the converter could not run: {e}")))?;
        let identity = |path: &Path| -> Value {
            std::fs::read(path).map(|bytes| json!({"path": path.display().to_string(), "sha256": sha256_hex(&bytes), "size": bytes.len()})).unwrap_or(Value::Null)
        };
        let report = json!({
            "project": input.project, "revision": revision.id,
            "converter": {"executable": identity(&converter), "revision": crate::target::converter_revision(&converter)},
            "mapping_sha256": sha256_hex(serde_json::to_string(&input.mapping).expect("serializes").as_bytes()),
            "timeline": identity(&destination.join("timeline.json")), "frames": identity(&destination.join("frames/frames.json")),
            "capability_profile": profile.as_deref().map(identity),
            "capability_status": std::fs::read_to_string(destination.join("capability/report.json")).ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|report| report.pointer("/result/status").or_else(|| report.get("status")).cloned()),
            "losses": timeline.losses, "steps": steps,
            "not_checked": "the consumer game's encoding, insertion and playback belong to the consumer project",
        });
        std::fs::write(
            destination.join("report.json"),
            serde_json::to_vec_pretty(&report).expect("serializes"),
        )
        .map_err(|e| ToolResponse::error(e.to_string()))?;
        let refused: Vec<Diagnostic> = steps
            .iter()
            .filter(|s| s.exit_code != 0)
            .map(|s| Diagnostic {
                code: if s.exit_code == 2 {
                    Code::TargetRefused
                } else {
                    Code::TargetFailed
                },
                location: format!("/steps/{}", s.step),
                message: match (&s.reason, s.stderr.is_empty()) {
                    (Some(reason), _) => reason.clone(),
                    (None, false) => s.stderr.clone(),
                    (None, true) => {
                        format!("{} exited with {}; see {}", s.step, s.exit_code, s.output)
                    }
                },
            })
            .collect();
        let status_summary = if refused.is_empty() {
            format!(
                "converted {} of `{}` for the target into {}",
                revision.id, input.project, input.destination
            )
        } else {
            format!(
                "the converter refused or failed {} steps; see {}/report.json",
                refused.len(),
                input.destination
            )
        };
        let mut response = if refused.is_empty() {
            ToolResponse::ok(status_summary, report.clone())
        } else {
            ToolResponse {
                result: report.clone(),
                ..ToolResponse::blocked(status_summary, refused)
            }
        };
        for (kind, name) in [
            ("timeline", "timeline.json"),
            ("frames", "frames/frames.json"),
            ("report", "report.json"),
        ] {
            let path = destination.join(name);
            if let Ok(bytes) = std::fs::read(&path) {
                response.artifacts.push(Artifact {
                    kind: kind.into(),
                    path: path.display().to_string(),
                    sha256: sha256_hex(&bytes),
                });
            }
        }
        let id = format!(
            "conversion-{}",
            self.store_ref()?
                .reader()
                .document_count(&input.project, "conversion")?
                + 1
        );
        let project = input.project.clone();
        commit(response, move |writer| {
            writer.put_document(&project, "conversion", &id, &report)
        })
    }
    pub(super) fn export_project(&mut self, input: ExportProjectInput) -> Handled {
        let (info, settings) = self.project(&input.project)?;
        let (_, revision) = self.revision(&input.project, input.revision.as_deref())?;
        let destination = self.workspace.join(&input.destination);
        if destination.exists()
            && std::fs::read_dir(&destination)
                .map(|mut d| d.next().is_some())
                .unwrap_or(true)
        {
            return done(ToolResponse::refused(
                Code::DuplicateId,
                "/destination",
                format!(
                    "{} already has files; choose a new folder",
                    input.destination
                ),
            ));
        }
        let assignments = input
            .assignments
            .unwrap_or_else(|| default_assignments(&revision.composition, &settings));
        let timing = match info.timebase {
            Timebase::Effect => Some(input.timing.unwrap_or(MidiTiming {
                ppq: 1000,
                microseconds_per_quarter: 1_000_000,
            })),
            Timebase::Music { .. } => input.timing,
        };
        let revisions = self.store_ref()?.reader().revisions(&input.project)?;
        let (references, objects) = self.referenced_data(&revisions)?;
        let store = self.store_ref()?;
        let reader = store.reader();
        let id = format!(
            "midi-{}",
            reader.document_count(&input.project, "midi_export")? + 1
        );
        let plan = MidiExport {
            id: id.clone(),
            revision: revision.id.clone(),
            timing,
            assignments,
            sections_as_markers: input.sections_as_markers,
        };
        if input.sections_as_markers && info.midi_source.is_some() {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/sections_as_markers",
                "source-backed MIDI keeps its original markers and bytes; sections stay in the project bundle",
            ));
        }
        let written = match input.sections_as_markers {
            true => with_section_markers(&revision.composition),
            false => revision.composition.clone(),
        };
        let declared_ends = match &info.midi_source {
            Some(source) if input.write_declared_ends => {
                match crate::midi::declared_end_changes(source, &revision.composition, true) {
                    Ok(written) => Some(written),
                    Err(diagnostics) => {
                        return done(ToolResponse::blocked(
                            "the MIDI file was not written",
                            diagnostics,
                        ));
                    }
                }
            }
            _ => None,
        };
        let midi = match info.midi_source.as_ref().map_or_else(
            || write_smf(info.timebase, &written, &plan),
            |source| {
                crate::midi::write_preserving_declared(
                    source,
                    info.timebase,
                    &revision.composition,
                    &plan,
                    input.write_declared_ends,
                )
            },
        ) {
            Ok(midi) => midi,
            Err(diagnostics) => {
                return done(ToolResponse::blocked(
                    "the MIDI file was not written",
                    diagnostics,
                ));
            }
        };
        let renders: Vec<RenderRecord> = reader.documents(&input.project, "render")?;
        let mut bundle = ProjectBundle {
            project: info.clone(),
            revisions,
            // The store object is the WAV's SHA-256; recording it lets an import
            // verify audio it regenerates.
            renders: renders
                .iter()
                .map(|r| Render {
                    sha256: Some(r.object.clone()),
                    ..r.render.clone()
                })
                .collect(),
            presentations: reader.documents(&input.project, "presentation")?,
            feedback: [
                reader.documents::<Feedback>(&input.project, "feedback")?,
                reader.documents::<Feedback>(&input.project, DRAFTS)?,
            ]
            .concat(),
            midi_exports: reader.documents(&input.project, "midi_export")?,
            references,
            source_reviews: super::source_review::stored_source_reviews(&reader, &info)?,
        };
        bundle.midi_exports.push(plan.clone());
        let problems = check_project_bundle(&bundle);
        if !problems.is_empty() {
            return done(ToolResponse::blocked(
                "the stored project does not pass the bundle checks",
                problems,
            ));
        }
        let presented: std::collections::BTreeSet<&str> = bundle
            .presentations
            .iter()
            .flat_map(|p| p.renders.iter().map(|r| r.render.as_str()))
            .collect();
        let listed: Vec<&str> = input
            .render_ids
            .iter()
            .flatten()
            .map(String::as_str)
            .collect();
        let selection_problem = match (input.render_audio, &input.render_ids) {
            (RenderAudio::All, Some(_)) => Some((
                Code::InvalidRange,
                "/render_ids",
                "render_audio all already includes every render; use listed for exactly these, or presented or none to add them".to_owned(),
            )),
            (RenderAudio::Listed, None) => Some((
                Code::InvalidRange,
                "/render_ids",
                "render_audio listed needs render_ids".to_owned(),
            )),
            _ => listed
                .iter()
                .enumerate()
                .find(|(_, id)| !renders.iter().any(|r| r.render.id == **id))
                .map(|(i, id)| {
                    (
                        Code::UnknownReference,
                        "/render_ids",
                        format!("render_ids/{i}: render `{id}` does not exist"),
                    )
                }),
        };
        if let Some((code, location, message)) = selection_problem {
            return done(ToolResponse::refused(code, location, message));
        }
        let included: Vec<&RenderRecord> = renders
            .iter()
            .filter(|r| {
                listed.contains(&r.render.id.as_str())
                    || match input.render_audio {
                        RenderAudio::All => true,
                        RenderAudio::Presented => presented.contains(r.render.id.as_str()),
                        RenderAudio::None | RenderAudio::Listed => false,
                    }
            })
            .collect();
        let omitted: Vec<&str> = renders
            .iter()
            .filter(|r| !included.iter().any(|i| i.render.id == r.render.id))
            .map(|r| r.render.id.as_str())
            .collect();
        let absent: Vec<&str> = included
            .iter()
            .filter(|r| !r.audio_present(store))
            .map(|r| r.render.id.as_str())
            .collect();
        if let Some(first) = absent.first() {
            let mut response = absent_render_audio(
                &input.project,
                first,
                if listed.contains(first) {
                    "/render_ids"
                } else {
                    "/render_audio"
                },
            );
            if absent.len() > 1 {
                response.diagnostics[0].message.push_str(&format!(
                    ". Also without audio: {}; regenerate them too or leave them out with render_audio listed, presented or none",
                    absent[1..].join(", ")
                ));
            }
            return done(response);
        }
        if input.render_records == RenderRecords::Delivered {
            let referenced = bundle
                .presentations
                .iter()
                .flat_map(|p| {
                    p.renders
                        .iter()
                        .map(move |r| (r.render.as_str(), format!("presentation `{}`", p.id)))
                })
                .chain(bundle.feedback.iter().flat_map(|f| {
                    f.marks
                        .iter()
                        .map(move |m| (m.render.as_str(), format!("feedback `{}`", f.id)))
                }))
                .find(|(render, _)| omitted.contains(render));
            if let Some((render, by)) = referenced {
                return done(ToolResponse::refused(
                    Code::UnknownReference,
                    "/render_records",
                    format!(
                        "{by} refers to `{render}`, whose WAV is not delivered; add it to render_ids, or keep render_records all so the record stays in the bundle"
                    ),
                ));
            }
            bundle.renders.retain(|r| !omitted.contains(&r.id.as_str()));
        }

        if let Some(problem) = super::source_review::absent_source_audio(store, &bundle) {
            return done(problem);
        }
        let copies = if input.named_copies {
            named_copies(&included)
        } else {
            Vec::new()
        };
        let staging = destination.with_file_name(format!(
            ".{}.partial-{}",
            destination
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("export"),
            std::process::id()
        ));
        let write_all = || -> std::io::Result<Vec<(String, PathBuf, Vec<u8>)>> {
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(staging.join("renders"))?;
            let mut files = vec![
                (
                    "midi".to_owned(),
                    PathBuf::from(format!("{}.mid", input.project)),
                    midi.clone(),
                ),
                (
                    "project".to_owned(),
                    PathBuf::from(format!("{}.project.json", input.project)),
                    serde_json::to_vec_pretty(&bundle).expect("serializes"),
                ),
            ];
            for record in &included {
                let bytes = std::fs::read(store.object_path(&record.object))?;
                files.push(("wav".into(), render_file(&record.render.id), bytes));
            }
            for render in bundle.source_reviews.iter().flat_map(|r| &r.renders) {
                let bytes = std::fs::read(store.object_path(&render.sha256))?;
                let path = super::source_review::source_render_file(&render.id);
                files.push(("source_wav".into(), path, bytes));
            }
            for (path, id) in &copies {
                let bytes = files
                    .iter()
                    .find(|(kind, file, _)| kind == "wav" && *file == render_file(id))
                    .map(|(_, _, bytes)| bytes.clone())
                    .expect("every copy names a delivered render");
                files.push((NAMED_COPY.into(), path.clone(), bytes));
            }
            let carry_objects = input.referenced_data == ReferencedData::Include;
            if carry_objects && !objects.is_empty() {
                std::fs::create_dir_all(staging.join("objects"))?;
                for (hash, bytes) in &objects {
                    files.push((
                        "object".into(),
                        PathBuf::from("objects").join(hash),
                        bytes.clone(),
                    ));
                }
            }
            let mut report = export_report(&info, &revision, &plan, &files, (&included, &copies));
            if let Some(written) = &declared_ends {
                report["declared_ends_written"] = json!({"notes": written,
                    "scope": "write_declared_ends: every declared-end note's release written at its projected end; the delivered MIDI differs from the original at these events"});
            }
            if !bundle.references.is_empty() {
                report["referenced_data"] = json!({
                    "records": bundle.references.iter().map(|r| json!({"collection": r.collection, "id": r.id})).collect::<Vec<_>>(),
                    "objects": objects.keys().collect::<Vec<_>>(),
                    "objects_written": carry_objects,
                    "location": "objects/<sha256>, exact bytes",
                    "note": "records are carried in the bundle's references; import_project verifies every record and object by hash and needs omitted objects retained in its workspace"});
            }
            files.push((
                "report".into(),
                PathBuf::from("report.json"),
                serde_json::to_vec_pretty(&report).expect("serializes"),
            ));
            for (_, relative, bytes) in &files {
                let path = staging.join(relative);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, bytes)?;
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if destination.exists() {
                std::fs::remove_dir(&destination)?;
            }
            std::fs::rename(&staging, &destination)?;
            Ok(files)
        };
        let files = write_all().map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging);
            ToolResponse::error(format!("export failed: {e}"))
        })?;
        let mut response = ToolResponse::ok(
            format!(
                "exported {} of `{}` to {}",
                revision.id, input.project, input.destination
            ),
            json!({"midi_export": plan, "report": destination.join("report.json"),
            "render_audio": input.render_audio, "renders_omitted": omitted,
            "renders_note": (!omitted.is_empty()).then_some(match input.render_records {
                RenderRecords::All => "records stay in the bundle; import_project with missing_renders=regenerate renders them again and checks their hashes",
                RenderRecords::Delivered => "render_records delivered: the omitted renders' records are not in the bundle, so it imports without regeneration",
            })}),
        );
        if let Some(reviews) = &bundle.source_reviews {
            response.result["source_reviews"] = super::source_review::delivered_summary(reviews);
        }
        if let Some(written) = &declared_ends {
            response.result["declared_ends_written"] = json!(written);
        }
        if input.named_copies {
            response.result["named_copies"] = json!({
                "files": copies.iter().map(|(path, id)| {
                    let render = included.iter().find(|r| &r.render.id == id).map(|r| &r.render);
                    let role = match render.and_then(|r| r.part.as_ref()) {
                        None | Some(crate::review::RenderPart::Mixdown) => "mixdown",
                        Some(_) => "stem",
                    };
                    json!({"path": path, "render": id, "copy_of": render_file(id), "role": role,
                        "revision": render.map(|r| &r.revision),
                        "delivered_revision": render.is_some_and(|r| r.revision == revision.id)})
                }).collect::<Vec<_>>(),
                "note": "byte-identical copies under readable names for people; import_project reads renders/<render id>.wav only, and report.json lists both",
            });
        }
        for (kind, relative, bytes) in files {
            response.artifacts.push(Artifact {
                kind,
                path: destination.join(relative).display().to_string(),
                sha256: sha256_hex(&bytes),
            });
        }
        let project = input.project;
        commit(response, move |writer| {
            writer.put_document(&project, "midi_export", &id, &plan)
        })
    }
}

/// What a delivered render WAV holds, so its file needs no lookup in the
/// bundle: the render, its revision and layout, and when set its region,
/// part, stem policy and tail, with the format, rate and length.
fn describe_render_file(entry: &mut Value, render: &Render) {
    entry["render"] = json!(render.id);
    entry["revision"] = json!(render.revision);
    entry["layout"] = json!(render.layout);
    if let Some(region) = &render.region {
        entry["region"] = json!(region);
    }
    if let Some(part) = &render.part {
        entry["part"] = json!(part);
    }
    if render.pre_master {
        entry["pre_master"] = json!(true);
    }
    if render.tail_cut {
        entry["tail_cut"] = json!(true);
    }
    if let Some(voices) = &render.voices {
        entry["voices"] = json!(voices);
    }
    entry["format"] = json!(render.format);
    entry["sample_rate"] = json!(render.sample_rate);
    entry["length_samples"] = json!(render.length_samples);
}

/// The composition with a `marker` meta event at each section's start,
/// carrying the section id, after markers already at that position.
fn with_section_markers(composition: &Composition) -> Composition {
    let mut written = composition.clone();
    written.markers.extend(
        composition
            .sections
            .iter()
            .map(|s| crate::composition::Marker {
                at: s.start,
                kind: crate::composition::MarkerKind::Marker,
                text: s.id.clone(),
            }),
    );
    written.markers.sort_by_key(|m| m.at);
    written
}

/// Imported track and channel for voices that still exist, then free channels.
pub(super) fn default_assignments(
    composition: &Composition,
    settings: &Value,
) -> Vec<VoiceAssignment> {
    let imported: Vec<VoiceAssignment> = settings
        .get("midi_assignments")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let mut assignments: Vec<VoiceAssignment> = Vec::new();
    for voice in &composition.voices {
        if let Some(found) = imported.iter().find(|a| a.voice == voice.id) {
            assignments.push(found.clone());
        }
    }
    let taken: Vec<u8> = assignments.iter().map(|a| a.channel).collect();
    let mut free = (0..16u8).filter(|c| *c != PERCUSSION_CHANNEL && !taken.contains(c));
    for voice in &composition.voices {
        if !assignments.iter().any(|a| a.voice == voice.id)
            && let Some(channel) = free.next()
        {
            assignments.push(VoiceAssignment {
                voice: voice.id.clone(),
                track: voice.id.clone(),
                channel,
                port: None,
            });
        }
    }
    assignments
}

/// The file kind of a named copy of a delivered render WAV.
const NAMED_COPY: &str = "wav_named_copy";

/// Where a delivered render's WAV is written: the name import_project reads.
fn render_file(id: &str) -> PathBuf {
    Path::new("renders").join(format!("{id}.wav"))
}

/// A file or folder name from an id, keeping letters, digits, `-`, `_`
/// and `.` and replacing anything else with `_`.
fn file_name_part(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Readable paths for delivered render WAVs: `mix/<span>.wav` for a mix
/// and `stems/<span>/<part>.wav` for a stem, where the span is the region
/// (section id) or `full`, `full-<n>-passes` or `excerpt-<start>-<end>`, and
/// the part is `voice-<id>`, `bus-<id>`, `group-<id>` or `direct`. A
/// voice-filtered render adds `-only-<voices>`, a pre-master stem
/// `-pre-master` and a cut tail `-tail-cut`. Renders that would share a path
/// add `-<revision>`, and
/// `-<revision>-<render id>` when the same revision still shares it.
fn named_copies(renders: &[&RenderRecord]) -> Vec<(PathBuf, String)> {
    use crate::review::RenderPart;
    let named: Vec<(String, String, String)> = renders
        .iter()
        .map(|record| {
            let render = &record.render;
            let span = match (&render.region, render.layout) {
                (Some(region), _) => file_name_part(region),
                (None, RenderLayout::Full { loop_passes: 1 }) => "full".to_owned(),
                (None, RenderLayout::Full { loop_passes }) => format!("full-{loop_passes}-passes"),
                (None, RenderLayout::Excerpt { start, end }) => format!("excerpt-{start}-{end}"),
            };
            let mut suffix = String::new();
            if let Some(voices) = &render.voices {
                suffix.push_str("-only-");
                suffix.push_str(
                    &voices
                        .iter()
                        .map(|v| file_name_part(v))
                        .collect::<Vec<_>>()
                        .join("-"),
                );
            }
            if render.pre_master {
                suffix.push_str("-pre-master");
            }
            // The tail policy is part of what the file is, like pre-master.
            if render.tail_cut {
                suffix.push_str("-tail-cut");
            }
            let stem = match &render.part {
                None | Some(RenderPart::Mixdown) => None,
                Some(RenderPart::Voice { id }) => Some(format!("voice-{}", file_name_part(id))),
                Some(RenderPart::Bus { id }) => Some(format!("bus-{}", file_name_part(id))),
                Some(RenderPart::Group { id, .. }) => Some(format!("group-{}", file_name_part(id))),
                Some(RenderPart::Direct) => Some("direct".to_owned()),
            };
            let base = match stem {
                None => format!("mix/{span}{suffix}"),
                Some(part) => format!("stems/{span}/{part}{suffix}"),
            };
            (base, render.revision.clone(), render.id.clone())
        })
        .collect();
    named
        .iter()
        .map(|(base, revision, id)| {
            let shared = |same_revision: bool| {
                named
                    .iter()
                    .filter(|(other, r, _)| other == base && (!same_revision || r == revision))
                    .count()
                    > 1
            };
            let name = match (shared(false), shared(true)) {
                (false, _) => format!("{base}.wav"),
                (true, false) => format!("{base}-{}.wav", file_name_part(revision)),
                (true, true) => format!(
                    "{base}-{}-{}.wav",
                    file_name_part(revision),
                    file_name_part(id)
                ),
            };
            (name.split('/').collect::<PathBuf>(), id.clone())
        })
        .collect()
}

pub(super) fn export_report(
    info: &ProjectInfo,
    revision: &Revision,
    plan: &MidiExport,
    files: &[(String, PathBuf, Vec<u8>)],
    (renders, copies): (&[&RenderRecord], &[(PathBuf, String)]),
) -> Value {
    let composition = &revision.composition;
    let mut not_in_midi = vec![
        if plan.sections_as_markers {
            "section ends and overlaps, the project brief, revision history and feedback are only in the project bundle; each section's id is written as a marker at its start, which reads back as a marker, not a section".to_owned()
        } else {
            "project brief, sections, revision history and feedback are only in the project bundle".to_owned()
        },
        "the reference render sound is not stored in MIDI; program numbers are General MIDI numbers, not chip patches".to_owned(),
    ];
    if info.midi_source.is_none() && composition.playback.loop_start.is_some() {
        not_in_midi.push("the loop is written as loopStart/loopEnd markers; players repeat it only if they support those markers".into());
    }
    if let Timebase::Effect = info.timebase {
        not_in_midi.push("effect timing is written on the export grid with a tempo that carries no musical meaning".into());
    }
    if info.midi_source.is_none()
        && composition
            .notes
            .iter()
            .any(|n| n.release_velocity.is_none())
    {
        not_in_midi.push("notes without a release velocity are written with velocity 64".into());
    }
    if !composition.mix.is_empty() {
        not_in_midi.push("the mixer (voice volume, pan, mute, solo and output, buses, sends, master gain and their automation) is not written to MIDI; it is in the project bundle and applied by render_preview. MIDI channel volume and pan come only from the volume and pan control lanes".into());
    }
    if !composition.mix.effects.is_empty() {
        not_in_midi.push("effect chains (EQ, compression, limiting, delay, reverb, SNES-style echo) are render-time processing of the preview audio; MIDI has no equivalent, so they are kept in the project bundle only".into());
    }
    if !composition.mix.effect_automation.is_empty() {
        not_in_midi.push("effect parameter automation (equalizer, dynamics, delay, reverb and SNES-style echo settings that move over time) is render-time processing like the chains themselves; MIDI has no equivalent, so the lanes are kept in the project bundle only".into());
    }
    if !composition.mix.instruments.is_empty() {
        not_in_midi.push("chip instruments assigned to voices (S-DSP BRR samples, OPN FM patches) play those voices in render_preview only; MIDI cannot carry them, so the file keeps the authored notes and program numbers and the project bundle keeps the assignments".into());
    }
    if !composition.mix.clips.is_empty() {
        not_in_midi.push("audio clips (ranges of retained WAV sources placed on voices) play in render_preview only; MIDI cannot carry audio, so the project bundle keeps the clip records and objects/ carries their source files".into());
    }
    let mut report = json!({
        "project": info.name,
        "revision": revision.id,
        "timebase": info.timebase,
        "midi_source_sha256": info.midi_source.as_ref().map(|source| &source.sha256),
        "midi": {"format": info.midi_source.as_ref().and_then(|source| midly::Smf::parse(&source.bytes).ok().map(|s| match s.header.format {midly::Format::SingleTrack => 0, _ => 1})).unwrap_or(1), "assignments": plan.assignments, "timing": plan.timing},
        "preservation": match info.midi_source.as_ref() {
            Some(source) => {
                let midi = &files.iter().find(|(kind, _, _)| kind == "midi").expect("MIDI is in the delivery").2;
                json!({"source_sha256": source.sha256, "comparison": crate::midi::compare_midi(&source.bytes, midi),
                    "scope": "exact bytes when unchanged; edited source events preserved outside requested fields, with composition readback checked before writing; no target-audio equivalence claim"})
            }
            None => json!({"source_sha256": null, "scope": "newly composed project; there is no original MIDI to preserve"}),
        },
        "not_in_midi": not_in_midi,
        "checks": {"project_bundle": "passed", "midi_reimport": if info.midi_source.is_some() { "source edit readback checked" } else { "checked composition writer" },
            "constraint_profiles": "not run by export; use check_project and report its result"},
        "files": files.iter().map(|(kind, path, bytes)| {
            let mut entry = json!({"kind": kind, "path": path, "sha256": sha256_hex(bytes)});
            let render = match kind.as_str() {
                "wav" => renders.iter().find(|r| *path == render_file(&r.render.id)),
                NAMED_COPY => copies.iter().find(|(copy, _)| copy == path).and_then(|(_, id)| renders.iter().find(|r| r.render.id == *id)),
                _ => None,
            };
            if let Some(record) = render {
                if kind == NAMED_COPY {
                    entry["copy_of"] = json!(render_file(&record.render.id));
                }
                describe_render_file(&mut entry, &record.render);
            }
            entry
        }).collect::<Vec<_>>(),
    });
    if plan.sections_as_markers {
        report["sections"] = json!({
            "written_to_midi": "the id of each section as a marker meta event at its start, after markers already there",
            "not_in_midi": "section ends and overlaps; a reimport reads these markers as markers, not sections",
            "items": composition.sections,
        });
    }
    if !composition.mix.is_empty() {
        report["mix"] = json!({
            "written_to_midi": false,
            "reason": "MIDI has no buses, sends or master gain, and CC7/CC10 already carry the composition's own volume and pan lanes with receiver-defined curves, so no mixer value maps losslessly",
            "carried_by": "project bundle",
            "items": crate::mix::mix_changes(&Default::default(), &composition.mix).into_iter().map(|(_, label)| label).collect::<Vec<_>>(),
        });
        if !composition.mix.instruments.is_empty() {
            report["mix"]["instruments"] = json!(
                composition
                    .mix
                    .instruments
                    .iter()
                    .map(|a| json!({
                "voice": a.voice, "kind": a.instrument.kind(), "source": a.instrument.source(),
                "written_to_midi": false}))
                    .collect::<Vec<_>>()
            );
        }
        if !composition.mix.clips.is_empty() {
            report["mix"]["clips"] = json!(
                composition
                    .mix
                    .clips
                    .iter()
                    .map(
                        |c| json!({"clip": c.id, "voice": c.voice, "source": c.source,
                        "start": c.start, "duration": c.duration, "written_to_midi": false})
                    )
                    .collect::<Vec<_>>()
            );
        }
    }
    report
}
