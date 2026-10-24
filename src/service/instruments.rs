//! Content-addressed instrument banks and exact, source-preserving transfers.
use super::*;
use crate::instruments::{BankLayout, InstrumentBank, InstrumentSettings, SamplePlacement};
use std::collections::BTreeMap;

pub(super) const BANKS: &str = "instrument_bank";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadInstrumentBankInput {
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank: Option<String>,
    /// Driver code and shared resident tables define tuning/envelope compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resident: Option<String>,
    /// Module file offsets from a loaded sound source; replaces all path inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveBankInput>,
    /// Additional sample modules, such as resident BRR. Inputs must agree where they overlap.
    #[serde(default)]
    pub sample_modules: Vec<String>,
    pub layout: BankLayout,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectInstrumentBankInput {
    #[serde(default)]
    pub bank: Option<String>,
    /// summary (default), samples, tracks, references, or banks to discover saved IDs.
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyInstrumentInput {
    pub job_id: String,
    pub source_bank: String,
    /// Donor track settings. Copy octave/envelope choices from its mapped track;
    /// BRR bytes alone cannot identify the musical octave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<InstrumentSettings>,
    /// Native source track index from inspect_instrument_bank view=tracks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_track: Option<usize>,
    /// Apply the returned binding to this native track's initial instrument header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_track: Option<usize>,
    /// Preserve the receiving melody's transpose, or explicitly copy the donor context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_transpose: Option<NativeTranspose>,
    /// Explicit unused table indices for missing exact pitch/envelope programs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_allocation: Option<crate::compile_instruments::TableAllocation>,
    pub destination_bank: String,
    /// Replace this directory slot in a new bank; the loaded bank stays immutable.
    pub destination_slot: u8,
    #[serde(default)]
    pub placement: SamplePlacement,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NativeTranspose {
    PreserveDestination,
    CopySource,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArchiveBankInput {
    pub source: String,
    pub template: usize,
    pub bank: usize,
    pub driver: usize,
    pub resident: usize,
    #[serde(default)]
    pub sample_modules: Vec<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportInstrumentBankInput {
    pub job_id: String,
    pub bank: String,
    pub destination: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct BankRecord {
    pub layout: BankLayout,
    pub assets: BTreeMap<String, String>,
    pub lineage: Value,
}

impl Service {
    pub(super) fn instrument_bank_data(
        &mut self,
        id: &str,
    ) -> Result<(InstrumentBank, BankRecord), ToolResponse> {
        let record: BankRecord = self.shared_document(BANKS, id)?.ok_or_else(|| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/bank",
                "unknown instrument bank; load it first",
            )
        })?;
        let store = self.store_ref()?;
        let record_bytes = serde_json::to_vec(&record).expect("serializes");
        if sha256_hex(&record_bytes) != id {
            return Err(ToolResponse::error("instrument bank identity mismatch"));
        }
        let mut assets = BTreeMap::new();
        for (name, hash) in &record.assets {
            let bytes = std::fs::read(store.object_path(hash))
                .map_err(|e| ToolResponse::error(e.to_string()))?;
            if sha256_hex(&bytes) != *hash {
                return Err(ToolResponse::error(format!(
                    "instrument asset {name} identity mismatch"
                )));
            }
            assets.insert(name.clone(), bytes);
        }
        let bank = InstrumentBank::new(record.layout.clone(), assets)
            .map_err(|e| ToolResponse::refused(Code::TargetRefused, "/bank", e))?;
        Ok((bank, record))
    }
    fn save_instrument_bank(&mut self, bank: InstrumentBank, lineage: Value) -> Handled {
        let summary = bank.summary();
        let store = self.store_mut(true)?;
        let assets = bank
            .assets
            .iter()
            .map(|(name, bytes)| Ok((name.clone(), store.put_object(bytes)?)))
            .collect::<Result<BTreeMap<_, _>, StoreError>>()?;
        let record = BankRecord {
            layout: bank.layout,
            assets,
            lineage,
        };
        let id = store.put_object(&serde_json::to_vec(&record).expect("serializes"))?;
        let response = ToolResponse::ok(
            "stored an immutable instrument bank with exact original sample bytes",
            json!({"bank":id,"summary":summary,"assets":record.assets,"lineage":record.lineage}),
        );
        commit(response, move |w| w.put_document("", BANKS, &id, &record))
    }
    pub(super) fn load_instrument_bank(&mut self, input: LoadInstrumentBankInput) -> Handled {
        let mut assets = BTreeMap::new();
        let provenance = if let Some(archive) = &input.archive {
            if input.template.is_some()
                || input.bank.is_some()
                || input.driver.is_some()
                || input.resident.is_some()
                || !input.sample_modules.is_empty()
            {
                return Err(ToolResponse::refused(
                    Code::InvalidRange,
                    "/archive",
                    "choose archive offsets or file paths, not both",
                ));
            }
            let bytes = self.sound_source_bytes(&archive.source)?;
            let mut offsets = vec![
                ("template.bin".to_string(), archive.template),
                ("bank.bin".into(), archive.bank),
                ("runtime-driver.bin".into(), archive.driver),
                ("runtime-data.bin".into(), archive.resident),
            ];
            offsets.extend(
                archive
                    .sample_modules
                    .iter()
                    .enumerate()
                    .map(|(i, at)| (format!("sample-{i}.bin"), *at)),
            );
            for (name, offset) in offsets {
                let module = crate::instrument_source::module_at(&bytes, offset)
                    .map_err(|e| ToolResponse::refused(Code::InvalidRange, "/archive", e))?;
                assets.insert(name, bytes[offset..offset + module.bytes].to_vec());
            }
            json!({"source":archive.source,"offsets":archive})
        } else {
            let (Some(template), Some(bank), Some(driver), Some(resident)) =
                (input.template, input.bank, input.driver, input.resident)
            else {
                return Err(ToolResponse::refused(
                    Code::InvalidRange,
                    "/archive",
                    "supply archive offsets or template, bank, driver and resident paths",
                ));
            };
            let mut paths = vec![
                ("template.bin".to_string(), template),
                ("bank.bin".into(), bank),
                ("runtime-driver.bin".into(), driver),
                ("runtime-data.bin".into(), resident),
            ];
            paths.extend(
                input
                    .sample_modules
                    .into_iter()
                    .enumerate()
                    .map(|(i, path)| (format!("sample-{i}.bin"), path)),
            );
            for (name, path) in paths {
                let bytes = std::fs::read(self.workspace.join(&path))
                    .map_err(|e| ToolResponse::error(format!("cannot read {path}: {e}")))?;
                assets.insert(name, bytes);
            }
            Value::Null
        };
        let bank = InstrumentBank::new(input.layout, assets)
            .map_err(|e| ToolResponse::refused(Code::InvalidRange, "/layout", e))?;
        self.save_instrument_bank(bank,json!({"kind":"loaded","archive":provenance,"preservation":"all supplied module bytes retained; unresolvable sample entries remain inspectable"}))
    }
    pub(super) fn inspect_instrument_bank(&mut self, input: InspectInstrumentBankInput) -> Handled {
        if input.view.as_deref() == Some("banks") {
            let records: Vec<BankRecord> = if Store::exists(&self.workspace) {
                self.store_ref()?.reader().documents("", BANKS)?
            } else {
                vec![]
            };
            let start = input.offset.min(records.len());
            let end =
                (start + input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize).min(records.len());
            let banks: Vec<_> = records[start..end]
                .iter()
                .map(|record| {
                    json!({
                        "bank": sha256_hex(&serde_json::to_vec(record).expect("serializes")),
                        "layout": record.layout, "origin": record.lineage["kind"],
                        "binding": record.lineage.pointer("/transfer/binding")
                    })
                })
                .collect();
            return done(ToolResponse::ok(
                "saved instrument banks",
                json!({"banks":banks,"total":records.len(),"next_cursor":(end<records.len()).then_some(end)}),
            ));
        }
        let id = input.bank.as_deref().ok_or_else(|| {
            ToolResponse::refused(
                Code::InvalidRange,
                "/bank",
                "this view needs a bank ID; use view=banks to discover IDs",
            )
        })?;
        let (bank, record) = self.instrument_bank_data(id)?;
        match input.view.as_deref().unwrap_or("summary") {
            "summary" => done(ToolResponse::ok(
                "instrument bank and transfer provenance",
                json!({"bank":input.bank,"summary":bank.summary(),"interpretation":crate::compile_instruments::describe(&bank).unwrap_or_else(|reason|json!({"available":false,"reason":reason})),"assets":record.assets,"lineage":record.lineage}),
            )),
            "references" => {
                let references = crate::compile_instruments::used_indices(&bank)
                    .map_err(|e| ToolResponse::refused(Code::TargetRefused, "/bank", e))?;
                done(ToolResponse::ok(
                    "referenced runtime table indices",
                    json!({"bank":input.bank,"references":references}),
                ))
            }
            "tracks" => {
                let tracks = crate::compile_instruments::native_tracks(&bank)
                    .map_err(|e| ToolResponse::refused(Code::TargetRefused, "/bank", e))?;
                done(ToolResponse::ok(
                    "native initial instrument bindings",
                    json!({"bank":input.bank,"tracks":tracks,"scope":"initial headers; subsequent score commands are retained"}),
                ))
            }
            "samples" => {
                let samples = bank.samples();
                let start = input.offset.min(samples.len());
                let end = (start + input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize)
                    .min(samples.len());
                done(ToolResponse::ok(
                    "exact BRR samples and opaque driver tuning",
                    json!({"bank":input.bank,"items":&samples[start..end],"total":samples.len(),"next_cursor":(end<samples.len()).then_some(end)}),
                ))
            }
            _ => done(ToolResponse::refused(
                Code::InvalidRange,
                "/view",
                "choose summary, samples, tracks, references, or banks",
            )),
        }
    }
    pub(super) fn copy_instrument(&mut self, input: CopyInstrumentInput) -> Handled {
        let (source, _) = self.instrument_bank_data(&input.source_bank)?;
        let (destination, _) = self.instrument_bank_data(&input.destination_bank)?;
        let settings = match (&input.instrument, input.source_track) {
            (Some(settings), None) => settings.clone(),
            (None, Some(index)) => {
                let tracks = crate::compile_instruments::native_tracks(&source)
                    .map_err(|e| ToolResponse::refused(Code::TargetRefused, "/source_track", e))?;
                let track = tracks.get(index).ok_or_else(|| {
                    ToolResponse::refused(
                        Code::InvalidRange,
                        "/source_track",
                        "native track index is outside the loaded song",
                    )
                })?;
                if track.flags & 8 != 0 {
                    return Err(ToolResponse::refused(
                        Code::TargetRefused,
                        "/source_track",
                        "source track starts in noise mode; select a tonal BRR instrument",
                    ));
                }
                track.settings.clone()
            }
            _ => {
                return Err(ToolResponse::refused(
                    Code::InvalidRange,
                    "/instrument",
                    "choose instrument settings or a source_track, not both",
                ));
            }
        };
        let (prepared, allocation) = if let Some(selected) = &input.table_allocation {
            crate::compile_instruments::prepare_destination(
                &destination,
                &source,
                &settings,
                selected,
            )
            .map_err(|e| ToolResponse::refused(Code::TargetRefused, "/table_allocation", e))?
        } else {
            (
                InstrumentBank::new(destination.layout.clone(), destination.assets.clone())
                    .expect("validated"),
                json!({"allocated":false}),
            )
        };
        let (mut bank, mut transfer) = prepared
            .copy_from(&source, &settings, input.destination_slot, input.placement)
            .map_err(|e| ToolResponse::refused(Code::TargetRefused, "/instrument", e))?;
        transfer["table_allocation"] = allocation;
        if let Some(index) = input.destination_track {
            let mut binding: InstrumentSettings =
                serde_json::from_value(transfer["binding"].clone()).expect("binding");
            if !matches!(input.native_transpose, Some(NativeTranspose::CopySource)) {
                let tracks =
                    crate::compile_instruments::native_tracks(&destination).map_err(|e| {
                        ToolResponse::refused(Code::TargetRefused, "/destination_track", e)
                    })?;
                binding.transpose = tracks
                    .get(index)
                    .ok_or_else(|| {
                        ToolResponse::refused(
                            Code::InvalidRange,
                            "/destination_track",
                            "native track index is outside the loaded song",
                        )
                    })?
                    .settings
                    .transpose;
            }
            let (applied, mut report) = bank
                .apply_native_binding(index, &binding)
                .map_err(|e| ToolResponse::refused(Code::TargetRefused, "/destination_track", e))?;
            report["transpose_policy"] = json!(
                input
                    .native_transpose
                    .unwrap_or(NativeTranspose::PreserveDestination)
            );
            bank = applied;
            transfer["native_track"] = report;
        }
        self.save_instrument_bank(bank,json!({"kind":"instrument_copy","source_bank":input.source_bank,"source_track":input.source_track,"destination_bank":input.destination_bank,"transfer":transfer}))
    }

    pub(super) fn export_instrument_bank(&mut self, input: ExportInstrumentBankInput) -> Handled {
        let (bank, record) = self.instrument_bank_data(&input.bank)?;
        let mut files: Vec<_> = bank.assets.into_iter().collect();
        files.push(("bank.json".into(),serde_json::to_vec_pretty(&json!({"bank":input.bank,"record":record,"scope":"native module delivery; ROM insertion and playback are separate"})).expect("serializes")));
        let mut response = self.export_sound_files(&input.job_id, &input.destination, files)?;
        response.result["bank"] = json!(input.bank);
        done(response)
    }
    pub(super) fn export_sound_files(
        &mut self,
        job: &str,
        destination: &str,
        files: Vec<(String, Vec<u8>)>,
    ) -> Result<ToolResponse, ToolResponse> {
        use std::io::Write;
        let relative = Path::new(destination);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(ToolResponse::refused(
                Code::InvalidRange,
                "/destination",
                "choose a new relative folder without parent components",
            ));
        }
        let destination = self.workspace.join(relative);
        if destination.exists() {
            return Err(ToolResponse::refused(
                Code::DuplicateId,
                "/destination",
                "destination exists",
            ));
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ToolResponse::error(e.to_string()))?;
        }
        let staging = destination.with_file_name(format!(
            ".tunemend-bank-{}-{}",
            std::process::id(),
            sha256_hex(job.as_bytes())
        ));
        std::fs::create_dir(&staging).map_err(|e| ToolResponse::error(e.to_string()))?;
        let outcome = (|| -> std::io::Result<()> {
            for (name, bytes) in &files {
                let path = staging.join(name);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                if std::fs::read(path)? != *bytes {
                    return Err(std::io::Error::other("sound export readback differs"));
                }
            }
            if destination.exists() {
                return Err(std::io::Error::other("destination appeared during export"));
            }
            std::fs::rename(&staging, &destination)
        })();
        if let Err(e) = outcome {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(ToolResponse::error(e.to_string()));
        }
        let mut response = ToolResponse::ok(
            "exported exact sound data with readback",
            json!({"readback_checked":true,"runtime_verified":false}),
        );
        response.artifacts = files
            .into_iter()
            .map(|(name, bytes)| Artifact {
                kind: name.clone(),
                path: destination.join(name).display().to_string(),
                sha256: sha256_hex(&bytes),
            })
            .collect();
        Ok(response)
    }
}
