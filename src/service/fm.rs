//! Persistent FM banks use the common original-source and export mechanisms.
use super::*;
use crate::fm::{FmBank, FmLayout, OpnChip, SCOPE};

pub(super) const FM_BANKS: &str = "fm_bank";
use FM_BANKS as BANKS;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadFmBankInput {
    pub job_id: String,
    /// Exactly one of path or a saved load_instrument_source ID.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    pub chip: OpnChip,
    pub layout: FmLayout,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectFmBankInput {
    #[serde(default)]
    pub bank: Option<String>,
    /// summary (default), records, banks, or layouts (no bank required).
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<u32>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyFmPatchInput {
    pub job_id: String,
    pub source_bank: String,
    pub source_index: usize,
    pub destination_bank: String,
    /// Explicit replacement; all songs referencing this record will use the new patch.
    pub destination_index: usize,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportFmBankInput {
    pub job_id: String,
    pub bank: String,
    pub destination: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FmRecord {
    pub(super) asset: String,
    chip: OpnChip,
    layout: FmLayout,
    lineage: Value,
}
fn refused(message: String) -> ToolResponse {
    ToolResponse::refused(Code::TargetRefused, "/fm_bank", message)
}

impl Service {
    /// A saved bank with verified identities, for read-only consumers such as audition.
    pub(super) fn saved_fm_bank(&mut self, id: &str) -> Result<FmBank, ToolResponse> {
        self.fm_bank_data(id).map(|(bank, _)| bank)
    }
    pub(super) fn fm_bank_data(&mut self, id: &str) -> Result<(FmBank, FmRecord), ToolResponse> {
        let record: FmRecord = self.shared_document(BANKS, id)?.ok_or_else(|| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/bank",
                "unknown FM bank; list inspect_fm_bank view=banks",
            )
        })?;
        let store = self.store_ref()?;
        if sha256_hex(&serde_json::to_vec(&record).expect("serializes")) != id {
            return Err(ToolResponse::error("FM bank identity mismatch"));
        }
        let bytes = std::fs::read(store.object_path(&record.asset))
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if sha256_hex(&bytes) != record.asset {
            return Err(ToolResponse::error("FM container identity mismatch"));
        }
        let bank = FmBank::new(bytes, record.layout.clone(), record.chip).map_err(refused)?;
        Ok((bank, record))
    }
    fn save_fm_bank(&mut self, bank: FmBank, lineage: Value) -> Handled {
        let store = self.store_mut(true)?;
        let asset = store.put_object(&bank.bytes)?;
        let record = FmRecord {
            asset,
            chip: bank.chip,
            layout: bank.layout,
            lineage,
        };
        let id = store.put_object(&serde_json::to_vec(&record).expect("serializes"))?;
        let response = ToolResponse::ok(
            "stored an immutable OPN register bank and its complete container",
            json!({"bank":id,"asset":record.asset,"records":record.layout.record_offsets.len(),"lineage":record.lineage,"scope":SCOPE}),
        );
        commit(response, move |w| w.put_document("", BANKS, &id, &record))
    }
    pub(super) fn load_fm_bank(&mut self, input: LoadFmBankInput) -> Handled {
        let bytes = match (&input.source, &input.path) {
            (Some(id), None) => self.sound_source_bytes(id)?,
            (None, Some(path)) => std::fs::read(self.workspace.join(path))
                .map_err(|e| ToolResponse::error(e.to_string()))?,
            _ => {
                return Err(refused(
                    "provide exactly one path or saved source ID".into(),
                ));
            }
        };
        let hash = sha256_hex(&bytes);
        if input
            .expected_sha256
            .is_some_and(|expected| expected != hash)
        {
            return Err(refused("source identity differs".into()));
        }
        let bank = FmBank::new(bytes, input.layout, input.chip).map_err(refused)?;
        self.save_fm_bank(bank,json!({"kind":"loaded","source_sha256":hash,"saved_source":input.source,"layout_authority":"caller supplied; layout is not inferred from game identity"}))
    }
    pub(super) fn inspect_fm_bank(&mut self, input: InspectFmBankInput) -> Handled {
        let view = input.view.as_deref().unwrap_or("summary");
        if view == "layouts" {
            return done(ToolResponse::ok(
                "OPN register bank layout guide",
                json!({
                "scope":SCOPE,"family":"opn","chips":["opn","ym2203","ym2608","ym2612"],
                "operator_register_groups":[48,64,80,96,112,128,144],"register_slot_offsets":[0,4,8,12],"named_operators":[1,3,2,4],
                "packing":["operator_rows","register_columns"],
                "example":{"record_offsets":[0,32],"record_size":32,"algorithm_offset":28,"operators_offset":0,"packing":"register_columns","slot_order":[0,1,2,3],"reserved_records":[]},
                "requirements":"Supply verified absolute record offsets and field layout. The example is synthetic, not source discovery. All original bytes are retained, including opaque metadata and reserved bits. Missing/unknown layouts can still use load_instrument_source.",
                "limits":"Complete 29-register patches only; no automatic ROM scan, reduced-register formats, native binding rewrite, dynamic state transfer or hardware audio. render_fm_patch auditions a record through the built-in model with declared context."}),
            ));
        }
        if view == "banks" {
            let records: Vec<FmRecord> = if Store::exists(&self.workspace) {
                self.store_ref()?.reader().documents("", BANKS)?
            } else {
                vec![]
            };
            let start = input.offset.min(records.len());
            let end = start
                .saturating_add(input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize)
                .min(records.len());
            let rows:Vec<_>=records[start..end].iter().map(|r|json!({"bank":sha256_hex(&serde_json::to_vec(r).expect("serializes")),"chip":r.chip,"asset":r.asset,"records":r.layout.record_offsets.len()})).collect();
            return done(ToolResponse::ok(
                "saved FM banks",
                json!({"items":rows,"total":records.len(),"next_cursor":(end<records.len()).then_some(end)}),
            ));
        }
        let id = input
            .bank
            .ok_or_else(|| refused("select a bank; view=banks lists saved IDs".into()))?;
        let (bank, record) = self.fm_bank_data(&id)?;
        match view {
            "summary" => done(ToolResponse::ok(
                "complete FM container with declared register interpretation",
                json!({"bank":id,"bytes":bank.bytes.len(),"records":bank.layout.record_offsets.len(),"record":record,"scope":SCOPE}),
            )),
            "records" => {
                let start = input.offset.min(bank.layout.record_offsets.len());
                let end = start
                    .saturating_add(input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize)
                    .min(bank.layout.record_offsets.len());
                let items = (start..end)
                    .map(|i| bank.inspect(i))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(refused)?;
                done(ToolResponse::ok(
                    "exact raw records and decoded OPN register parameters",
                    json!({"bank":id,"items":items,"total":bank.layout.record_offsets.len(),"next_cursor":(end<bank.layout.record_offsets.len()).then_some(end)}),
                ))
            }
            _ => Err(refused("choose summary, records, banks or layouts".into())),
        }
    }
    pub(super) fn copy_fm_patch(&mut self, input: CopyFmPatchInput) -> Handled {
        let (source, _) = self.fm_bank_data(&input.source_bank)?;
        let (destination, _) = self.fm_bank_data(&input.destination_bank)?;
        let (copied, transfer) = destination
            .copy_patch(&source, input.source_index, input.destination_index)
            .map_err(refused)?;
        self.save_fm_bank(copied,json!({"kind":"fm_patch_copy","source_bank":input.source_bank,"destination_bank":input.destination_bank,"transfer":transfer}))
    }
    pub(super) fn export_fm_bank(&mut self, input: ExportFmBankInput) -> Handled {
        let (bank, record) = self.fm_bank_data(&input.bank)?;
        let patches = (0..bank.layout.record_offsets.len())
            .map(|i| bank.inspect(i))
            .collect::<Result<Vec<_>, _>>()
            .map_err(refused)?;
        let files = vec![
            ("container.bin".into(), bank.bytes),
            (
                "bank.json".into(),
                serde_json::to_vec_pretty(
                    &json!({"bank":input.bank,"record":record,"scope":SCOPE}),
                )
                .expect("serializes"),
            ),
            (
                "patches.json".into(),
                serde_json::to_vec_pretty(&patches).expect("serializes"),
            ),
        ];
        let mut response = self.export_sound_files(&input.job_id, &input.destination, files)?;
        response.result["bank"] = json!(input.bank);
        done(response)
    }
}
