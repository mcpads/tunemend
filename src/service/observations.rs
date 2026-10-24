//! Exact retention of external runtime observations and their association
//! with Tunemend subjects. Producers are optional evidence, never loaders.
use super::*;
use std::collections::BTreeMap;

const OBSERVATIONS: &str = "observation";
const MAX_FILES: usize = 256;
const MAX_BYTES: u64 = 256 << 20;
const COMPLETION: [&str; 4] = ["complete", "interrupted", "truncated", "failed"];

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachObservationInput {
    pub job_id: String,
    /// Published observation folder, relative to the workspace. Every file is retained.
    pub directory: String,
    /// portable (default; requires receipt) or emucap_audio (derived from manifest.json).
    #[serde(default = "portable")]
    pub producer_format: String,
    /// Portable receipt: producer, content, capability, clock, completion, members, scope.
    /// Unknown fields are retained verbatim.
    #[serde(default)]
    pub receipt: Option<Value>,
    /// What the agent compared or diagnosed with this observation.
    #[serde(default)]
    pub subjects: Vec<ObservationSubject>,
    #[serde(default)]
    pub expected: ObservationExpectation,
    /// Exactly the limitation kinds a previous attempt reported.
    #[serde(default)]
    pub accepted_limitations: Vec<String>,
}
fn portable() -> String {
    "portable".into()
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationSubject {
    /// midi_source, sound_source, instrument_bank, fm_bank, sample_plan, target_plan or revision.
    pub kind: String,
    pub id: String,
    /// Required for kind=revision.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub relation: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationExpectation {
    #[serde(default)]
    pub system: Option<String>,
    /// Content identity keys and values, compared by exact equality.
    #[serde(default)]
    pub content: BTreeMap<String, Value>,
    #[serde(default)]
    pub producer_build: Option<String>,
    #[serde(default)]
    pub capability_revision: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectObservationInput {
    #[serde(default)]
    pub observation: Option<String>,
    /// observations, summary (default), members or bytes.
    #[serde(default)]
    pub view: Option<String>,
    /// observations view: only records associated with this subject ID.
    #[serde(default)]
    pub subject: Option<String>,
    /// bytes view: member path.
    #[serde(default)]
    pub member: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<u32>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportObservationInput {
    pub job_id: String,
    pub observation: String,
    pub destination: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ObservationRecord {
    producer_format: String,
    receipt: Value,
    /// Every retained file by folder-relative path.
    members: BTreeMap<String, String>,
    limitations: Vec<String>,
    checks: Value,
    subjects: Vec<ObservationSubject>,
}
fn refused(location: &str, message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::InvalidRange, location, message)
}
fn text<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

/// Reads every regular file below `root`, refusing links and special files.
fn read_folder(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, ToolResponse> {
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| ToolResponse::error(e.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|e| ToolResponse::error(e.to_string()))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| ToolResponse::error(e.to_string()))?;
            let relative = path
                .strip_prefix(root)
                .expect("below root")
                .components()
                .map(|c| c.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| refused("/directory", "member names must be UTF-8"))?
                .join("/");
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                if files.len() == MAX_FILES {
                    return Err(refused(
                        "/directory",
                        format!("more than {MAX_FILES} files"),
                    ));
                }
                let bytes = std::fs::read(&path).map_err(|e| ToolResponse::error(e.to_string()))?;
                total += bytes.len() as u64;
                if total > MAX_BYTES {
                    return Err(refused("/directory", "observation exceeds 256 MiB"));
                }
                files.insert(relative, bytes);
            } else {
                return Err(refused(
                    "/directory",
                    format!("`{relative}` is a link or special file"),
                ));
            }
        }
    }
    if files.is_empty() {
        return Err(refused("/directory", "observation folder has no files"));
    }
    Ok(files)
}

/// Portable receipt from an emucap `record_audio` folder, checked against its WAV.
fn emucap_audio_receipt(files: &BTreeMap<String, Vec<u8>>) -> Result<Value, ToolResponse> {
    let location = "/producer_format";
    let manifest: Value = files
        .get("manifest.json")
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .filter(Value::is_object)
        .ok_or_else(|| refused(location, "emucap_audio needs a JSON manifest.json"))?;
    let audio = &manifest["audio"];
    let name = audio["file"].as_str().unwrap_or("audio.wav");
    let wav = files
        .get(name)
        .ok_or_else(|| refused(location, format!("manifest names missing `{name}`")))?;
    let number = |key: &str| audio[key].as_u64();
    let (Some(channels), Some(rate), Some(frames), Some(payload)) = (
        number("channels"),
        number("sample_rate"),
        number("sample_frames"),
        number("payload_bytes"),
    ) else {
        return Err(refused(location, "manifest audio shape is incomplete"));
    };
    let le16 = |at: usize| {
        wav.get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as u64)
    };
    let le32 = |at: usize| {
        wav.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64)
    };
    let pcm16 = audio["format"] == "pcm_s16le";
    if !pcm16
        || wav.get(0..4) != Some(b"RIFF")
        || wav.get(8..16) != Some(b"WAVEfmt ")
        || le16(20) != Some(1)
        || le16(22) != Some(channels)
        || le32(24) != Some(rate)
        || le16(34) != Some(16)
        || wav.get(36..40) != Some(b"data")
        || le32(40) != Some(payload)
        || payload != frames * channels * 2
        || wav.len() as u64 != 44 + payload
        || text(audio, "/file_sha256") != Some(sha256_hex(wav).as_str())
        || text(audio, "/payload_sha256") != Some(sha256_hex(&wav[44..]).as_str())
    {
        return Err(refused(
            location,
            "audio.wav differs from the manifest's declared format, size or hashes",
        ));
    }
    let status = match manifest["status"].as_str() {
        Some("completed") => "complete",
        Some("interrupted") => "interrupted",
        _ => "failed",
    };
    let producer = &manifest["producer"];
    Ok(json!({
        "producer": {"name": "emucap", "system": producer["system"], "backend": producer["adapter"],
            "build": producer["build"], "host_build": producer["host_build"], "launch_id": producer["launch_id"]},
        "content": manifest["content"],
        "clock": manifest["clock"],
        "completion": {"status": status, "detail": manifest["sink_outcome"]},
        "members": [
            {"path": "manifest.json", "role": "producer_manifest", "sha256": sha256_hex(&files["manifest.json"])},
            {"path": name, "role": "audio", "sha256": audio["file_sha256"]},
        ],
        "scope": format!("{} PCM at {rate} Hz from {}", audio["source"].as_str().unwrap_or("guest"), manifest["clock"]["domain"].as_str().unwrap_or("an unrecorded clock")),
    }))
}

/// Hard checks refuse; the rest become limitation kinds the caller must accept.
fn check_receipt(
    receipt: &Value,
    files: &BTreeMap<String, Vec<u8>>,
    expected: &ObservationExpectation,
) -> Result<(Vec<String>, Value), ToolResponse> {
    if !receipt.is_object() {
        return Err(refused("/receipt", "receipt must be an object"));
    }
    for field in ["/producer/name", "/producer/system"] {
        if text(receipt, field).is_none() {
            return Err(refused(&format!("/receipt{field}"), "required text"));
        }
    }
    let status = text(receipt, "/completion/status")
        .filter(|s| COMPLETION.contains(s))
        .ok_or_else(|| {
            refused(
                "/receipt/completion/status",
                "complete, interrupted, truncated or failed",
            )
        })?;
    let mut described = Vec::new();
    for (i, member) in receipt["members"]
        .as_array()
        .ok_or_else(|| refused("/receipt/members", "list every described file"))?
        .iter()
        .enumerate()
    {
        let (Some(path), Some(hash)) = (text(member, "/path"), text(member, "/sha256")) else {
            return Err(refused(
                &format!("/receipt/members/{i}"),
                "path and sha256 required",
            ));
        };
        let bytes = files
            .get(path)
            .ok_or_else(|| refused(&format!("/receipt/members/{i}/path"), "file is missing"))?;
        if sha256_hex(bytes) != hash {
            return Err(refused(
                &format!("/receipt/members/{i}/sha256"),
                "file bytes differ from the receipt",
            ));
        }
        described.push(path);
    }
    let system = text(receipt, "/producer/system");
    if expected
        .system
        .as_deref()
        .is_some_and(|s| Some(s) != system)
    {
        return Err(refused(
            "/expected/system",
            format!("receipt observed `{}`", system.unwrap_or_default()),
        ));
    }
    let mut compared = Vec::new();
    let mut absent = Vec::new();
    for (key, value) in &expected.content {
        match receipt["content"].get(key) {
            Some(observed) if observed == value => compared.push(key.clone()),
            Some(observed) => {
                return Err(refused(
                    &format!("/expected/content/{key}"),
                    format!(
                        "receipt observed {observed}; values compare exactly, including letter case"
                    ),
                ));
            }
            None => absent.push(key.clone()),
        }
    }
    let mut limitations = Vec::new();
    let dropped = receipt["completion"]["dropped"].as_u64().unwrap_or(0);
    if status != "complete" || dropped > 0 {
        limitations.push("incomplete_capture");
    }
    let capability = text(receipt, "/capability/revision");
    if capability.is_none() {
        limitations.push("capability_unrecorded");
    }
    if expected
        .capability_revision
        .as_deref()
        .is_some_and(|r| capability.is_some_and(|c| c != r))
    {
        limitations.push("capability_revision_changed");
    }
    let build = text(receipt, "/producer/build");
    if expected
        .producer_build
        .as_deref()
        .is_some_and(|b| Some(b) != build)
    {
        limitations.push("stale_producer_build");
    }
    if compared.is_empty() {
        limitations.push("content_unverified");
    }
    let clock = &receipt["clock"];
    if clock["domain"].as_str().is_none() || clock["start"].is_null() || clock["end"].is_null() {
        limitations.push("clock_unrecorded");
    }
    let undescribed: Vec<&String> = files
        .keys()
        .filter(|path| !described.contains(&path.as_str()))
        .collect();
    if !undescribed.is_empty() {
        limitations.push("undescribed_members");
    }
    let checks = json!({
        "member_hashes": "every described member matched its retained bytes",
        "completion": status, "dropped": dropped,
        "content_keys_compared": compared, "expected_content_keys_absent": absent,
        "receipt_content_keys": receipt["content"].as_object().map(|c| c.keys().collect::<Vec<_>>()),
        "undescribed_members": undescribed,
        "producer_build": build, "capability_revision": capability,
    });
    Ok((limitations.into_iter().map(str::to_owned).collect(), checks))
}

impl Service {
    fn subject_exists(&mut self, subject: &ObservationSubject) -> Result<(), ToolResponse> {
        if subject.kind == "revision" {
            let project = subject
                .project
                .as_deref()
                .ok_or_else(|| refused("/subjects", "a revision subject needs its project"))?;
            return self.revision(project, Some(&subject.id)).map(|_| ());
        }
        let collection = match subject.kind.as_str() {
            "midi_source" => "midi_source",
            "sound_source" => "instrument_source",
            "instrument_bank" => "instrument_bank",
            "fm_bank" => "fm_bank",
            "sample_plan" => "sample_plan",
            "target_plan" => "target_plan",
            other => {
                return Err(refused(
                    "/subjects",
                    format!("unknown subject kind `{other}`"),
                ));
            }
        };
        let found = self
            .store_ref()?
            .reader()
            .document::<Value>("", collection, &subject.id)?;
        found.map(|_| ()).ok_or_else(|| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/subjects",
                format!("unknown {} `{}`", subject.kind, subject.id),
            )
        })
    }
    pub(super) fn attach_observation(&mut self, input: AttachObservationInput) -> Handled {
        let relative = Path::new(&input.directory);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(refused(
                "/directory",
                "choose a relative folder without parent components",
            ));
        }
        let root = self.workspace.join(relative);
        let is_dir = std::fs::symlink_metadata(&root)
            .map_err(|e| ToolResponse::error(e.to_string()))?
            .is_dir();
        if !is_dir {
            return Err(refused("/directory", "not a folder"));
        }
        let files = read_folder(&root)?;
        let receipt = match (input.producer_format.as_str(), input.receipt) {
            ("portable", Some(receipt)) => receipt,
            ("emucap_audio", None) => emucap_audio_receipt(&files)?,
            ("portable", None) => {
                return Err(refused(
                    "/receipt",
                    "a portable observation needs a receipt",
                ));
            }
            ("emucap_audio", Some(_)) => {
                return Err(refused("/receipt", "emucap_audio derives its receipt"));
            }
            _ => {
                return Err(refused(
                    "/producer_format",
                    "choose portable or emucap_audio",
                ));
            }
        };
        let (limitations, checks) = check_receipt(&receipt, &files, &input.expected)?;
        for subject in &input.subjects {
            self.subject_exists(subject)?;
        }
        let mut accepted = input.accepted_limitations.clone();
        accepted.sort();
        accepted.dedup();
        let mut required = limitations.clone();
        required.sort();
        if accepted != required {
            let mut message = format!(
                "inspect these limitations and accept exactly them to attach: {required:?}"
            );
            if required.iter().any(|k| k == "content_unverified") {
                message.push_str(&format!(
                    ". No expected content key matched a receipt key: expected {:?}, receipt has {}",
                    input.expected.content.keys().collect::<Vec<_>>(),
                    checks["receipt_content_keys"]
                ));
            }
            return Err(ToolResponse::refused(
                Code::TargetRefused,
                "/accepted_limitations",
                message,
            ));
        }
        let store = self.store_mut(true)?;
        let members = files
            .iter()
            .map(|(path, bytes)| Ok((path.clone(), store.put_object(bytes)?)))
            .collect::<Result<BTreeMap<_, _>, StoreError>>()?;
        let record = ObservationRecord {
            producer_format: input.producer_format,
            receipt,
            members,
            limitations,
            checks,
            subjects: input.subjects,
        };
        let id = sha256_hex(&serde_json::to_vec(&record).expect("serializes"));
        let response = ToolResponse::ok(
            "retained every observation file and recorded its receipt; this is evidence, not game or sound-equivalence authority",
            json!({"observation":id,"receipt":record.receipt,"limitations":record.limitations,
                "checks":record.checks,"subjects":record.subjects,"files":record.members.len()}),
        );
        commit(response, move |w| {
            w.put_document("", OBSERVATIONS, &id, &record)
        })
    }
    fn observation(
        &mut self,
        id: &str,
    ) -> Result<(ObservationRecord, BTreeMap<String, Vec<u8>>), ToolResponse> {
        let store = self.store_ref()?;
        let record: ObservationRecord =
            store
                .reader()
                .document("", OBSERVATIONS, id)?
                .ok_or_else(|| {
                    ToolResponse::refused(
                        Code::UnknownReference,
                        "/observation",
                        "unknown observation; view=observations lists saved IDs",
                    )
                })?;
        if sha256_hex(&serde_json::to_vec(&record).expect("serializes")) != id {
            return Err(ToolResponse::error("observation identity mismatch"));
        }
        let mut files = BTreeMap::new();
        for (path, hash) in &record.members {
            let bytes = std::fs::read(store.object_path(hash))
                .map_err(|e| ToolResponse::error(e.to_string()))?;
            if sha256_hex(&bytes) != *hash {
                return Err(ToolResponse::error("observation member identity mismatch"));
            }
            files.insert(path.clone(), bytes);
        }
        Ok((record, files))
    }
    pub(super) fn inspect_observation(&mut self, input: InspectObservationInput) -> Handled {
        let view = input.view.as_deref().unwrap_or("summary");
        let page = |total: usize| {
            let start = input.offset.min(total);
            let end = start
                .saturating_add(input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize)
                .min(total);
            (start, end)
        };
        if view == "observations" {
            let rows: Vec<(String, ObservationRecord)> = if Store::exists(&self.workspace) {
                self.store_ref()?
                    .reader()
                    .documents::<ObservationRecord>("", OBSERVATIONS)?
                    .into_iter()
                    .map(|r| (sha256_hex(&serde_json::to_vec(&r).expect("serializes")), r))
                    .filter(|(_, r)| {
                        input
                            .subject
                            .as_ref()
                            .is_none_or(|s| r.subjects.iter().any(|x| &x.id == s))
                    })
                    .collect()
            } else {
                vec![]
            };
            let (start, end) = page(rows.len());
            let items: Vec<Value> = rows[start..end]
                .iter()
                .map(|(id, r)| {
                    json!({"observation":id,"producer":r.receipt["producer"],
                        "completion":r.receipt["completion"]["status"],
                        "limitations":r.limitations,"subjects":r.subjects})
                })
                .collect();
            return done(ToolResponse::ok(
                "saved observations",
                json!({"items":items,"total":rows.len(),"next_cursor":(end<rows.len()).then_some(end)}),
            ));
        }
        let id = input.observation.as_deref().ok_or_else(|| {
            refused(
                "/observation",
                "select an observation; view=observations lists saved IDs",
            )
        })?;
        let (record, files) = self.observation(id)?;
        match view {
            "summary" => done(ToolResponse::ok(
                "observation receipt, checks and associations; identities verified",
                json!({"observation":id,"producer_format":record.producer_format,"receipt":record.receipt,
                    "limitations":record.limitations,"checks":record.checks,"subjects":record.subjects,
                    "files":record.members.len()}),
            )),
            "members" => {
                let rows: Vec<Value> = record
                    .members
                    .iter()
                    .map(
                        |(path, hash)| json!({"path":path,"sha256":hash,"bytes":files[path].len()}),
                    )
                    .collect();
                let (start, end) = page(rows.len());
                done(ToolResponse::ok(
                    "retained observation files",
                    json!({"observation":id,"items":&rows[start..end],"total":rows.len(),"next_cursor":(end<rows.len()).then_some(end)}),
                ))
            }
            "bytes" => {
                let member = input
                    .member
                    .as_deref()
                    .ok_or_else(|| refused("/member", "select a member path"))?;
                let bytes = files
                    .get(member)
                    .ok_or_else(|| refused("/member", "not a retained member"))?;
                let (start, end) = page(bytes.len());
                done(ToolResponse::ok(
                    "original observation bytes",
                    json!({"observation":id,"member":member,"offset":start,"bytes":&bytes[start..end],"total":bytes.len(),"next_cursor":(end<bytes.len()).then_some(end)}),
                ))
            }
            _ => Err(refused(
                "/view",
                "choose observations, summary, members or bytes",
            )),
        }
    }
    pub(super) fn export_observation(&mut self, input: ExportObservationInput) -> Handled {
        let (record, files) = self.observation(&input.observation)?;
        let mut out = vec![(
            "observation.json".to_owned(),
            serde_json::to_vec_pretty(&json!({"observation":input.observation,"record":record}))
                .expect("serializes"),
        )];
        out.extend(
            files
                .into_iter()
                .map(|(path, bytes)| (format!("members/{path}"), bytes)),
        );
        let mut response = self.export_sound_files(&input.job_id, &input.destination, out)?;
        response.summary = "exported exact observation files and receipt with readback".into();
        response.result["observation"] = json!(input.observation);
        done(response)
    }
}
