//! PlayStation SEQ/SEP sequences: loading from retained sources, event
//! inspection, binding checks against a VAB bank, exact and SMF export, and
//! SPU audition against a bank.
use super::*;
use crate::psx_seq::{Container, SCOPE, SeqError, SeqFile, SmfOptions};
use crate::spu_sequence::Overflow;

pub(super) const PSX_SEQUENCES: &str = "psx_sequence";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadPsxSequenceInput {
    pub job_id: String,
    /// The SEQ or SEP bytes: a range of a source retained with
    /// load_instrument_source (offset and length for a member of a container).
    pub sequence: VabPart,
    /// `detect` (default) reads version 1 as SEQ and 16-bit version 0 with
    /// first sequence ID 0 as SEP.
    #[serde(default, skip_serializing_if = "is_detect")]
    pub container: Container,
}
fn is_detect(value: &Container) -> bool {
    *value == Container::Detect
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PsxSequenceView {
    /// Saved sequence IDs.
    Sequences,
    /// Header fields with offsets, event kinds, loops, programs and checks.
    #[default]
    Summary,
    /// Every event of one sequence with its byte offset and meaning.
    Events,
    /// Programs and keys the sequence plays, against the tones of `bank`.
    Binding,
    /// Every VAB header inside the retained source `bank_source`, ranked by
    /// how many of the sequence's notes its tones cover (header only).
    BindingCandidates,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectPsxSequenceInput {
    #[serde(default)]
    pub sequence: Option<String>,
    #[serde(default)]
    pub view: PsxSequenceView,
    /// Sequence within a SEP (0 for a SEQ).
    #[serde(default)]
    pub index: usize,
    /// binding view: saved VAB bank.
    #[serde(default)]
    pub bank: Option<String>,
    /// binding_candidates view: a load_instrument_source ID (such as a
    /// container) to scan for VAB headers.
    #[serde(default)]
    pub bank_source: Option<String>,
    /// Item offset of the page; use next_cursor.
    #[serde(default)]
    pub offset: usize,
    /// Items per page; default 64, at most 500.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportPsxSequenceInput {
    pub job_id: String,
    pub sequence: String,
    /// New folder relative to the workspace.
    pub destination: String,
    /// Add `loopStart`/`loopEnd` marker metas at the NRPN loop events of
    /// the MIDI files; default true.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub loop_markers: bool,
}
fn yes() -> bool {
    true
}
fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderPsxSequenceInput {
    pub job_id: String,
    pub sequence: String,
    /// Sequence within a SEP (0 for a SEQ).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub index: usize,
    /// Saved VAB bank the sequence plays; the SEQ does not name it.
    pub bank: String,
    /// Jumps back at the NRPN loop end; default 1, at most 64. A finite
    /// source count limits it.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub loop_repeats: u32,
    /// Output bound in seconds, tail included; default 180, at most 600.
    #[serde(default = "minutes", skip_serializing_if = "is_minutes")]
    pub max_seconds: u32,
    /// Release time after end of track, at most 10; default 2.
    #[serde(default = "two", skip_serializing_if = "is_two")]
    pub tail_seconds: f64,
    /// When all 24 SPU voices are occupied; default refuse.
    #[serde(default, skip_serializing_if = "is_refuse")]
    pub voice_overflow: Overflow,
    /// Optional gain in dB (-120.0..12.0, one decimal) applied to every float
    /// sample after rendering, for headroom; omitted, the samples are
    /// unscaled. Reported with the peak before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<crate::mix::Tenths>,
    /// Optional new folder for render.wav and render.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}
fn is_zero(value: &usize) -> bool {
    *value == 0
}
fn one() -> u32 {
    1
}
fn is_one(value: &u32) -> bool {
    *value == 1
}
fn minutes() -> u32 {
    180
}
fn is_minutes(value: &u32) -> bool {
    *value == 180
}
fn two() -> f64 {
    2.0
}
fn is_two(value: &f64) -> bool {
    *value == 2.0
}
fn is_refuse(value: &Overflow) -> bool {
    *value == Overflow::Refuse
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PsxSequenceRecord {
    bytes: String,
    container: Container,
    range: VabPart,
    lineage: Value,
}

fn refused(location: &str, message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::TargetRefused, location, message)
}

fn located(error: &SeqError, base: usize) -> ToolResponse {
    let mut response = refused(
        "/sequence",
        format!("{error} (source byte {})", base + error.offset),
    );
    response.result = json!({"offset": error.offset, "source_offset": base + error.offset,
        "sequence_index": error.sequence, "retained": "the source is unchanged and stays exportable with export_instrument_source"});
    response
}

impl Service {
    fn psx_sequence_data(
        &mut self,
        id: &str,
    ) -> Result<(SeqFile, Vec<u8>, PsxSequenceRecord), ToolResponse> {
        let record: PsxSequenceRecord =
            self.shared_document(PSX_SEQUENCES, id)?.ok_or_else(|| {
                ToolResponse::refused(
                    Code::UnknownReference,
                    "/sequence",
                    "unknown PSX sequence; list inspect_psx_sequence view=sequences",
                )
            })?;
        if sha256_hex(&serde_json::to_vec(&record).expect("serializes")) != id {
            return Err(ToolResponse::error("PSX sequence identity mismatch"));
        }
        let bytes = std::fs::read(self.store_ref()?.object_path(&record.bytes))
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if sha256_hex(&bytes) != record.bytes {
            return Err(ToolResponse::error("PSX sequence object identity mismatch"));
        }
        let file = crate::psx_seq::parse(&bytes, record.container)
            .map_err(|e| located(&e, record.range.offset))?;
        Ok((file, bytes, record))
    }

    pub(super) fn load_psx_sequence(&mut self, input: LoadPsxSequenceInput) -> Handled {
        let bytes = self.part_bytes(&input.sequence, "/sequence")?;
        let file = crate::psx_seq::parse(&bytes, input.container)
            .map_err(|e| located(&e, input.sequence.offset))?;
        let store = self.store_mut(true)?;
        let hash = store.put_object(&bytes)?;
        let record = PsxSequenceRecord {
            bytes: hash.clone(),
            container: file.container,
            range: input.sequence.clone(),
            lineage: json!({"kind": "loaded", "range": input.sequence, "container_requested": input.container,
                "layout_authority": "pQES header; SEP sequences by their data sizes"}),
        };
        let id = store.put_object(&serde_json::to_vec(&record).expect("serializes"))?;
        let sequences: Vec<Value> = file
            .sequences
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let summary = s.summary(i);
                json!({"index": i, "start": summary["start"], "end": summary["end"], "events": summary["events"], "notes": summary["notes"],
                    "seconds": summary["seconds"], "resolution": s.header.resolution, "loops": summary["loops"],
                    "programs_by_channel": summary["programs_by_channel"]})
            })
            .collect();
        let result = json!({"sequence": id, "container": file.container, "version": file.version,
            "bytes": bytes.len(), "sha256": hash, "sequences": sequences, "trailing_bytes": file.trailing,
            "scope": SCOPE,
            "next": "inspect_psx_sequence view=events for every event; view=binding with a VAB bank; export_psx_sequence for exact bytes and MIDI; render_psx_sequence to audition against a bank"});
        let response = ToolResponse::ok(
            "stored an immutable PSX sequence; the retained source stays exact and separate",
            result,
        );
        commit(response, move |w| {
            w.put_document("", PSX_SEQUENCES, &id, &record)
        })
    }

    pub(super) fn inspect_psx_sequence(&mut self, input: InspectPsxSequenceInput) -> Handled {
        if input.view == PsxSequenceView::Sequences {
            let records: Vec<PsxSequenceRecord> = if Store::exists(&self.workspace) {
                self.store_ref()?.reader().documents("", PSX_SEQUENCES)?
            } else {
                vec![]
            };
            let rows: Vec<Value> = records
                .iter()
                .map(|r| {
                    json!({"sequence": sha256_hex(&serde_json::to_vec(r).expect("serializes")),
                        "container": r.container, "sha256": r.bytes, "range": r.range})
                })
                .collect();
            return done(page("saved PSX sequences", rows, input.offset, input.limit));
        }
        let id = input.sequence.clone().ok_or_else(|| {
            refused(
                "/sequence",
                "select a sequence; view=sequences lists saved IDs",
            )
        })?;
        let (file, _, record) = self.psx_sequence_data(&id)?;
        let seq = file.sequences.get(input.index).ok_or_else(|| {
            refused(
                "/index",
                format!("the file holds {} sequence(s)", file.sequences.len()),
            )
        })?;
        match input.view {
            PsxSequenceView::Sequences => unreachable!("handled above"),
            PsxSequenceView::Summary => {
                let mut summary = file.summary();
                summary["sequence"] = json!(id);
                summary["range"] = json!(record.range);
                summary["lineage"] = record.lineage;
                done(ToolResponse::ok(
                    "SEQ/SEP header, event kinds, loops and checks; page events for every event",
                    summary,
                ))
            }
            PsxSequenceView::Events => {
                let start = input.offset.min(seq.events.len());
                let end = start
                    .saturating_add(input.limit.unwrap_or(64).clamp(1, 500) as usize)
                    .min(seq.events.len());
                let items: Vec<Value> = (start..end).map(|i| seq.event_json(i)).collect();
                done(ToolResponse::ok(
                    "events in stream order with byte offsets (within the sequence bytes), ticks and meanings",
                    json!({"sequence": id, "index": input.index, "items": items, "total": seq.events.len(),
                        "source_base": record.range.offset,
                        "next_cursor": (end < seq.events.len()).then_some(end)}),
                ))
            }
            PsxSequenceView::Binding => {
                let bank = input
                    .bank
                    .clone()
                    .ok_or_else(|| refused("/bank", "binding needs a saved VAB bank"))?;
                let (vab, _) = self.vab_bank_data(&bank)?;
                let mut response = ToolResponse::ok(
                    "programs and keys the sequence plays, matched against the bank's tones",
                    binding(seq, &vab.header, &vab.tone_programs),
                );
                response.result["sequence"] = json!(id);
                response.result["bank"] = json!(bank);
                done(response)
            }
            PsxSequenceView::BindingCandidates => {
                let source = input.bank_source.clone().ok_or_else(|| {
                    refused(
                        "/bank_source",
                        "binding_candidates needs a load_instrument_source ID to scan",
                    )
                })?;
                let bytes = self.sound_source_bytes(&source)?;
                let mut rows = Vec::new();
                let mut at = 0;
                while let Some(i) = bytes
                    .get(at..)
                    .and_then(|rest| rest.windows(4).position(|w| w == crate::vab::MAGIC))
                {
                    let offset = at + i;
                    at = offset + 4;
                    let Ok(facts) = crate::vab::header_facts(&bytes[offset..]) else {
                        continue;
                    };
                    let Ok((_, programs)) = crate::vab::header_length(&bytes[offset..]) else {
                        continue;
                    };
                    let header = &bytes[offset..offset + facts.header_bytes];
                    let b = binding(seq, header, &programs);
                    let used: Vec<usize> = b["programs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|r| r["program_has_tones"] == json!(true))
                        .filter_map(|r| r["program"].as_u64().map(|p| p as usize))
                        .collect();
                    let missing = b["programs"].as_array().map_or(0, |a| a.len() - used.len());
                    rows.push(json!({"offset": offset, "header_bytes": facts.header_bytes,
                        "notes_without_tone": b["notes_without_tone"], "programs_without_tones": missing,
                        "bank_programs_unused": programs.len() - used.len(),
                        "keys_with_layered_tones": b["keys_with_layered_tones"],
                        "header": {"source": source, "offset": offset, "length": facts.header_bytes},
                        "body_bytes_expected": facts.region_bytes}));
                }
                rows.sort_by_key(|r| {
                    (
                        r["notes_without_tone"].as_u64(),
                        r["programs_without_tones"].as_u64(),
                        r["bank_programs_unused"].as_u64(),
                        r["offset"].as_u64(),
                    )
                });
                let mut response = page(
                    "VAB headers in the source ranked by binding coverage of the sequence",
                    rows,
                    input.offset,
                    input.limit,
                );
                response.result["sequence"] = json!(id);
                response.result["bank_source"] = json!(source);
                response.result["ranking"] = json!(
                    "fewest notes without a tone, then fewest programs without tones, then fewest bank programs the sequence does not use, then offset; read from each header's program and tone tables only"
                );
                response.result["caution"] = json!(
                    "coverage narrows the choice but does not prove the pairing: banks whose tones span every key tie. Load the chosen bank with load_vab_bank (its body offset is not in the header), check view=binding and report the pairing as an assumption"
                );
                done(response)
            }
        }
    }

    pub(super) fn export_psx_sequence(&mut self, input: ExportPsxSequenceInput) -> Handled {
        let (file, bytes, record) = self.psx_sequence_data(&input.sequence)?;
        let name = match file.container {
            Container::Sep => "sequence.sep",
            _ => "sequence.seq",
        };
        let mut files = vec![(name.to_owned(), bytes)];
        let mut midi = Vec::new();
        for (i, s) in file.sequences.iter().enumerate() {
            match crate::psx_seq::to_smf(
                s,
                SmfOptions {
                    loop_markers: input.loop_markers,
                },
            ) {
                Ok((smf, report)) => {
                    let file_name = format!("sequence-{i:02}.mid");
                    midi.push(json!({"index": i, "file": file_name, "sha256": sha256_hex(&smf), "bytes": smf.len(), "report": report}));
                    files.push((file_name, smf));
                }
                Err(reason) => midi.push(json!({"index": i, "file": null, "not_exported": reason})),
            }
        }
        let report = json!({"sequence": input.sequence, "record": record, "summary": file.summary(), "midi": midi,
            "load_with": "load_midi on a sequence-NN.mid path; source-only render_preview plays it on the reference synthesizer, not the VAB"});
        files.push((
            "sequence.json".into(),
            serde_json::to_vec_pretty(&report).expect("serializes"),
        ));
        let mut response = self.export_sound_files(&input.job_id, &input.destination, files)?;
        response.summary = "exported the exact sequence bytes, MIDI files with event readback and the interpretation".into();
        response.result["sequence"] = json!(input.sequence);
        response.result["midi"] = json!(midi
            .iter()
            .map(|m| json!({"index": m["index"], "file": m["file"], "not_exported": m["not_exported"],
                "not_in_midi": m["report"]["not_in_midi"], "added": m["report"]["added"]}))
            .collect::<Vec<_>>());
        done(response)
    }

    pub(super) fn render_psx_sequence(&mut self, input: RenderPsxSequenceInput) -> Handled {
        let (file, _, record) = self.psx_sequence_data(&input.sequence)?;
        let seq = file.sequences.get(input.index).ok_or_else(|| {
            refused(
                "/index",
                format!("the file holds {} sequence(s)", file.sequences.len()),
            )
        })?;
        let (vab, _) = self.vab_bank_data(&input.bank)?;
        if let Some(gain) = input.gain_db
            && !(crate::mix::MIN_GAIN_DB..=crate::mix::MAX_GAIN_DB).contains(&gain.value())
        {
            return Err(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/gain_db",
                "gain_db must be -120.0–12.0",
            ));
        }
        let options = crate::spu_sequence::Options {
            loop_repeats: input.loop_repeats,
            max_seconds: input.max_seconds,
            tail_seconds: input.tail_seconds,
            overflow: input.voice_overflow,
        };
        let mut rendered = crate::spu_sequence::render(seq, &vab, &options).map_err(|e| match e.event {
            Some((event, offset)) => {
                let mut r = refused(
                    "/sequence",
                    format!(
                        "event {event} at byte {offset} (source byte {}): {}",
                        record.range.offset + offset,
                        e.message
                    ),
                );
                r.result = json!({"event": event, "offset": offset, "source_offset": record.range.offset + offset,
                    "index": input.index, "event_detail": seq.event_json(event)});
                r
            }
            None => refused("", e.message),
        })?;
        let peak_before_gain = input.gain_db.map(|gain| {
            let before = rendered
                .left
                .iter()
                .chain(&rendered.right)
                .fold(0.0f32, |peak, v| peak.max(v.abs()));
            let factor = 10f64.powf(gain.value() / 20.0) as f32;
            for v in rendered.left.iter_mut().chain(rendered.right.iter_mut()) {
                *v *= factor;
            }
            before
        });
        let format = crate::review::WavFormat::Float32;
        let levels = super::batch::render_levels(&rendered.left, &rendered.right, format);
        let warning = levels["samples_over_full_scale"]
            .as_u64()
            .filter(|&n| n > 0)
            .map(|over| {
                let peak = levels["sample_peak"].as_f64().unwrap_or(0.0) as f32;
                let suggested = super::source_review::gain_for_peak(peak, input.gain_db);
                json!({"code": "samples_over_full_scale", "location": "/levels/samples_over_full_scale",
                    "message": format!("{over} samples exceed full scale (peak {peak:.4}); the float file keeps them, but playback or integer conversion will clip them. No level was changed; render again with gain_db at or below {suggested:.1} (another job_id) for headroom"),
                    "suggested_gain_db": suggested})
            });
        let wav = crate::render::wav_bytes_in(
            crate::spu_voice::OUTPUT_RATE,
            &rendered.left,
            &rendered.right,
            crate::review::WavFormat::Float32,
        );
        let store = self.store_mut(true)?;
        let object = store.put_object(&wav)?;
        let path = store.object_path(&object);
        let measurable = path
            .strip_prefix(&self.workspace)
            .unwrap_or(&path)
            .display()
            .to_string();
        let mut summary = format!(
            "SPU sequence audition: {:.2} s stereo float at 44100 Hz, {} notes, peak {} voices; a reference audition from documented voices and declared readings, not console output",
            rendered.report["seconds"].as_f64().unwrap_or(0.0),
            rendered.report["notes"],
            rendered.report["peak_voices"]
        );
        if warning.is_some() {
            summary.push_str(&format!(
                "; {} samples exceed full scale (see warnings)",
                levels["samples_over_full_scale"]
            ));
        }
        let mut response = ToolResponse::ok(
            summary,
            json!({"sequence": input.sequence, "index": input.index, "bank": input.bank,
                "render": rendered.report,
                "wav": {"sha256": object, "channels": 2, "format": "float32", "sample_rate": crate::spu_voice::OUTPUT_RATE,
                    "frames": rendered.left.len(), "path": measurable},
                "levels": levels,
                "measure_with": {"audio": {"path": measurable, "expected_sha256": object}}}),
        );
        if let (Some(gain), Some(before)) = (input.gain_db, peak_before_gain) {
            response.result["gain"] = json!({"gain_db": gain, "sample_peak_before_gain": before,
                "applied": "to every float sample after rendering; the sequence, bank and voice model are unchanged"});
        }
        if let Some(warning) = warning {
            response.result["warnings"] = json!([warning]);
        }
        response.artifacts.push(Artifact {
            kind: "wav".into(),
            path: path.display().to_string(),
            sha256: response.result["wav"]["sha256"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        });
        if let Some(destination) = &input.destination {
            let report = serde_json::to_vec_pretty(&response.result).expect("serializes");
            let exported = self.export_sound_files(
                &input.job_id,
                destination,
                vec![("render.wav".into(), wav), ("render.json".into(), report)],
            )?;
            response.artifacts.extend(exported.artifacts);
        }
        done(response)
    }
}

/// Programs and keys a sequence plays, and the bank's tones for them.
fn binding(seq: &crate::psx_seq::Sequence, header: &[u8], tone_programs: &[usize]) -> Value {
    use crate::psx_seq::Kind;
    use std::collections::BTreeMap;
    let mut program = [0u8; 16];
    // (program) -> key -> (notes, first event)
    let mut keys: BTreeMap<u8, BTreeMap<u8, (usize, usize)>> = BTreeMap::new();
    for (i, e) in seq.events.iter().enumerate() {
        let ch = (e.status & 0x0F) as usize;
        match e.kind {
            Kind::Program { program: p } => program[ch] = p,
            Kind::NoteOn { key, velocity } if velocity > 0 => {
                let entry = keys
                    .entry(program[ch])
                    .or_default()
                    .entry(key)
                    .or_insert((0, i));
                entry.0 += 1;
            }
            _ => {}
        }
    }
    let mut rows = Vec::new();
    let (mut unbound_notes, mut layered_keys) = (0usize, 0usize);
    for (p, keyset) in &keys {
        let p = *p as usize;
        let block = tone_programs.iter().position(|&q| q == p);
        let count = block.map_or(0, |_| header[crate::vab::PROGRAM_TABLE + p * 16] as usize);
        let ranges: Vec<(usize, u8, u8, usize)> = (0..count)
            .filter_map(|t| {
                block.map(|b| {
                    let at = crate::vab::TONE_TABLE
                        + b * crate::vab::TONE_BLOCK_BYTES
                        + t * crate::vab::TONE_BYTES;
                    let h = &header[at..];
                    (
                        t,
                        h[6],
                        h[7],
                        u16::from_le_bytes([h[0x16], h[0x17]]) as usize,
                    )
                })
            })
            .collect();
        let mut unbound = Vec::new();
        for (&key, &(notes, first)) in keyset {
            let n = ranges.iter().filter(|r| (r.1..=r.2).contains(&key)).count();
            if n == 0 {
                unbound_notes += notes;
                unbound.push(json!({"key": key, "notes": notes, "first_event": first, "offset": seq.events[first].offset}));
            }
            layered_keys += (n > 1) as usize;
        }
        rows.push(json!({"program": p, "program_has_tones": count > 0, "tones": ranges.iter().map(|r| json!({"tone": r.0, "key_range": [r.1, r.2], "vag": r.3})).collect::<Vec<_>>(),
            "keys_played": keyset.keys().collect::<Vec<_>>(),
            "notes": keyset.values().map(|v| v.0).sum::<usize>(),
            "keys_without_tone": unbound}));
    }
    json!({"programs": rows, "notes_without_tone": unbound_notes, "keys_with_layered_tones": layered_keys,
        "rule": "a channel plays VAB program 0 until its first program change; a key plays every used tone of the program whose key range holds it",
        "binding_source": "the SEQ does not name its VAB; the caller chooses the bank"})
}

fn page(summary: &str, items: Vec<Value>, offset: usize, limit: Option<u32>) -> ToolResponse {
    let start = offset.min(items.len());
    let end = start
        .saturating_add(limit.unwrap_or(64).clamp(1, 500) as usize)
        .min(items.len());
    ToolResponse::ok(
        summary,
        json!({"items": &items[start..end], "total": items.len(), "next_cursor": (end < items.len()).then_some(end)}),
    )
}
