//! `transform_vgm`: a new retained VGM derived from a retained one with
//! declared changes; the original is never changed.
use super::instrument_sources::{SOURCES, SoundSourceRecord};
use super::*;
use crate::vgm::Vgm;
use crate::vgm_transform::{
    CopyChanges, LossCode, RESPONSE_CHANGES, Request, SnToAyMapping, transform,
};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TransformVgmInput {
    pub job_id: String,
    /// A retained VGM sound source (load_instrument_source). It is never
    /// changed; the result is a new retained source.
    pub source: String,
    /// A same-family copy with declared changes. Give exactly one of `copy`
    /// or `map_sn76489_to_ay`.
    #[serde(default)]
    pub copy: Option<CopyChanges>,
    /// The explicit SN76489 to AY-3-8910/YM2149 mapping.
    #[serde(default)]
    pub map_sn76489_to_ay: Option<SnToAyMapping>,
    /// Loss codes you accept. A transformation with any other loss refuses
    /// and lists each loss with its count and first command; repeat the call
    /// with a new job_id and the codes you accept.
    #[serde(default)]
    pub accept_losses: Option<Vec<LossCode>>,
    /// New folder relative to the workspace for the VGM and
    /// `transform.json`, such as `out/vgm-transposed`.
    pub destination: String,
    /// File name of the written VGM, such as `jingle-msx.vgm` (default
    /// `transformed.vgm`); a plain name ending in `.vgm`. It also names the
    /// new retained source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vgm_name: Option<String>,
}

impl Service {
    pub(super) fn transform_vgm(&mut self, input: TransformVgmInput) -> Handled {
        let bytes = self.sound_source_bytes(&input.source)?;
        let vgm = Vgm::parse(&bytes).map_err(|e| super::psg::located("/source", e))?;
        let vgm_name = input.vgm_name.as_deref().unwrap_or("transformed.vgm");
        let plain = !vgm_name.is_empty()
            && vgm_name.len() <= 128
            && !vgm_name.starts_with('.')
            && !vgm_name.contains(['/', '\\', ':'])
            && vgm_name.ends_with(".vgm")
            && vgm_name != "transform.json";
        if !plain {
            return Err(ToolResponse::refused(
                Code::InvalidRange,
                "/vgm_name",
                "give a plain file name ending in .vgm, without a folder, such as jingle-msx.vgm",
            ));
        }
        let request = match (&input.copy, &input.map_sn76489_to_ay) {
            (Some(copy), None) => Request::Copy(copy),
            (None, Some(map)) => Request::Map(map),
            _ => {
                return Err(ToolResponse::refused(
                    Code::InvalidRange,
                    "/copy",
                    "give exactly one of copy or map_sn76489_to_ay",
                ));
            }
        };
        let outcome = transform(&vgm, request).map_err(|refusal| {
            if refusal.location.is_empty() {
                ToolResponse::error(refusal.message)
            } else {
                ToolResponse::refused(Code::TargetRefused, &refusal.location, refusal.message)
            }
        })?;
        let mut report = outcome.report;
        let accepted = input.accept_losses.clone().unwrap_or_default();
        let unaccepted: Vec<LossCode> = outcome
            .losses
            .iter()
            .filter(|code| !accepted.contains(code))
            .copied()
            .collect();
        if !unaccepted.is_empty() {
            let rows = report["losses"].as_array().cloned().unwrap_or_default();
            let diagnostics = rows
                .iter()
                .filter(|row| {
                    unaccepted
                        .iter()
                        .any(|code| row["code"] == json!(code))
                })
                .map(|row| Diagnostic {
                    code: Code::TargetRefused,
                    location: "/accept_losses".into(),
                    message: format!(
                        "{}: {} ({} occurrence(s), first at command {}, byte {}); list it in accept_losses to proceed",
                        row["code"].as_str().unwrap_or(""),
                        row["detail"].as_str().unwrap_or(""),
                        row["occurrences"],
                        row["first_command_index"],
                        row["first_offset"]
                    ),
                })
                .collect();
            let names: Vec<String> = unaccepted.iter().map(|c| c.name()).collect();
            let mut response = ToolResponse::blocked(
                format!(
                    "refused: {} loss(es) not accepted ({}); nothing was written",
                    names.len(),
                    names.join(", ")
                ),
                diagnostics,
            );
            response.result = json!({"source":input.source,"written":false,"unaccepted":unaccepted,
                "losses":report["losses"],"operation":report["operation"],"header_changes":report["header_changes"],
                "commands":report["commands"]});
            return Err(response);
        }
        if outcome.bytes == bytes {
            let mut response = ToolResponse::refused(
                Code::EmptyEdit,
                "/copy",
                "the declared changes leave every byte unchanged (for example a channel that is never written, or the clock or variant the file already declares); the retained original already is the result",
            );
            response.result =
                json!({"source":input.source,"written":false,"operation":report["operation"]});
            return Err(response);
        }
        let hash = sha256_hex(&outcome.bytes);
        let original: Option<SoundSourceRecord> = self.shared_document(SOURCES, &input.source)?;
        let stem = original
            .as_ref()
            .map(|r| r.name.trim_end_matches(".vgm").to_owned())
            .unwrap_or_else(|| "source".into());
        report["source"] = json!(hash);
        report["from"] = json!(input.source);
        report["accepted_losses"] = json!(accepted);
        report["files"] = json!({"vgm":vgm_name,"report":"transform.json"});
        let report_bytes = serde_json::to_vec_pretty(&report).expect("serializes");
        let mut response = self.export_sound_files(
            &input.job_id,
            &input.destination,
            vec![
                (vgm_name.into(), outcome.bytes.clone()),
                ("transform.json".into(), report_bytes.clone()),
            ],
        )?;
        let store = self.store_mut(true)?;
        store.put_object(&outcome.bytes)?;
        let report_hash = store.put_object(&report_bytes)?;
        let record = SoundSourceRecord {
            source: hash.clone(),
            name: match &input.vgm_name {
                Some(name) => name.clone(),
                None => format!("{stem}-{}.vgm", input.job_id),
            },
            bytes: outcome.bytes.len(),
            derived_from: Some(
                json!({"source":input.source,"tool":"transform_vgm","job_id":input.job_id,
                "report_sha256":report_hash,"operation":report["operation"]["kind"]}),
            ),
        };
        let changes = report["changes"].as_array().cloned().unwrap_or_default();
        let changed = report["commands"].clone();
        let summary = format!(
            "derived a new VGM source from {}: {} of {} commands unchanged; {} commands in {} changed group(s) replaced by {}; {} header field(s) changed; losses accepted: {}",
            &input.source[..12.min(input.source.len())],
            changed["unchanged"],
            changed["original"],
            changed["replaced"],
            changed["changed_groups"],
            changed["replacement_commands"],
            report["header_changes"].as_array().map_or(0, Vec::len),
            if outcome.losses.is_empty() {
                "none".to_owned()
            } else {
                outcome
                    .losses
                    .iter()
                    .map(|c| c.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        report["changes"] = json!(&changes[..changes.len().min(RESPONSE_CHANGES)]);
        report["changes_note"] = json!(format!(
            "first {} of {} changed command groups; transform.json lists all",
            changes.len().min(RESPONSE_CHANGES),
            changes.len()
        ));
        report["next"] = json!(
            "inspect_vgm reads the new source back; render_psg renders source and result, and compare_audio compares the two WAVs"
        );
        report["original_unchanged"] = json!(true);
        response.summary = summary;
        response.result = report;
        response.artifacts.push(Artifact {
            kind: "derived_vgm_source".into(),
            path: store.object_path(&hash).display().to_string(),
            sha256: hash.clone(),
        });
        commit(response, move |writer| {
            writer.put_document("", SOURCES, &hash, &record)
        })
    }
}
