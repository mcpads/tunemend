//! PlayStation VAB banks: loading from retained sources, inspection,
//! sample replacement, exact export and SPU voice audition.
use super::*;
use crate::psx_adpcm;
use crate::vab::{PITCH_READING, Packing, SCOPE, Vab, VabError};
use std::collections::BTreeMap;

pub(super) const VAB_BANKS: &str = "vab_bank";

/// A byte range of a source retained with load_instrument_source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VabPart {
    /// Saved load_instrument_source ID.
    pub source: String,
    /// Byte offset of the part within the source; default 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset: usize,
    /// Bytes in the part; omit for the rest of the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
}
fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadVabBankInput {
    pub job_id: String,
    /// The VH (header, program and tone tables, VAG size table). With
    /// `body` omitted, a combined VAB: the body follows the header.
    pub header: VabPart,
    /// The VB (SPU ADPCM regions); omit for a combined VAB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<VabPart>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VabView {
    /// Saved VAB bank IDs.
    Banks,
    /// Header fields with offsets, layout and consistency checks.
    #[default]
    Summary,
    /// All 128 program entries, every field with its offset.
    Programs,
    /// Every tone record of every tone block, used or not.
    Tones,
    /// VAG regions: offsets, decoded stream, ending, loop and references.
    Samples,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectVabBankInput {
    #[serde(default)]
    pub bank: Option<String>,
    #[serde(default)]
    pub view: VabView,
    /// tones view: only this program's tone block.
    #[serde(default)]
    pub program: Option<usize>,
    /// Item offset of the page; use next_cursor.
    #[serde(default)]
    pub offset: usize,
    /// Items per page; default 32, at most 128. A larger limit returns 128
    /// items and the response's `limit` reports `requested` and `applied`.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportVabBankInput {
    pub job_id: String,
    pub bank: String,
    /// New folder relative to the workspace.
    pub destination: String,
    /// Also write each decodable VAG's first pass as `samples/vag-NNN.wav`
    /// at 44100 Hz (SPU pitch 0x1000); default true.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub decoded_samples: bool,
}
fn yes() -> bool {
    true
}
fn is_true(value: &bool) -> bool {
    *value
}

/// Whether region offsets may move.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VabLayout {
    /// Every region keeps its offset and size; a different size refuses.
    #[default]
    Fixed,
    /// The region may change size; later regions shift, the VAG size table
    /// and a matching total size are rewritten.
    Resize,
}
impl VabLayout {
    fn is_fixed(&self) -> bool {
        *self == Self::Fixed
    }
}

/// How a sample plan's stream is framed in the region.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VabFraming {
    /// Prepend a zero block when the replaced region starts with one, and
    /// append the replaced region's bytes after its loop-end block when
    /// the new stream ends with end_mute.
    #[default]
    MatchOriginal,
    /// The plan's stream exactly.
    None,
}
impl VabFraming {
    fn is_match_original(&self) -> bool {
        *self == Self::MatchOriginal
    }
}

/// New center note or fine tune of one used tone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VabToneUpdate {
    pub program: usize,
    pub tone: usize,
    /// 0..127.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_note: Option<u8>,
    /// 0..127.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fine_tune: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaceVabSampleInput {
    pub job_id: String,
    pub bank: String,
    /// VAG to replace, 1..=the bank's VAG count. Every tone referencing it
    /// plays the new sample.
    pub vag: usize,
    /// Sample plan with target psx_adpcm or vag. Exclusive with donor_bank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Saved VAB bank whose region `donor_vag` is copied byte for byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub donor_bank: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub donor_vag: Option<usize>,
    #[serde(default, skip_serializing_if = "VabLayout::is_fixed")]
    pub layout: VabLayout,
    /// Plan sources only.
    #[serde(default, skip_serializing_if = "VabFraming::is_match_original")]
    pub framing: VabFraming,
    /// Explicit tone tuning changes applied with the replacement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tone_updates: Vec<VabToneUpdate>,
    /// Change codes the request accepts. `silent_padding`: with layout fixed,
    /// fill a smaller replacement up to the slot size with zero blocks after
    /// its loop-end block, which the voice never plays. Accept it only when
    /// a fixed-layout refusal offers it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_changes: Vec<String>,
}

/// The one change code replace_vab_sample can apply on acceptance.
const SILENT_PADDING: &str = "silent_padding";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderVabToneInput {
    pub job_id: String,
    pub bank: String,
    pub program: usize,
    /// Tone slot within the program, below its tone count.
    pub tone: usize,
    /// MIDI key; the pitch register follows from the tone's center note and
    /// fine tune (see the report's pitch reading). Exclusive with pitch_register.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<u8>,
    /// Explicit SPU pitch register (0x1000 = 44100 Hz).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_register: Option<u16>,
    /// Output samples (44100 Hz) before key-off; default 44100.
    #[serde(default = "second", skip_serializing_if = "is_second")]
    pub key_on_samples: u32,
    /// Output samples after key-off; default 22050.
    #[serde(default = "half_second", skip_serializing_if = "is_half_second")]
    pub release_samples: u32,
    /// Optional new folder for audition.wav and audition.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}
fn second() -> u32 {
    44100
}
fn is_second(value: &u32) -> bool {
    *value == 44100
}
fn half_second() -> u32 {
    22050
}
fn is_half_second(value: &u32) -> bool {
    *value == 22050
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct VabRecord {
    pub(super) header: String,
    pub(super) body: String,
    packing: Packing,
    /// Where the body sits in a retained source, when it was loaded from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_source: Option<VabPart>,
    lineage: Value,
}

/// Sample plan fields read here, as stored by prepare_sample.
#[derive(Debug, Serialize, Deserialize)]
struct PlanRecord {
    files: BTreeMap<String, String>,
    report: Value,
}

fn refused(location: &str, message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::TargetRefused, location, message)
}

fn located(error: VabError, base: usize) -> ToolResponse {
    refused(
        &format!("/{}", error.part),
        format!("{error} (source byte {})", base + error.offset),
    )
}

fn suggested_tuning(rate: f64, root: f64) -> Value {
    let (center, fine, exact) = crate::vab::tone_tuning(rate, root);
    json!({"center_note": center, "fine_tune": fine, "exact_center_minus_fine": exact,
        "in_range": (0.0..=127.0).contains(&center),
        "apply_with": "tone_updates center_note and fine_tune",
        "reading": PITCH_READING})
}

impl Service {
    pub(super) fn part_bytes(
        &mut self,
        part: &VabPart,
        location: &str,
    ) -> Result<Vec<u8>, ToolResponse> {
        let bytes = self.sound_source_bytes(&part.source)?;
        let end = match part.length {
            Some(n) => part.offset.checked_add(n),
            None => Some(bytes.len()),
        };
        end.and_then(|end| bytes.get(part.offset..end))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                ToolResponse::refused(
                    Code::InvalidRange,
                    location,
                    format!(
                        "the part {}..{} lies outside the {}-byte source",
                        part.offset,
                        end.map_or("overflow".into(), |e| e.to_string()),
                        bytes.len()
                    ),
                )
            })
    }

    pub(super) fn vab_bank_data(&mut self, id: &str) -> Result<(Vab, VabRecord), ToolResponse> {
        let record: VabRecord = self.shared_document(VAB_BANKS, id)?.ok_or_else(|| {
            ToolResponse::refused(
                Code::UnknownReference,
                "/bank",
                "unknown VAB bank; list inspect_vab_bank view=banks",
            )
        })?;
        if sha256_hex(&serde_json::to_vec(&record).expect("serializes")) != id {
            return Err(ToolResponse::error("VAB bank identity mismatch"));
        }
        let store = self.store_ref()?;
        let mut parts = Vec::new();
        for hash in [&record.header, &record.body] {
            let bytes = std::fs::read(store.object_path(hash))
                .map_err(|e| ToolResponse::error(e.to_string()))?;
            if sha256_hex(&bytes) != *hash {
                return Err(ToolResponse::error("VAB object identity mismatch"));
            }
            parts.push(bytes);
        }
        let body = parts.pop().expect("two parts");
        let header = parts.pop().expect("two parts");
        let vab = match record.packing {
            Packing::Separate => Vab::parse(header, body),
            Packing::Combined => {
                let mut all = header;
                all.extend(body);
                Vab::parse_combined(&all)
            }
        }
        .map_err(|e| located(e, 0))?;
        Ok((vab, record))
    }

    fn save_vab_bank(
        &mut self,
        vab: &Vab,
        body_source: Option<VabPart>,
        lineage: Value,
        summary: &str,
        mut result: Value,
    ) -> Handled {
        let store = self.store_mut(true)?;
        let header = store.put_object(&vab.header)?;
        let body = store.put_object(&vab.body)?;
        let record = VabRecord {
            header,
            body,
            packing: vab.packing,
            body_source,
            lineage,
        };
        let id = store.put_object(&serde_json::to_vec(&record).expect("serializes"))?;
        result["bank"] = json!(id);
        result["header_sha256"] = json!(record.header);
        result["body_sha256"] = json!(record.body);
        result["lineage"] = record.lineage.clone();
        let response = ToolResponse::ok(summary, result);
        commit(response, move |w| {
            w.put_document("", VAB_BANKS, &id, &record)
        })
    }

    pub(super) fn load_vab_bank(&mut self, input: LoadVabBankInput) -> Handled {
        let header = self.part_bytes(&input.header, "/header")?;
        let (vab, body_source) = match &input.body {
            Some(part) => {
                let body = self.part_bytes(part, "/body")?;
                let vab = Vab::parse(header.clone(), body).map_err(|e| {
                    let base = if e.part == "body" {
                        part.offset
                    } else {
                        input.header.offset
                    };
                    located(e, base)
                })?;
                (vab, Some(part.clone()))
            }
            None => {
                let vab = Vab::parse_combined(&header).map_err(|e| {
                    let base = input.header.offset
                        + if e.part == "body" {
                            crate::vab::header_length(&header).map_or(0, |(n, _)| n)
                        } else {
                            0
                        };
                    located(e, base)
                })?;
                let body_part = VabPart {
                    source: input.header.source.clone(),
                    offset: input.header.offset + vab.header.len(),
                    length: Some(vab.body.len()),
                };
                (vab, Some(body_part))
            }
        };
        let lineage = json!({"kind": "loaded", "header": input.header, "body": input.body,
            "packing": vab.packing, "layout_authority": "pBAV header fields; counts and sizes checked against each other"});
        let summary = vab.summary();
        let result = json!({"packing": vab.packing, "header_bytes": vab.header.len(), "body_bytes": vab.body.len(),
            "programs_with_tones": vab.tone_programs, "vags": vab.vag_count,
            "checks": summary["checks"], "samples": summary["samples"], "version": summary["fields"]["version"],
            "scope": SCOPE});
        self.save_vab_bank(
            &vab,
            body_source,
            lineage,
            "stored an immutable VAB bank; the retained sources stay exact and separate",
            result,
        )
    }

    pub(super) fn inspect_vab_bank(&mut self, input: InspectVabBankInput) -> Handled {
        if input.view == VabView::Banks {
            let records: Vec<VabRecord> = if Store::exists(&self.workspace) {
                self.store_ref()?.reader().documents("", VAB_BANKS)?
            } else {
                vec![]
            };
            let rows: Vec<Value> = records
                .iter()
                .map(|r| {
                    json!({"bank": sha256_hex(&serde_json::to_vec(r).expect("serializes")),
                        "packing": r.packing, "header_sha256": r.header, "body_sha256": r.body,
                        "kind": r.lineage["kind"]})
                })
                .collect();
            return done(page("saved VAB banks", rows, input.offset, input.limit));
        }
        let id = input
            .bank
            .clone()
            .ok_or_else(|| refused("/bank", "select a bank; view=banks lists saved IDs"))?;
        let (vab, record) = self.vab_bank_data(&id)?;
        let items: Vec<Value> = match input.view {
            VabView::Banks => unreachable!("handled above"),
            VabView::Summary => {
                let mut summary = vab.summary();
                summary["bank"] = json!(id);
                summary["lineage"] = record.lineage;
                summary["scope"] = json!(SCOPE);
                return done(ToolResponse::ok(
                    "VAB header, layout and checks; page programs, tones and samples for details",
                    summary,
                ));
            }
            VabView::Programs => (0..crate::vab::PROGRAMS).map(|p| vab.program(p)).collect(),
            VabView::Tones => {
                let blocks: Vec<usize> =
                    match input.program {
                        Some(p) => {
                            vec![vab.tone_programs.iter().position(|&q| q == p).ok_or_else(
                                || {
                                    refused(
                                        "/program",
                                        format!(
                                            "program {p} has no tones; programs with tones: {:?}",
                                            vab.tone_programs
                                        ),
                                    )
                                },
                            )?]
                        }
                        None => (0..vab.tone_programs.len()).collect(),
                    };
                blocks.into_iter().flat_map(|b| vab.tone_block(b)).collect()
            }
            VabView::Samples => {
                let start = input.offset.min(vab.vag_count);
                let (applied, limit) = page_limit(input.limit);
                let end = start.saturating_add(applied).min(vab.vag_count);
                let mut rows = Vec::new();
                for v in start + 1..=end {
                    let (mut row, stream) = vab.sample(v).map_err(|e| refused("/bank", e))?;
                    if let (Some(stream), Some(part)) = (stream, &record.body_source) {
                        for key in ["end", "loop_start"] {
                            if let Some(body) = row["offsets"][key]["body_offset"].as_u64() {
                                row["offsets"][key]["source_offset"] =
                                    json!(part.offset + body as usize);
                            }
                        }
                        row["prepare_sample"] = json!({"source": part.source,
                            "offset": part.offset + vab.regions[v].start,
                            "length": (stream.end_block + 1) * psx_adpcm::BLOCK_BYTES,
                            "encoding": psx_adpcm::RAW, "sample_rate": psx_adpcm::SPU_RATE,
                            "note": "the stream through its loop-end block; a VAB stores no sample rate, 44100 Hz is the rate at SPU pitch 0x1000"});
                    }
                    rows.push(row);
                }
                return done(ToolResponse::ok(
                    "VAG regions with offsets, decoded stream reports and references",
                    json!({"bank": id, "items": rows, "total": vab.vag_count, "limit": limit,
                        "next_cursor": (end < vab.vag_count).then_some(end)}),
                ));
            }
        };
        let mut response = page(
            "VAB records with every field's offset",
            items,
            input.offset,
            input.limit,
        );
        response.result["bank"] = json!(id);
        done(response)
    }

    pub(super) fn export_vab_bank(&mut self, input: ExportVabBankInput) -> Handled {
        let (vab, record) = self.vab_bank_data(&input.bank)?;
        let mut files: Vec<(String, Vec<u8>)> = match vab.packing {
            Packing::Separate => vec![
                ("bank.vh".into(), vab.header.clone()),
                ("bank.vb".into(), vab.body.clone()),
            ],
            Packing::Combined => vec![("bank.vab".into(), vab.combined_bytes())],
        };
        let mut samples = Vec::new();
        for v in 1..=vab.vag_count {
            let (mut row, stream) = vab.sample(v).map_err(|e| refused("/bank", e))?;
            if let (true, Some(stream)) = (input.decoded_samples, stream) {
                let pcm = stream.first_pass();
                let name = format!("samples/vag-{v:03}.wav");
                let wav = crate::sample_codec::mono_wav(&pcm, psx_adpcm::SPU_RATE);
                row["decoded"] = json!({"file": name, "samples": pcm.len(), "sample_rate": psx_adpcm::SPU_RATE,
                    "sha256": sha256_hex(&wav), "pass": "first pass through the loop-end block; loops are described by stream, not baked"});
                files.push((name, wav));
            }
            samples.push(row);
        }
        let tones: Vec<Value> = (0..vab.tone_programs.len())
            .flat_map(|b| vab.tone_block(b))
            .collect();
        let programs: Vec<Value> = (0..crate::vab::PROGRAMS).map(|p| vab.program(p)).collect();
        let report = json!({"bank": input.bank, "record": record, "summary": vab.summary(),
            "programs": programs, "tones": tones, "samples": samples, "scope": SCOPE});
        files.push((
            "bank.json".into(),
            serde_json::to_vec_pretty(&report).expect("serializes"),
        ));
        let mut response = self.export_sound_files(&input.job_id, &input.destination, files)?;
        response.result["bank"] = json!(input.bank);
        response.result["packing"] = json!(vab.packing);
        done(response)
    }

    pub(super) fn replace_vab_sample(&mut self, input: ReplaceVabSampleInput) -> Handled {
        let (vab, _) = self.vab_bank_data(&input.bank)?;
        let old = vab
            .region_bytes(input.vag)
            .map_err(|e| refused("/vag", e))?
            .to_vec();
        let (mut region, source, plan_stream, tuning) = match (
            &input.plan,
            &input.donor_bank,
            input.donor_vag,
        ) {
            (Some(plan), None, None) => {
                let (stream, tuning) = self.plan_stream(plan)?;
                let mut region = Vec::new();
                let mut framing = vec![];
                if input.framing == VabFraming::MatchOriginal {
                    let old_zero = old.len() >= 16 && old[..16].iter().all(|&b| b == 0);
                    let new_zero = stream[..16].iter().all(|&b| b == 0);
                    if old_zero && !new_zero {
                        region.extend([0u8; 16]);
                        framing.push(json!({"part": "leading_zero_block", "bytes": 16, "reason": "the replaced region starts with a zero block"}));
                    }
                }
                region.extend(&stream);
                if input.framing == VabFraming::MatchOriginal {
                    let (new_stream, _) =
                        crate::vab::stream_of(&stream, 0).map_err(|e| refused("/plan", e))?;
                    if let (Ok((old_stream, after)), psx_adpcm::Ending::EndMute) =
                        (crate::vab::stream_of(&old, 0), new_stream.ending)
                        && after > 0
                    {
                        let at = (old_stream.end_block + 1) * psx_adpcm::BLOCK_BYTES;
                        region.extend(&old[at..]);
                        framing.push(json!({"part": "after_end", "bytes": after, "reason": "the replaced region carries bytes after its end_mute block; copied unchanged"}));
                    }
                }
                let source = json!({"kind": "sample_plan", "plan": plan, "stream_bytes": stream.len(),
                    "stream_sha256": sha256_hex(&stream), "framing": input.framing, "framing_parts": framing});
                (region, source, Some(stream), tuning)
            }
            (None, Some(donor), Some(donor_vag)) => {
                let (donor_vab, _) = self.vab_bank_data(donor)?;
                let bytes = donor_vab
                    .region_bytes(donor_vag)
                    .map_err(|e| refused("/donor_vag", e))?
                    .to_vec();
                if !input.framing.is_match_original() {
                    return Err(refused(
                        "/framing",
                        "framing applies to sample plans; a donor region is copied byte for byte",
                    ));
                }
                let source = json!({"kind": "vab_region", "donor_bank": donor, "donor_vag": donor_vag,
                    "donor_body_offset": donor_vab.regions[donor_vag].start, "bytes": bytes.len(), "sha256": sha256_hex(&bytes)});
                (bytes, source, None, Value::Null)
            }
            _ => {
                return Err(refused(
                    "/plan",
                    "select a sample plan, or donor_bank with donor_vag, but not both",
                ));
            }
        };
        let unpadded = region.len();
        let mut changes = fit_region(&vab, input.vag, &mut region, &input)?;
        let (mut next, transfer) = vab
            .with_region(input.vag, &region, input.layout.is_fixed())
            .map_err(|e| refused("/vag", e))?;
        region_changes(
            &mut changes,
            &vab,
            input.vag,
            &old,
            &region,
            unpadded,
            &source,
        )?;
        let mut tone_changes = Vec::new();
        if !input.tone_updates.is_empty() {
            let mut edits = Vec::new();
            for (i, update) in input.tone_updates.iter().enumerate() {
                let at = next
                    .tone_offset(update.program, update.tone)
                    .ok_or_else(|| {
                        refused(
                            &format!("/tone_updates/{i}"),
                            format!(
                                "program {} has no used tone {}",
                                update.program, update.tone
                            ),
                        )
                    })?;
                for (delta, value, name) in [
                    (4, update.center_note, "center_note"),
                    (5, update.fine_tune, "fine_tune"),
                ] {
                    let Some(value) = value else { continue };
                    if value > 127 {
                        return Err(refused(
                            &format!("/tone_updates/{i}/{name}"),
                            format!("{name} is 0..127"),
                        ));
                    }
                    edits.push((at + delta, value));
                    tone_changes.push(
                        json!({"program": update.program, "tone": update.tone, "field": name,
                        "offset": at + delta, "old": next.header[at + delta], "new": value}),
                    );
                }
            }
            next = next
                .with_header_bytes(&edits)
                .map_err(|e| refused("/tone_updates", e))?;
        }
        let readback = replacement_readback(
            &vab,
            &next,
            input.vag,
            &region,
            plan_stream.as_deref(),
            &transfer,
            &tone_changes,
        )?;
        let references = next.references(input.vag);
        let lineage = json!({"kind": "vab_sample_replace", "base_bank": input.bank, "vag": input.vag,
            "layout": input.layout, "source": source, "transfer": transfer, "tone_changes": tone_changes,
            "changes": changes});
        let result = json!({"vag": input.vag, "layout": input.layout, "source": source, "transfer": transfer,
            "changes": changes, "tone_changes": tone_changes, "readback": readback, "tones_playing_it": references,
            "suggested_tone_tuning": tuning,
            "scope": "every tone referencing this VAG plays the new region; driver behaviour, other banks and SPU RAM placement are unchanged by this tool"});
        self.save_vab_bank(
            &next,
            None,
            lineage,
            "stored a new VAB bank with the replaced region; inspect transfer and readback",
            result,
        )
    }

    /// The SPU ADPCM stream of a sample plan and its suggested tone tuning.
    fn plan_stream(&mut self, id: &str) -> Result<(Vec<u8>, Value), ToolResponse> {
        let record: PlanRecord = self
            .shared_document("sample_plan", id)?
            .ok_or_else(|| refused("/plan", "unknown sample plan"))?;
        if sha256_hex(&serde_json::to_vec(&record).expect("serializes")) != id {
            return Err(ToolResponse::error("sample plan identity mismatch"));
        }
        let conversion = &record.report["conversion"];
        let target = conversion["target"].as_str().unwrap_or_default();
        if !psx_adpcm::ENCODINGS.contains(&target) {
            return Err(refused(
                "/plan",
                "a VAB region holds SPU ADPCM; prepare a plan with target psx_adpcm or vag",
            ));
        }
        let hash = record
            .files
            .get("sample.bin")
            .ok_or_else(|| ToolResponse::error("plan has no sample.bin"))?;
        let store = self.store_ref()?;
        let bytes = std::fs::read(store.object_path(hash))
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if sha256_hex(&bytes) != *hash {
            return Err(ToolResponse::error("sample object identity mismatch"));
        }
        let stream = if target == psx_adpcm::VAG {
            let vag = psx_adpcm::Vag::parse(&bytes).map_err(|e| refused("/plan", e))?;
            bytes[psx_adpcm::VAG_HEADER_BYTES..psx_adpcm::VAG_HEADER_BYTES + vag.data_size].to_vec()
        } else {
            bytes
        };
        let rate = conversion
            .get("target_sample_rate")
            .filter(|v| !v.is_null())
            .unwrap_or(&conversion["sample_rate"])
            .as_f64();
        let root = conversion.pointer("/tuning/root").filter(|r| !r.is_null());
        let tuning = match (rate, root) {
            (Some(rate), Some(root)) => suggested_tuning(
                rate,
                root["note"].as_f64().unwrap_or(60.0)
                    + root["cents"].as_f64().unwrap_or(0.0) / 100.0,
            ),
            (Some(rate), None) => {
                json!({"sample_rate": rate, "note": "the plan has no root; prepare it with root to get a center note and fine tune", "reading": PITCH_READING})
            }
            _ => Value::Null,
        };
        Ok((stream, tuning))
    }

    pub(super) fn render_vab_tone(&mut self, input: RenderVabToneInput) -> Handled {
        let (vab, _) = self.vab_bank_data(&input.bank)?;
        let at = vab.tone_offset(input.program, input.tone).ok_or_else(|| {
            refused(
                "/tone",
                format!(
                    "program {} has no used tone {}; programs with tones: {:?}",
                    input.program, input.tone, vab.tone_programs
                ),
            )
        })?;
        let t = &vab.header[at..at + crate::vab::TONE_BYTES];
        let (center, fine, low, high) = (t[4], t[5], t[6], t[7]);
        let adsr1 = u16::from_le_bytes([t[0x10], t[0x11]]);
        let adsr2 = u16::from_le_bytes([t[0x12], t[0x13]]);
        let vag = u16::from_le_bytes([t[0x16], t[0x17]]) as usize;
        let region = vab
            .region_bytes(vag)
            .map_err(|e| refused("/tone", format!("the tone's VAG: {e}")))?;
        crate::vab::stream_of(region, vab.regions[vag].start)
            .map_err(|e| refused("/tone", format!("VAG {vag} does not decode: {e}")))?;
        let (pitch, pitch_detail) = match (input.note, input.pitch_register) {
            (Some(note), None) => {
                if note > 127 {
                    return Err(refused("/note", "note is 0..127"));
                }
                let semitones = note as f64 - center as f64 + fine as f64 / 128.0;
                let exact = crate::vab::note_pitch(note, center, fine);
                let value = exact.round().clamp(1.0, u16::MAX as f64) as u16;
                (
                    value,
                    json!({"source": "note", "note": note, "center_note": center, "fine_tune": fine,
                    "semitones_from_center": semitones, "exact": exact, "register": value,
                    "in_tone_range": (low..=high).contains(&note), "tone_range": [low, high], "reading": PITCH_READING}),
                )
            }
            (None, Some(register)) => (
                register,
                json!({"source": "pitch_register", "register": register}),
            ),
            _ => return Err(refused("/note", "give note or pitch_register, not both")),
        };
        let setup = crate::spu_voice::VoiceSetup {
            region,
            pitch,
            adsr1,
            adsr2,
            key_on_samples: input.key_on_samples as usize,
            release_samples: input.release_samples as usize,
        };
        let (pcm, voice) =
            crate::spu_voice::render(&setup).map_err(|e| refused("/key_on_samples", e))?;
        let wav = crate::sample_codec::mono_wav(&pcm, crate::spu_voice::OUTPUT_RATE);
        let store = self.store_mut(true)?;
        let object = store.put_object(&wav)?;
        let path = store.object_path(&object);
        let measurable = path
            .strip_prefix(&self.workspace)
            .unwrap_or(&path)
            .display()
            .to_string();
        let mut response = ToolResponse::ok(
            format!(
                "SPU voice audition: {} samples at 44100 Hz, pitch 0x{pitch:04X}; measure it with {{\"audio\": {{\"path\": \"{measurable}\"}}}}",
                pcm.len()
            ),
            json!({"tone": {"program": input.program, "tone": input.tone, "offset": at, "vag": vag,
                    "volume": t[2], "pan": t[3], "program_volume": vab.header[crate::vab::PROGRAM_TABLE + input.program * 16 + 1],
                    "master_volume": vab.header[0x18]},
                "pitch": pitch_detail, "voice": voice,
                "wav": {"sha256": object, "channels": 1, "bits": 16, "sample_rate": crate::spu_voice::OUTPUT_RATE, "samples": pcm.len()},
                "limits": "one SPU voice from public documentation at unity voice volume; tone, program and master volume and pan are reported, not applied; vibrato, portamento, pitch bend, reverb, the sound library's note arithmetic and SPU hardware output are not reproduced"}),
        );
        response.artifacts.push(Artifact {
            kind: "wav".into(),
            path: path.display().to_string(),
            sha256: object,
        });
        if let Some(destination) = &input.destination {
            let report = serde_json::to_vec_pretty(&response.result).expect("serializes");
            let exported = self.export_sound_files(
                &input.job_id,
                destination,
                vec![
                    ("audition.wav".into(), wav),
                    ("audition.json".into(), report),
                ],
            )?;
            response.artifacts.extend(exported.artifacts);
        }
        response.result["wav"]["path"] = json!(measurable);
        response.result["measure_with"] = json!({"audio": {"path": measurable, "expected_sha256": response.result["wav"]["sha256"]}});
        done(response)
    }
}

/// Adds to `changes` every difference between the replaced and the new
/// region: the count of differing bytes and each changed flag byte with its
/// origin (the plan's own encoding, framing, padding or the donor region)
/// and whether playback can notice it. Flags are never carried over from
/// the replaced region: the new region's blocks are its source's encoding.
fn region_changes(
    changes: &mut Value,
    base: &Vab,
    vag: usize,
    old: &[u8],
    region: &[u8],
    unpadded: usize,
    source: &Value,
) -> Result<(), ToolResponse> {
    let start = base.regions[vag].start;
    let (bytes, flags) = crate::vab::region_differences(old, region, start);
    let parts = source["framing_parts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let has = |name: &str| {
        parts
            .iter()
            .find(|p| p["part"] == name)
            .and_then(|p| p["bytes"].as_u64())
            .unwrap_or(0) as usize
    };
    let lead = has("leading_zero_block");
    let stream_end = lead + source["stream_bytes"].as_u64().unwrap_or(0) as usize;
    let origin = |offset: usize| -> &'static str {
        if offset >= unpadded {
            "silent_padding"
        } else if source["kind"] == "vab_region" {
            "donor region"
        } else if offset < lead {
            "framing: leading_zero_block"
        } else if offset < stream_end {
            "the plan's own encoding"
        } else {
            "framing: after_end"
        }
    };
    let streams = (
        crate::vab::stream_of(old, start),
        crate::vab::stream_of(region, start),
    );
    let (Ok((old_stream, _)), Ok((new_stream, _))) = streams else {
        return Err(ToolResponse::error(
            "VAB replacement: a region does not decode",
        ));
    };
    let ending = |s: &psx_adpcm::Stream| s.report()["ending"].clone();
    let both_release = old_stream.ending == psx_adpcm::Ending::EndMute
        && new_stream.ending == psx_adpcm::Ending::EndMute;
    let rows: Vec<Value> = flags
        .iter()
        .map(|&(block, offset, was, now)| {
            let new_unplayed = block > new_stream.end_block;
            let effect = if new_unplayed && block > old_stream.end_block {
                "none: after the loop-end block in both regions, never played"
            } else if new_unplayed {
                "none for this flag: the new region ends before this block, so it is never played; the old region played it, and the shorter stream is reported under region_bytes"
            } else if (was ^ now) & !psx_adpcm::LOOP_START == 0 && both_release {
                "none under the documented SPU: only loop_start differs and both streams end with end_mute, which releases the voice instead of jumping to the repeat address"
            } else {
                "can change playback: compare the endings and loop starts in this entry"
            };
            json!({"block": block, "body_offset": offset, "old": was, "new": now,
                "old_bits": crate::vab::flag_names(was), "new_bits": crate::vab::flag_names(now),
                "origin": origin(block * psx_adpcm::BLOCK_BYTES), "effect": effect})
        })
        .collect();
    changes["region_bytes"] = json!({"bytes_differing": bytes, "old_bytes": old.len(), "new_bytes": region.len(),
        "old_ending": ending(&old_stream), "new_ending": ending(&new_stream),
        "old_loop_start_block": old_stream.loop_start_block(), "new_loop_start_block": new_stream.loop_start_block()});
    changes["block_flags"] = json!({"changed": rows.len(), "blocks": rows,
        "policy": "flags come from the new region's source and are not carried over from the replaced region"});
    Ok(())
}

/// Makes the replacement fit a fixed slot, or refuses naming the routes
/// that exist: accepting `silent_padding` for a smaller region, layout
/// resize, or a shorter or longer sample.
fn fit_region(
    vab: &Vab,
    vag: usize,
    region: &mut Vec<u8>,
    input: &ReplaceVabSampleInput,
) -> Result<Value, ToolResponse> {
    let mut accepted = input.accepted_changes.clone();
    accepted.sort();
    accepted.dedup();
    if let Some(unknown) = accepted.iter().find(|c| *c != SILENT_PADDING) {
        return Err(refused(
            "/accepted_changes",
            format!(
                "unknown change `{unknown}`; replace_vab_sample can apply only {SILENT_PADDING}"
            ),
        ));
    }
    let padding_accepted = !accepted.is_empty();
    let slot = vab.regions[vag];
    let fixed = input.layout.is_fixed();
    if fixed && region.len() < slot.bytes {
        let missing = slot.bytes - region.len();
        if !missing.is_multiple_of(psx_adpcm::BLOCK_BYTES) {
            return Err(refused(
                "/vag",
                format!(
                    "VAG {vag} occupies {} bytes, which is not a whole number of 16-byte blocks more than the replacement's {}; silent padding cannot fill it. Choose layout resize",
                    slot.bytes,
                    region.len()
                ),
            ));
        }
        if !padding_accepted {
            return Err(refused(
                "/vag",
                format!(
                    "VAG {vag} occupies {} bytes at body byte {}; the replacement holds {} ({missing} bytes short). layout fixed keeps every region offset, so the size must match. Add \"{SILENT_PADDING}\" to accepted_changes to fill the remaining {} blocks with zero blocks after the replacement's loop-end block (never played), or choose layout resize to shift later regions",
                    slot.bytes,
                    slot.start,
                    region.len(),
                    missing / psx_adpcm::BLOCK_BYTES
                ),
            ));
        }
        let at = region.len();
        region.resize(slot.bytes, 0);
        return Ok(
            json!({SILENT_PADDING: {"bytes": missing, "blocks": missing / psx_adpcm::BLOCK_BYTES,
            "region_offset": at, "body_offset": slot.start + at,
            "meaning": "zero blocks after the replacement's loop-end block; the voice repeats or is released before them, so they are never played"}}),
        );
    }
    if fixed && region.len() > slot.bytes {
        return Err(refused(
            "/vag",
            format!(
                "VAG {vag} occupies {} bytes at body byte {}; the replacement holds {} ({} bytes too long). layout fixed keeps every region offset, so the size must match. Choose layout resize to shift later regions, or prepare a shorter sample (a shorter source slice or WAV, or a lower target_sample_rate)",
                slot.bytes,
                slot.start,
                region.len(),
                region.len() - slot.bytes
            ),
        ));
    }
    if padding_accepted {
        return Err(refused(
            "/accepted_changes",
            format!(
                "{SILENT_PADDING} applies only when layout fixed meets a smaller replacement; this request needs no padding, so remove it"
            ),
        ));
    }
    Ok(json!({}))
}

/// Checks the derived bank against its base: only declared header bytes
/// changed, every other region is byte-identical, and the new region
/// decodes to the plan's stream.
fn replacement_readback(
    base: &Vab,
    next: &Vab,
    vag: usize,
    region: &[u8],
    plan_stream: Option<&[u8]>,
    transfer: &Value,
    tone_changes: &[Value],
) -> Result<Value, ToolResponse> {
    let fail = |what: &str| Err(ToolResponse::error(format!("VAB readback failed: {what}")));
    let mut declared: Vec<u64> = transfer["changed_header_bytes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["offset"].as_u64())
        .collect();
    declared.extend(tone_changes.iter().filter_map(|c| c["offset"].as_u64()));
    let undeclared = (0..base.header.len())
        .filter(|&i| base.header[i] != next.header[i] && !declared.contains(&(i as u64)))
        .count();
    if undeclared > 0 || base.header.len() != next.header.len() {
        return fail("undeclared header bytes changed");
    }
    let (old, new) = (base.regions[vag], next.regions[vag]);
    if next.body[..old.start] != base.body[..old.start]
        || next.body[new.start + new.bytes..] != base.body[old.start + old.bytes..]
        || next.body[new.start..new.start + new.bytes] != *region
    {
        return fail("body bytes outside the replaced region differ");
    }
    let unchanged = (1..=base.vag_count)
        .filter(|&v| v != vag)
        .all(|v| base.region_bytes(v).ok() == next.region_bytes(v).ok());
    if !unchanged {
        return fail("another region differs");
    }
    let (stream, _) = next
        .sample_stream(vag)
        .map_err(|e| ToolResponse::error(format!("VAB readback: {e}")))?;
    let decoded = stream.first_pass();
    let mut report = json!({"header_bytes_compared": base.header.len(), "undeclared_header_changes": 0,
        "other_regions_identical": base.vag_count - 1, "body_bytes": next.body.len(),
        "decoded_samples": decoded.len(), "ending": stream.report()["ending"], "loop_start_block": stream.loop_start_block()});
    if let Some(plan) = plan_stream {
        let (expected, _) = crate::vab::stream_of(plan, 0).map_err(|e| refused("/plan", e))?;
        let expected = expected.first_pass();
        let lead = decoded.len().checked_sub(expected.len());
        let matches = lead.is_some_and(|lead| {
            decoded[..lead].iter().all(|&v| v == 0) && decoded[lead..] == expected[..]
        });
        if !matches {
            return fail("the region does not decode to the plan's stream");
        }
        report["decoded_matches_plan"] = json!(true);
        report["leading_silent_samples"] = json!(lead);
    }
    Ok(report)
}

/// Largest inspect_vab_bank page.
const MAX_VAB_PAGE: u32 = 128;

/// The page size to use and how the request's limit was applied: a limit
/// outside 1..=128 is applied as the nearest bound and reported, not refused,
/// like the other paging tools.
fn page_limit(limit: Option<u32>) -> (usize, Value) {
    let applied = limit.unwrap_or(32).clamp(1, MAX_VAB_PAGE);
    let report = match limit {
        Some(requested) if requested != applied => {
            json!({"requested": requested, "applied": applied,
            "clamped": true, "note": format!("pages hold 1..={MAX_VAB_PAGE} items; continue with next_cursor")})
        }
        _ => json!({"applied": applied, "clamped": false}),
    };
    (applied as usize, report)
}

fn page(summary: &str, items: Vec<Value>, offset: usize, limit: Option<u32>) -> ToolResponse {
    let start = offset.min(items.len());
    let (applied, limit) = page_limit(limit);
    let end = start.saturating_add(applied).min(items.len());
    ToolResponse::ok(
        summary,
        json!({"items": &items[start..end], "total": items.len(), "limit": limit, "next_cursor": (end < items.len()).then_some(end)}),
    )
}
