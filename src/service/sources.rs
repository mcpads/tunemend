//! Immutable source loading is independent of creating an editable project.
use super::*;
use std::io::Write;

// Empty is not a valid project ID: this collection belongs to the workspace.
const SOURCE_OWNER: &str = "";
const SOURCES: &str = "midi_source";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceRecord {
    sha256: String,
    bytes: usize,
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<crate::midi::EmbeddedMidi>,
}

/// States up front whether the source has any timed content. A file whose
/// every event sits at tick 0 loads and exports exactly but has nothing to
/// play, edit in time or map.
fn describe_timing(summary: &mut Value, bytes: &[u8]) {
    let Some(timed) = crate::render::has_timed_content(bytes) else {
        return;
    };
    summary["timed_content"] = json!(timed);
    if !timed {
        summary["timed_content_note"] = json!(
            "every event is at tick 0, so there is nothing to play, place in time or map; the file is retained and exports exactly. Report this to the person with the events it does hold (inspect_midi), and do not add notes or timing to make it playable"
        );
    }
}

/// Records a source whose bytes are already stored, unless it is loaded.
pub(super) fn register_source(
    writer: &crate::store::Writer,
    sha256: &str,
    bytes: usize,
    name: &str,
) -> Result<(), crate::store::StoreError> {
    if writer
        .document::<SourceRecord>(SOURCE_OWNER, SOURCES, sha256)?
        .is_none()
    {
        let record = SourceRecord {
            sha256: sha256.to_owned(),
            bytes,
            name: name.to_owned(),
            children: vec![],
        };
        writer.put_document(SOURCE_OWNER, SOURCES, sha256, &record)?;
    }
    Ok(())
}

impl Service {
    pub(super) fn midi_source_bytes(&mut self, source: &str) -> Result<Vec<u8>, ToolResponse> {
        let store = self.store_ref()?;
        let record: SourceRecord = store
            .reader()
            .document(SOURCE_OWNER, SOURCES, source)?
            .ok_or_else(|| {
                ToolResponse::refused(
                    Code::UnknownReference,
                    "/source",
                    "source is not loaded; call load_midi first",
                )
            })?;
        let bytes = std::fs::read(store.object_path(&record.sha256))
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if record.sha256 != source
            || bytes.len() != record.bytes
            || sha256_hex(&bytes) != record.sha256
        {
            return Err(ToolResponse::refused(
                Code::UnsupportedMidi,
                "/source",
                "stored source bytes do not match their recorded identity",
            ));
        }
        Ok(bytes)
    }

    pub(super) fn load_midi(&mut self, input: LoadMidiInput) -> Handled {
        let path = self.workspace.join(&input.path);
        let bytes = std::fs::read(&path)
            .map_err(|e| ToolResponse::error(format!("cannot read {}: {e}", input.path)))?;
        let hash = sha256_hex(&bytes);
        if input
            .expected_sha256
            .as_ref()
            .is_some_and(|expected| expected != &hash)
        {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/expected_sha256",
                "source changed since inspection",
            ));
        }
        let (mut summary, children) = match crate::midi::source_summary(&bytes) {
            Ok(summary) => (summary, vec![]),
            Err(diagnostics) => {
                let children = crate::midi::embedded_midi(&bytes);
                if children.is_empty() {
                    return done(ToolResponse::blocked(
                        "no structurally valid MIDI stream was found; original file was not changed",
                        diagnostics,
                    ));
                }
                (crate::midi::container_summary(&bytes, &children), children)
            }
        };
        if children.is_empty() {
            // Projection diagnostics never turn successful source loading into failure.
            let projection = crate::midi::analyze_smf(&bytes);
            summary["editable_projection"] = json!({"available": projection["can_import"], "diagnostics": projection["diagnostics"], "editing": projection["editing"], "preview": projection["preview"]});
            describe_timing(&mut summary, &bytes);
        } else {
            summary["editable_projection"] = json!({"available": false, "next_action": "select a child MIDI source first; no arrangement or target mapping has been applied"});
        }
        let store = self.store_mut(true)?;
        let object = store.put_object(&bytes)?;
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut records = vec![SourceRecord {
            sha256: hash.clone(),
            bytes: bytes.len(),
            name: name.clone(),
            children: children.clone(),
        }];
        for (index, child) in children.iter().enumerate() {
            let data = &bytes[child.offset..child.offset + child.length];
            let object = store.put_object(data)?;
            records.push(SourceRecord {
                sha256: object,
                bytes: data.len(),
                name: format!("{name}#{index}"),
                children: vec![],
            });
        }
        let mut response = ToolResponse::ok(
            "loaded original data without applying an arrangement or target mapping",
            summary,
        );
        response.artifacts.push(Artifact {
            kind: "original_midi_source".into(),
            path: store.object_path(&object).display().to_string(),
            sha256: object,
        });
        commit(response, move |writer| {
            for record in records {
                if writer
                    .document::<SourceRecord>(SOURCE_OWNER, SOURCES, &record.sha256)?
                    .is_none()
                {
                    writer.put_document(SOURCE_OWNER, SOURCES, &record.sha256, &record)?;
                }
            }
            Ok(())
        })
    }

    pub(super) fn inspect_midi(&mut self, input: InspectMidiInput) -> Handled {
        if matches!(input.view, MidiSourceView::Sources) {
            let sources: Vec<SourceRecord> = if Store::exists(&self.workspace) {
                self.store_ref()?
                    .reader()
                    .documents(SOURCE_OWNER, SOURCES)?
            } else {
                vec![]
            };
            return done(ToolResponse::ok(
                "loaded MIDI sources and containers",
                json!({"sources": sources}),
            ));
        }
        let source = input.source.as_ref().ok_or_else(|| {
            ToolResponse::refused(
                Code::InvalidRange,
                "/source",
                "this view needs a loaded source ID",
            )
        })?;
        let bytes = self.midi_source_bytes(source)?;
        let record: SourceRecord = self
            .store_ref()?
            .reader()
            .document(SOURCE_OWNER, SOURCES, source)?
            .expect("source read above");
        if matches!(input.view, MidiSourceView::Events) && !record.children.is_empty() {
            let mut response = ToolResponse::refused(
                Code::InvalidRange,
                "/source",
                "this container holds independent MIDI streams; select a child source ID to inspect events",
            );
            response.result = crate::midi::container_summary(&bytes, &record.children);
            return done(response);
        }
        let result = match input.view {
            MidiSourceView::Sources => unreachable!(),
            MidiSourceView::Summary => {
                if record.children.is_empty() {
                    crate::midi::source_summary(&bytes).map(|mut summary| {
                        let projection = crate::midi::analyze_smf(&bytes);
                        summary["editable_projection"] = json!({"available": projection["can_import"], "diagnostics": projection["diagnostics"], "editing": projection["editing"], "preview": projection["preview"]});
                        describe_timing(&mut summary, &bytes);
                        summary
                    })
                }
                else { Ok(crate::midi::container_summary(&bytes, &record.children)) }
            },
            MidiSourceView::Events => crate::midi::source_events(&bytes, input.track.map(|i| i as usize), input.offset.unwrap_or(0), input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize),
            MidiSourceView::Bytes => {
                let offset = input.offset.unwrap_or(0);
                if offset > bytes.len() { return done(ToolResponse::refused(Code::InvalidRange, "/offset", "offset exceeds the source length")); }
                let end = offset.saturating_add(input.limit.unwrap_or(256).clamp(1, 4096) as usize).min(bytes.len());
                Ok(json!({"source": source, "offset": offset, "bytes": &bytes[offset..end], "next_cursor": (end < bytes.len()).then_some(end), "total_bytes": bytes.len()}))
            }
        }.map_err(|diagnostics| ToolResponse::blocked("source inspection failed", diagnostics))?;
        done(ToolResponse::ok(
            "original source data; no mapping or event normalization applied",
            result,
        ))
    }

    pub(super) fn export_midi_source(&mut self, input: ExportMidiSourceInput) -> Handled {
        let bytes = self.midi_source_bytes(&input.source)?;
        let path = self.workspace.join(&input.destination);
        if path.exists() {
            return done(ToolResponse::refused(
                Code::DuplicateId,
                "/destination",
                "destination exists; choose a new file",
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ToolResponse::error(e.to_string()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(ToolResponse::error(error.to_string()));
        }
        drop(file);
        let written = std::fs::read(&path).map_err(|e| ToolResponse::error(e.to_string()))?;
        if written != bytes {
            return Err(ToolResponse::error("source export readback differs"));
        }
        let mut response = ToolResponse::ok(
            "exported the exact original source bytes",
            json!({"source": input.source, "bytes": bytes.len(), "byte_identical": true, "readback_checked": true}),
        );
        response.artifacts.push(Artifact {
            kind: "midi_source".into(),
            path: path.display().to_string(),
            sha256: input.source,
        });
        done(response)
    }
}
