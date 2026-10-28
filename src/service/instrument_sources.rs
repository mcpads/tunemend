//! Exact sound-source retention, with optional structural module discovery.
use super::*;

pub(super) const SOURCES: &str = "instrument_source";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadInstrumentSourceInput {
    pub job_id: String,
    pub path: String,
    #[serde(default)]
    pub expected_sha256: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectInstrumentSourceInput {
    #[serde(default)]
    pub source: Option<String>,
    /// sources, summary (default), modules, banks, or bytes.
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<u32>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportInstrumentSourceInput {
    pub job_id: String,
    pub source: String,
    pub destination: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SoundSourceRecord {
    pub(super) source: String,
    pub(super) name: String,
    pub(super) bytes: usize,
    /// For a source written by a transformation: what it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) derived_from: Option<Value>,
}
impl Service {
    pub(super) fn sound_source_bytes(&mut self, id: &str) -> Result<Vec<u8>, ToolResponse> {
        let record: SoundSourceRecord = self.shared_document(SOURCES, id)?.ok_or_else(|| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/source",
                "unknown sound source; call load_instrument_source",
            )
        })?;
        let store = self.store_ref()?;
        let bytes = std::fs::read(store.object_path(&record.source))
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if record.source != id || sha256_hex(&bytes) != id || bytes.len() != record.bytes {
            return Err(ToolResponse::error("sound source identity mismatch"));
        }
        Ok(bytes)
    }
    pub(super) fn load_instrument_source(&mut self, input: LoadInstrumentSourceInput) -> Handled {
        let path = self.workspace.join(&input.path);
        let bytes = std::fs::read(&path).map_err(|e| ToolResponse::error(e.to_string()))?;
        let hash = sha256_hex(&bytes);
        if input
            .expected_sha256
            .is_some_and(|expected| expected != hash)
        {
            return Err(ToolResponse::refused(
                Code::InvalidRange,
                "/expected_sha256",
                "source identity differs",
            ));
        }
        let record = SoundSourceRecord {
            source: hash.clone(),
            bytes: bytes.len(),
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            derived_from: None,
        };
        let discovery = discovery_summary(&bytes, Some(&record.name));
        let store = self.store_mut(true)?;
        store.put_object(&bytes)?;
        let mut response = ToolResponse::ok(
            "retained every original byte; module discovery is an optional interpretation",
            json!({"source":hash,"bytes":bytes.len(),"discovery":discovery}),
        );
        response.artifacts.push(Artifact {
            kind: "original_instrument_source".into(),
            path: store.object_path(&hash).display().to_string(),
            sha256: hash.clone(),
        });
        commit(response, move |writer| {
            writer.put_document("", SOURCES, &hash, &record)
        })
    }
    pub(super) fn inspect_instrument_source(
        &mut self,
        input: InspectInstrumentSourceInput,
    ) -> Handled {
        let view = input.view.as_deref().unwrap_or("summary");
        if view == "sources" {
            let records: Vec<SoundSourceRecord> = if Store::exists(&self.workspace) {
                self.store_ref()?.reader().documents("", SOURCES)?
            } else {
                vec![]
            };
            return done(ToolResponse::ok(
                "saved sound sources",
                json!({"sources":records}),
            ));
        }
        let id = input.source.as_deref().ok_or_else(|| {
            ToolResponse::refused(
                Code::InvalidRange,
                "/source",
                "select a source; view=sources lists saved IDs",
            )
        })?;
        let bytes = self.sound_source_bytes(id)?;
        if view == "bytes" {
            let start = input.offset.min(bytes.len());
            let end = start
                .saturating_add(input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize)
                .min(bytes.len());
            return done(ToolResponse::ok(
                "original sound-source bytes",
                json!({"source":id,"offset":start,"bytes":&bytes[start..end],"total":bytes.len(),"next_cursor":(end<bytes.len()).then_some(end)}),
            ));
        }
        let name = self
            .shared_document::<SoundSourceRecord>(SOURCES, id)?
            .map(|record| record.name);
        let discovery = discovery(&bytes, name.as_deref());
        match view {
            "summary" => done(ToolResponse::ok(
                "sound-source discovery",
                json!({"source":id,"bytes":bytes.len(),"discovery":discovery_summary(&bytes, name.as_deref())}),
            )),
            "modules" | "banks" => {
                let rows: Vec<Value> = discovery["catalogs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|catalog| catalog[view].as_array().into_iter().flatten().cloned())
                    .collect();
                let start = input.offset.min(rows.len());
                let end = start
                    .saturating_add(input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize)
                    .min(rows.len());
                done(ToolResponse::ok(
                    "discovered module ranges",
                    json!({"source":id,"items":&rows[start..end],"total":rows.len(),"next_cursor":(end<rows.len()).then_some(end)}),
                ))
            }
            _ => Err(ToolResponse::refused(
                Code::InvalidRange,
                "/view",
                "choose sources, summary, modules, banks or bytes",
            )),
        }
    }
    pub(super) fn export_instrument_source(
        &mut self,
        input: ExportInstrumentSourceInput,
    ) -> Handled {
        let bytes = self.sound_source_bytes(&input.source)?;
        let mut response = self.export_sound_files(
            &input.job_id,
            &input.destination,
            vec![("original.bin".into(), bytes)],
        )?;
        response.result["source"] = json!(input.source);
        done(response)
    }
}
fn discovery(bytes: &[u8], name: Option<&str>) -> Value {
    let catalogs = crate::instrument_source::discover_compile_sources(bytes);
    let mut formats = format_hints(bytes);
    if formats.is_empty() {
        formats.extend(body_name_hint(name).or_else(|| packed_dac_name_hint(name, bytes.len())));
    }
    // A LoROM image is a whole number of 32 KiB banks, optionally after a
    // 512-byte copier header.
    let snes_sized = bytes.len() >= 0x8000 && matches!(bytes.len() % 0x8000, 0 | 512);
    let interpretation = if !catalogs.is_empty() {
        "structural Compile LoROM candidates; select modules and an explicit layout before copying"
    } else if !formats.is_empty() {
        "see formats for the tools that interpret this file"
    } else if snes_sized {
        "no structural Compile LoROM catalog found and no other format recognized; every byte is retained"
    } else {
        "no format recognized; every byte is retained and tools that take offset and length can read slices of it"
    };
    json!({"catalogs":catalogs,"interpretation":interpretation,"formats":formats})
}
/// A file named like a VAB body: a VB has no header, so its bytes alone do
/// not identify it.
fn body_name_hint(name: Option<&str>) -> Option<Value> {
    let name = name?;
    name.to_ascii_lowercase().ends_with(".vb").then(|| {
        json!({"format":"vab_body_candidate","load_with":"load_vab_bank","note":"named like a VAB body (VB); a VB has no header and is not recognized from its bytes. Retain its VH as well and pass both to load_vab_bank as header and body"})
    })
}
/// A file named `.packed4`: headerless, so only its name points at md_dac4,
/// and the layout, rate and extent still have to be declared.
fn packed_dac_name_hint(name: Option<&str>, bytes: usize) -> Option<Value> {
    let name = name?;
    name.to_ascii_lowercase().ends_with(".packed4").then(|| {
        json!({"format":"md_dac4_candidate","basis":"file name only; the bytes have no header and are not recognized, so this is not a format identification",
            "convert_with":"prepare_sample","encoding":"md_dac4","samples_if_whole_file":bytes * 2,
            "declare":["md_dac.nibble_order (the driver's choice; not in the data)","sample_rate (the driver's write rate; not in the data)","offset and length when the file is not exactly one sample"],
            "note":"see the md_dac4 section of the sample conversion reference for choosing and reporting these"})
    })
}
/// Container signatures that another tool interprets.
fn format_hints(bytes: &[u8]) -> Vec<Value> {
    if bytes.starts_with(b"Vgm ") {
        vec![json!({"format":"vgm","inspect_with":"inspect_vgm","render_with":"render_psg"})]
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        vec![
            json!({"format":"wav","clip_with":"edit_project add_clip","convert_with":"prepare_sample","encoding":"wav","wav":crate::sample_codec::wav_facts(bytes)}),
        ]
    } else if bytes.starts_with(b"VAGp") {
        let mut hint = json!({"format":"vag","convert_with":"prepare_sample","encoding":"vag"});
        match crate::psx_adpcm::Vag::parse(bytes) {
            Ok(vag) => {
                let header = vag.report();
                for key in [
                    "version",
                    "version_documented",
                    "data_size",
                    "sample_rate",
                    "name",
                    "trailing_bytes",
                ] {
                    hint[key] = header[key].clone();
                }
                hint["sample_rate_note"] = json!("declared by the header; pass it as sample_rate");
            }
            Err(e) => hint["unusable"] = json!(e),
        }
        vec![hint]
    } else if bytes.starts_with(b"VAGi") || bytes.starts_with(b"pGAV") {
        vec![
            json!({"format":"vag_variant","magic":String::from_utf8_lossy(&bytes[..4]),"note":"interleaved or byte-reversed VAG is retained but not interpreted; prepare_sample admits only VAGp"}),
        ]
    } else if bytes.len() >= 4 && bytes[0] == 0x80 && bytes[1] == 0x00 {
        vec![super::adx::discovery_hint(bytes)]
    } else if let Some(hint) = crate::nds_sound::discovery_hint(bytes) {
        vec![hint]
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        vec![
            json!({"format":"gzip","note":"a VGZ file must be decompressed to .vgm before inspect_vgm"}),
        ]
    } else {
        // A container may hold both VAB banks and sequences.
        [
            crate::vab::discovery_hint(bytes),
            crate::psx_seq::discovery_hint(bytes),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}
fn discovery_summary(bytes: &[u8], name: Option<&str>) -> Value {
    let mut result = discovery(bytes, name);
    for catalog in result["catalogs"].as_array_mut().expect("array") {
        let modules = catalog
            .as_object_mut()
            .expect("object")
            .remove("modules")
            .expect("modules");
        catalog["module_count"] = json!(modules.as_array().expect("array").len());
        let banks = catalog
            .as_object_mut()
            .expect("object")
            .remove("banks")
            .expect("banks");
        catalog["bank_candidates"] = json!(banks.as_array().expect("array").len());
    }
    result
}
