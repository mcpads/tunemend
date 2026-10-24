//! Prepared target artifacts are immutable and separate from editable sources.
use super::*;
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

const PLANS: &str = "target_plan";
const BRIDGE: &str = include_str!("../score_bridge.py");

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScoreTarget {
    /// Existing Python driver encoder implementing the Compile score API.
    pub encoder: String,
    /// Original score module supplying driver duration/percussion/tuning tables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Matching sample-bank module; used to reserve its ARAM range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank: Option<String>,
    /// Immutable bank from load_instrument_bank/copy_instrument; replaces both file paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument_bank: Option<String>,
    /// Optional sample indices whose tuning words are explicitly reset.
    #[serde(default)]
    pub tuned_srcns: Vec<u8>,
    /// Python executable; defaults to python3. No third-party Python packages needed.
    #[serde(default = "default_python")]
    pub python: String,
}
fn default_python() -> String {
    "python3".into()
}
fn default_ppqn() -> u16 {
    12
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScoreMapping {
    #[serde(default = "default_ppqn")]
    pub ppqn: u16,
    #[serde(default)]
    pub offset_ticks: u64,
    #[serde(default = "yes")]
    pub whole_bars: bool,
    #[serde(default = "yes")]
    pub r#loop: bool,
    /// Play the initial silence once before the loop. Requires an encoder with
    /// lead-in support; otherwise the representation refuses.
    #[serde(default)]
    pub intro_once: bool,
    pub tracks: Vec<ScoreTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScoreTrack {
    pub name: String,
    #[serde(default)]
    pub channels: Vec<u8>,
    /// Highest-first chord rank among notes with the same onset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<u32>,
    /// Share another non-copy track's score, with optional delay and transposition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<String>,
    #[serde(default)]
    pub delay: u64,
    #[serde(default)]
    pub srcn: u8,
    #[serde(default)]
    pub adsr: u8,
    #[serde(default)]
    pub octave_shift: i16,
    #[serde(default)]
    pub transpose: i16,
    pub volume: u8,
    #[serde(default)]
    pub pan: i16,
    #[serde(default)]
    pub fill_gaps: u32,
    /// Coincident mapped onsets otherwise refuse. Set last to select the final
    /// event sorted by (tick, duration, encoded pitch/percussion status).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_tick: Option<String>,
    /// MIDI key to driver percussion-table index (0..29).
    #[serde(default)]
    pub drums: BTreeMap<u8, u8>,
    /// Use round to round starts; otherwise off-grid starts are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantize: Option<String>,
    /// Explicit driver volume changes at source ticks; no inferred MIDI CC mapping.
    #[serde(default)]
    pub volumes: Vec<(u64, u8)>,
    #[serde(default)]
    pub vol_env: u8,
    #[serde(default)]
    pub vibrato: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareTargetInput {
    pub job_id: String,
    /// A loaded source, or a project (exactly one). No editable projection is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub target: ScoreTarget,
    pub mapping: ScoreMapping,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectTargetInput {
    pub plan: String,
    /// summary (default), events, tracks, or score_events.
    #[serde(default)]
    pub view: Option<String>,
    /// Required for score_events: the mapped track name.
    #[serde(default)]
    pub track: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportTargetInput {
    pub job_id: String,
    /// Immutable plan returned by prepare_target; selects the exact source/settings/output.
    pub plan: String,
    /// Every change kind from the plan must be explicitly listed. This is the agent's
    /// mapping decision, not a request for a separate human approval.
    pub accepted_changes: Vec<String>,
    /// New folder in the workspace.
    pub destination: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanRecord {
    report: String,
    source: String,
    module: String,
    inputs: BTreeMap<String, String>,
}

fn read(path: &Path) -> Result<Vec<u8>, ToolResponse> {
    std::fs::read(path)
        .map_err(|e| ToolResponse::error(format!("cannot read {}: {e}", path.display())))
}

impl Service {
    pub(super) fn prepare_target(&mut self, input: PrepareTargetInput) -> Handled {
        let (source, origin) = match (&input.source, &input.project) {
            (Some(source), None) if input.revision.is_none() => {
                (self.midi_source_bytes(source)?, json!({"source": source}))
            }
            (None, Some(project)) => {
                let (info, revision) = self.revision(project, input.revision.as_deref())?;
                let (_, settings) = self.project(project)?;
                let assignments =
                    super::delivery::default_assignments(&revision.composition, &settings);
                let plan = MidiExport {
                    id: "target-input".into(),
                    revision: revision.id.clone(),
                    timing: if info.timebase == Timebase::Effect {
                        Some(MidiTiming {
                            ppq: 1000,
                            microseconds_per_quarter: 1_000_000,
                        })
                    } else {
                        None
                    },
                    assignments,
                    sections_as_markers: false,
                };
                let original_source = info.midi_source.as_ref().map(|s| s.sha256.clone());
                let bytes = if let Some(source) = info.midi_source {
                    crate::midi::write_preserving(
                        &source,
                        info.timebase,
                        &revision.composition,
                        &plan,
                    )
                } else {
                    write_smf(info.timebase, &revision.composition, &plan)
                }
                .map_err(|d| ToolResponse::blocked("cannot obtain this revision's MIDI", d))?;
                let mut origin = json!({"project": project, "revision": revision.id, "original_source_sha256": original_source});
                if !revision.composition.mix.is_empty() {
                    origin["mix_not_transferred"] = json!(
                        "the revision's render mixer is not part of its MIDI and is not mapped to the target"
                    );
                }
                (bytes, origin)
            }
            _ => {
                return done(ToolResponse::refused(
                    Code::InvalidRange,
                    "/source",
                    "supply source or project, exactly one; revision belongs only to project",
                ));
            }
        };
        let summary = crate::midi::source_summary(&source).map_err(|d| {
            ToolResponse::blocked("select a MIDI child instead of its container", d)
        })?;
        if summary["format"] == 2 || summary["timing"]["kind"] != "metrical" {
            return done(ToolResponse::refused(
                Code::TargetRefused,
                "/source",
                "this score backend requires synchronous PPQ MIDI (format 0 or 1); original loading and export remain available",
            ));
        }
        let events = crate::midi::source_events(&source, None, 0, usize::MAX)
            .map_err(|d| ToolResponse::blocked("cannot inspect source", d))?;
        let mut files = BTreeMap::new();
        files.insert(
            "encoder.py".to_string(),
            read(&self.workspace.join(&input.target.encoder))?,
        );
        let instrument_provenance = if let Some(id) = &input.target.instrument_bank {
            if input.target.template.is_some() || input.target.bank.is_some() {
                return done(ToolResponse::refused(
                    Code::InvalidRange,
                    "/target",
                    "choose instrument_bank or template/bank file paths, not both",
                ));
            }
            let (bank, record) = self.instrument_bank_data(id)?;
            let layout = &bank.layout;
            let blocks = crate::instruments::module_blocks(&bank.assets["bank.bin"])
                .expect("validated bank");
            if layout.directory != 0x4500
                || layout.tuning != 0x3af0
                || layout.slots != 55
                || layout.sample_memory_end > 0xaae0
                || blocks[0].address != 0x4500
            {
                return done(ToolResponse::refused(
                    Code::TargetRefused,
                    "/target/instrument_bank",
                    "bank layout does not match this Compile score backend",
                ));
            }
            for track in &input.mapping.tracks {
                if track.drums.is_empty() {
                    bank.sample(track.srcn).map_err(|e| {
                        ToolResponse::refused(
                            Code::TargetRefused,
                            "/mapping/tracks",
                            format!("{}: {e}", track.name),
                        )
                    })?;
                    bank.envelope(track.adsr).map_err(|e| {
                        ToolResponse::refused(
                            Code::TargetRefused,
                            "/mapping/tracks",
                            format!("{}: {e}", track.name),
                        )
                    })?;
                }
            }
            files.extend(bank.assets);
            json!({"bank":id,"layout":record.layout,"lineage":record.lineage})
        } else {
            let (Some(template), Some(bank)) = (&input.target.template, &input.target.bank) else {
                return done(ToolResponse::refused(
                    Code::InvalidRange,
                    "/target",
                    "supply instrument_bank or both template and bank paths",
                ));
            };
            files.insert("template.bin".into(), read(&self.workspace.join(template))?);
            files.insert("bank.bin".into(), read(&self.workspace.join(bank))?);
            Value::Null
        };
        let request = json!({"summary": summary, "events": events["events"], "mapping": input.mapping, "tuned_srcns": input.target.tuned_srcns, "files": files});
        let mut child = Command::new(&input.target.python)
            .args(["-B", "-c", BRIDGE])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ToolResponse::error(format!("cannot start score adapter: {e}")))?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(request.to_string().as_bytes())
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        let output = child
            .wait_with_output()
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if !output.status.success() {
            return Err(ToolResponse::error(format!(
                "score adapter failed: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(4000)
                    .collect::<String>()
            )));
        }
        let mut result: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| ToolResponse::error(format!("invalid score adapter response: {e}")))?;
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            return done(ToolResponse::refused(
                Code::TargetRefused,
                "/mapping",
                error,
            ));
        }
        let module: Vec<u8> = serde_json::from_value(
            result
                .as_object_mut()
                .expect("object")
                .remove("module")
                .ok_or_else(|| ToolResponse::error("adapter returned no module"))?,
        )
        .map_err(|e| ToolResponse::error(e.to_string()))?;
        if result.pointer("/verification/score_readback") != Some(&json!(true)) {
            return Err(ToolResponse::error(
                "adapter did not verify encoded score readback",
            ));
        }
        files.insert("bridge.py".into(), BRIDGE.as_bytes().to_vec());
        let store = self.store_mut(true)?;
        let inputs: BTreeMap<String, String> = files
            .iter()
            .map(|(name, bytes)| Ok((name.clone(), store.put_object(bytes)?)))
            .collect::<Result<_, StoreError>>()?;
        let source_hash = store.put_object(&source)?;
        let module_hash = store.put_object(&module)?;
        result["source_sha256"] = json!(source_hash);
        result["origin"] = origin;
        result["mapping"] = json!(input.mapping);
        result["target_inputs"] = json!(inputs);
        result["instrument_bank"] = instrument_provenance;
        result["tuned_srcns"] = json!(input.target.tuned_srcns);
        result["module_sha256"] = json!(module_hash);
        result["preservation"] = json!({"original_bytes": "retained exactly", "target": "explicit mapped representation; not lossless MIDI or original-device sound equivalence"});
        result["not_verified"] = json!([
            "ROM insertion",
            "game playback",
            "human listening",
            "driver timbre and envelope equivalence"
        ]);
        let report = store.put_object(&serde_json::to_vec_pretty(&result).expect("serializes"))?;
        let record = PlanRecord {
            report: report.clone(),
            source: source_hash,
            module: module_hash,
            inputs,
        };
        let response = ToolResponse::ok(
            "prepared and decoded a target candidate; inspect changes before selecting them for export",
            plan_summary(&report, &result),
        );
        commit(response, move |w| {
            w.put_document("", PLANS, &report, &record)
        })
    }

    fn target_plan(&mut self, id: &str) -> Result<(PlanRecord, Value), ToolResponse> {
        let store = self.store_ref()?;
        let record: PlanRecord = store.reader().document("", PLANS, id)?.ok_or_else(|| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/plan",
                "unknown target plan; call prepare_target",
            )
        })?;
        let report = read(&store.object_path(&record.report))?;
        if record.report != id || sha256_hex(&report) != id {
            return Err(ToolResponse::error("target report identity mismatch"));
        }
        let value = serde_json::from_slice(&report)
            .map_err(|e| ToolResponse::error(format!("invalid plan: {e}")))?;
        Ok((record, value))
    }

    pub(super) fn inspect_target(&mut self, input: InspectTargetInput) -> Handled {
        let (_, report) = self.target_plan(&input.plan)?;
        let view = input.view.as_deref().unwrap_or("summary");
        if view == "summary" {
            return done(ToolResponse::ok(
                "target plan",
                plan_summary(&input.plan, &report),
            ));
        }
        if !["events", "tracks", "score_events"].contains(&view) {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/view",
                "choose summary, events, tracks, or score_events",
            ));
        }
        let rows: Vec<Value> = match view {
            "tracks" => report["tracks"]
                .as_array()
                .ok_or_else(|| ToolResponse::error("missing tracks"))?
                .iter()
                .map(|track| {
                    let mut track = track.clone();
                    let events = track
                        .as_object_mut()
                        .expect("track object")
                        .remove("decoded_events")
                        .expect("decoded events");
                    track["score_events"] = json!(events.as_array().expect("event array").len());
                    track
                })
                .collect(),
            "score_events" => report["tracks"]
                .as_array()
                .and_then(|tracks| {
                    tracks
                        .iter()
                        .find(|track| track["name"].as_str() == input.track.as_deref())
                })
                .and_then(|track| track["decoded_events"].as_array())
                .cloned()
                .ok_or_else(|| {
                    ToolResponse::refused(
                        Code::UnknownReference,
                        "/track",
                        "select a mapped track by name from the tracks view",
                    )
                })?,
            _ => report["events"]
                .as_array()
                .cloned()
                .ok_or_else(|| ToolResponse::error("missing event rows"))?,
        };
        let offset = input.offset.min(rows.len());
        let end = offset
            .saturating_add(input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize)
            .min(rows.len());
        done(ToolResponse::ok(
            "target mapping details",
            json!({"plan": input.plan, "view": view, "items": &rows[offset..end], "total": rows.len(), "next_cursor": (end < rows.len()).then_some(end)}),
        ))
    }

    pub(super) fn export_target(&mut self, input: ExportTargetInput) -> Handled {
        let (record, report) = self.target_plan(&input.plan)?;
        let required = report["changes"]
            .as_object()
            .ok_or_else(|| ToolResponse::error("missing change inventory"))?;
        let missing: Vec<_> = required
            .keys()
            .filter(|k| !input.accepted_changes.contains(k))
            .collect();
        if !missing.is_empty()
            || input
                .accepted_changes
                .iter()
                .any(|k| !required.contains_key(k))
        {
            let mut response = ToolResponse::refused(
                Code::TargetRefused,
                "/accepted_changes",
                "select exactly the plan's change kinds after inspecting their details, or revise the mapping",
            );
            response.result = json!({"required_changes": required, "missing": missing});
            return done(response);
        }
        let relative = Path::new(&input.destination);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return done(ToolResponse::refused(
                Code::InvalidRange,
                "/destination",
                "choose a new relative folder without parent components",
            ));
        }
        let destination = self.workspace.join(relative);
        if destination.exists() {
            return done(ToolResponse::refused(
                Code::DuplicateId,
                "/destination",
                "destination exists",
            ));
        }
        let mut objects = record.inputs;
        objects.insert("source.mid".into(), record.source);
        objects.insert("score.bin".into(), record.module);
        objects.insert("report.json".into(), record.report);
        let store = self.store_ref()?;
        let mut files = Vec::new();
        for (name, hash) in objects {
            let bytes = read(&store.object_path(&hash))?;
            if sha256_hex(&bytes) != hash {
                return Err(ToolResponse::error(format!(
                    "stored {name} identity mismatch"
                )));
            }
            // Tool source snapshots remain private in the store; deliver target data and reports.
            if !name.ends_with(".py") {
                files.push((name, hash, bytes));
            }
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ToolResponse::error(e.to_string()))?;
        }
        let staging = destination.with_file_name(format!(
            ".tunemend-target-{}-{}",
            std::process::id(),
            sha256_hex(input.job_id.as_bytes())
        ));
        // Never reuse or remove a staging directory left by another attempt.
        std::fs::create_dir(&staging).map_err(|e| ToolResponse::error(e.to_string()))?;
        let outcome = (|| -> std::io::Result<()> {
            for (name, hash, bytes) in &files {
                let path = staging.join(name);
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                if sha256_hex(&std::fs::read(path)?) != *hash {
                    return Err(std::io::Error::other("target export readback differs"));
                }
            }
            if destination.exists() {
                return Err(std::io::Error::other("destination appeared during export"));
            }
            std::fs::rename(&staging, &destination)?;
            Ok(())
        })();
        if let Err(e) = outcome {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(ToolResponse::error(e.to_string()));
        }
        let mut response = ToolResponse::ok(
            "exported the verified mapped score and exact MIDI source",
            json!({"plan": input.plan, "accepted_changes": input.accepted_changes, "readback_checked": true, "target_audio_equivalence": false}),
        );
        response.artifacts = files
            .into_iter()
            .map(|(kind, sha256, _)| Artifact {
                path: destination.join(&kind).display().to_string(),
                kind,
                sha256,
            })
            .collect();
        done(response)
    }
}

fn plan_summary(id: &str, report: &Value) -> Value {
    json!({"plan": id, "source_sha256": report["source_sha256"], "module_sha256": report["module_sha256"], "changes": report["changes"], "encoding": report["encoding"], "verification": report["verification"], "events": report["events"].as_array().map(Vec::len), "tracks": report["tracks"].as_array().map(Vec::len), "not_verified": report["not_verified"]})
}
