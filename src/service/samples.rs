//! Immutable, inspected sample mappings through the built-in codec or an optional external one.
use super::*;
use crate::psx_adpcm;
use crate::sample_codec::{LoopPolicy, PsxOptions, Root, SampleRequest, WavOptions};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
const PLANS: &str = "sample_plan";
const RAW_MAX_BYTES: usize = 262144;
/// A RIFF/WAVE slice may hold up to 32 channels of 64-bit frames; frames stay
/// within the codec's sample limit.
const WAV_MAX_BYTES: usize = 8 << 20;
type SampleFiles = Vec<(String, Vec<u8>)>;
/// Codec report, its three output files, and the codec script when external.
type CodecOutput = (Value, BTreeMap<String, Vec<u8>>, Option<Vec<u8>>);

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareSampleInput {
    pub job_id: String,
    /// Saved original from load_instrument_source; mutually exclusive with bank.
    pub source: Option<String>,
    /// Saved native bank; selects the exact BRR bytes and loop address.
    pub bank: Option<String>,
    pub slot: Option<u8>,
    /// Byte slice within the retained source. Omit length for the remainder.
    #[serde(default)]
    pub offset: usize,
    pub length: Option<usize>,
    /// brr, u8, s8, s16le, s16be, psx_adpcm (PlayStation SPU ADPCM blocks),
    /// vag (a complete VAG file in the slice), or wav (a complete RIFF/WAVE
    /// file in the slice; see `wav`). Bank input requires brr.
    pub encoding: String,
    /// Explicit decoded sample rate, not inferred from the chip clock. For wav
    /// it must equal the header rate unless wav.reinterpret_rate is set.
    pub sample_rate: u32,
    /// Raw encodings: byte offset of the loop start within the slice. WAV loops
    /// use wav.loop_start_frame or wav.smpl_loop. PSX ADPCM/VAG loops come
    /// from the loop-start flag; loop_offset declares the repeat address only
    /// when no block carries that flag.
    pub loop_offset: Option<usize>,
    /// Additional loop traversals to bake, carrying BRR decoder history.
    #[serde(default)]
    pub loop_repeats: u8,
    /// brr, u8, s8, s16le, s16be, psx_adpcm or vag.
    pub target: String,
    /// finite (default) bakes loop_repeats traversals into a one-shot; preserve
    /// keeps the loop in the target and checks loop re-entry.
    #[serde(default = "finite", skip_serializing_if = "is_finite")]
    pub loop_policy: String,
    /// RIFF/WAVE interpretation for encoding=wav. Mono files need none.
    #[serde(default, skip_serializing_if = "no_wav_choice")]
    pub wav: Option<WavInput>,
    /// Resample to this rate (deterministic Kaiser-windowed sinc). Reported as
    /// the `resampled` change; preserved loops must scale to whole samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_sample_rate: Option<u32>,
    /// The MIDI note (and cents) the sample sounds at its declared rate.
    /// Recorded in the plan; BRR targets also get S-DSP pitch register values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootInput>,
    /// PlayStation SPU ADPCM and VAG choices; only for psx_adpcm or vag
    /// encodings or targets.
    #[serde(default, skip_serializing_if = "no_psx_choice")]
    pub psx_adpcm: Option<PsxAdpcmInput>,
    /// Optional external codec script with the same JSON protocol; executed
    /// and snapshotted. Omit to use the built-in codec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default = "python")]
    pub python: String,
}
/// How a RIFF/WAVE source becomes one mono stream. smpl data is reported
/// always but used only through `smpl_loop` and `smpl_root`.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WavInput {
    /// 0-based channel to use. A multi-channel file needs this or downmix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
    /// "mean" averages all channels per frame; reported as channel_downmix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downmix: Option<String>,
    /// Loop start frame; the loop runs to the last frame. Needs loop_policy=preserve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_start_frame: Option<usize>,
    /// Use this smpl loop (0-based): forward, infinite, ending at the last frame.
    /// Needs loop_policy=preserve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smpl_loop: Option<usize>,
    /// Use the smpl unity note and pitch fraction as the root.
    #[serde(default, skip_serializing_if = "is_false")]
    pub smpl_root: bool,
    /// Play the frames at sample_rate although the header declares another rate;
    /// changes pitch and duration and is reported as rate_reinterpreted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reinterpret_rate: bool,
}
/// PlayStation SPU ADPCM and VAG choices.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PsxAdpcmInput {
    /// vag target: the 16-byte name field (printable ASCII, at most 16
    /// bytes). Omitted, a vag source's name is kept and a new header's is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// One-shot psx_adpcm/vag target ending. silent_loop_block (default)
    /// appends the documented silent block with loop start, end and repeat on
    /// itself; end_mute sets loop end without repeat on the last data block.
    #[serde(
        default = "silent_loop_block",
        skip_serializing_if = "is_silent_loop_block"
    )]
    pub one_shot_end: String,
    /// vag source: play the blocks at sample_rate although the header declares
    /// another rate; changes pitch and duration (rate_reinterpreted).
    #[serde(default, skip_serializing_if = "is_false")]
    pub reinterpret_rate: bool,
    /// psx_adpcm/vag source: decode reserved shift values 13..15 as shift 9,
    /// as documented for CD-XA headers (reserved_shift_interpreted). Without
    /// it such blocks are refused.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reserved_shift_as_9: bool,
}
impl Default for PsxAdpcmInput {
    fn default() -> Self {
        PsxAdpcmInput {
            name: None,
            one_shot_end: silent_loop_block(),
            reinterpret_rate: false,
            reserved_shift_as_9: false,
        }
    }
}
fn silent_loop_block() -> String {
    "silent_loop_block".into()
}
fn is_silent_loop_block(value: &str) -> bool {
    value == "silent_loop_block"
}
fn no_psx_choice(psx: &Option<PsxAdpcmInput>) -> bool {
    psx.as_ref().is_none_or(|p| *p == PsxAdpcmInput::default())
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RootInput {
    /// MIDI note number 0..127.
    pub note: u8,
    /// Deviation from the note, -100..100 cents.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cents: f64,
}
fn no_wav_choice(wav: &Option<WavInput>) -> bool {
    wav.as_ref().is_none_or(|w| *w == WavInput::default())
}
fn is_false(value: &bool) -> bool {
    !value
}
fn is_zero(value: &f64) -> bool {
    *value == 0.0
}
fn python() -> String {
    "python3".into()
}
fn finite() -> String {
    "finite".into()
}
fn is_finite(policy: &str) -> bool {
    policy == "finite"
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectSampleInput {
    pub plan: String,
    /// `summary` (default): the change kinds export_sample needs, the
    /// measured changes, loop, tuning and WAV facts and the loop
    /// diagnostics; `full`: the stored report and the whole report_guide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<SampleDetail>,
}

/// How much of a sample plan inspect_sample returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SampleDetail {
    #[default]
    Summary,
    Full,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportSampleInput {
    pub job_id: String,
    pub plan: String,
    pub destination: String,
    #[serde(default)]
    pub accepted_changes: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
struct SamplePlan {
    files: BTreeMap<String, String>,
    report: Value,
}
fn refused(message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::TargetRefused, "/sample", message)
}
impl Service {
    pub(super) fn prepare_sample(&mut self, input: PrepareSampleInput) -> Handled {
        let (original, data, loop_offset) = match (&input.source, &input.bank) {
            (Some(id), None) if input.slot.is_none() => {
                let original = self.sound_source_bytes(id)?;
                let end = input
                    .length
                    .map_or(Some(original.len()), |n| input.offset.checked_add(n))
                    .ok_or_else(|| refused("sample slice overflow"))?;
                let data = original
                    .get(input.offset..end)
                    .ok_or_else(|| refused("sample slice outside original"))?
                    .to_vec();
                (original, data, input.loop_offset)
            }
            (None, Some(id))
                if input.encoding == "brr"
                    && input.offset == 0
                    && input.length.is_none()
                    && input.loop_offset.is_none() =>
            {
                let (bank, _) = self.instrument_bank_data(id)?;
                let sample = bank
                    .sample(input.slot.ok_or_else(|| refused("bank requires slot"))?)
                    .map_err(refused)?;
                (
                    sample.bytes.clone(),
                    sample.bytes,
                    if sample.looping {
                        sample.loop_offset
                    } else {
                        None
                    },
                )
            }
            _ => {
                return Err(refused(
                    "select source with optional slice, or bank and slot with encoding=brr and no slice/loop override",
                ));
            }
        };
        let psx_involved = psx_adpcm::ENCODINGS.contains(&input.encoding.as_str())
            || psx_adpcm::ENCODINGS.contains(&input.target.as_str());
        let limit = if input.encoding == crate::sample_codec::WAV {
            WAV_MAX_BYTES
        } else {
            RAW_MAX_BYTES
        };
        if data.len() > limit {
            return Err(refused(format!("sample slice exceeds {limit} bytes")));
        }
        let wav = match input.wav.as_ref().filter(|w| **w != WavInput::default()) {
            None => None,
            Some(w) => Some(WavOptions {
                channel: w.channel,
                downmix_mean: match w.downmix.as_deref() {
                    None => false,
                    Some("mean") => true,
                    Some(_) => return Err(refused("wav.downmix is \"mean\"")),
                },
                loop_start_frame: w.loop_start_frame,
                smpl_loop: w.smpl_loop,
                smpl_root: w.smpl_root,
                reinterpret_rate: w.reinterpret_rate,
            }),
        };
        let psx = match input
            .psx_adpcm
            .as_ref()
            .filter(|p| **p != PsxAdpcmInput::default())
        {
            None => None,
            Some(p) => Some(PsxOptions {
                name: p.name.clone(),
                end_mute: match p.one_shot_end.as_str() {
                    "silent_loop_block" => false,
                    "end_mute" => true,
                    _ => {
                        return Err(refused(
                            "psx_adpcm.one_shot_end is silent_loop_block or end_mute",
                        ));
                    }
                },
                reinterpret_rate: p.reinterpret_rate,
                reserved_shift_as_9: p.reserved_shift_as_9,
            }),
        };
        let policy = match input.loop_policy.as_str() {
            "finite" => LoopPolicy::Finite,
            "preserve" => LoopPolicy::Preserve,
            _ => return Err(refused("loop_policy is finite or preserve")),
        };
        let (result, mut files, codec) = match &input.codec {
            None => {
                let conversion = crate::sample_codec::convert(&SampleRequest {
                    data: &data,
                    encoding: &input.encoding,
                    target: &input.target,
                    sample_rate: input.sample_rate,
                    loop_offset,
                    loop_repeats: input.loop_repeats,
                    loop_policy: policy,
                    wav,
                    target_sample_rate: input.target_sample_rate,
                    root: input.root.as_ref().map(|r| Root {
                        note: r.note,
                        cents: r.cents,
                    }),
                    psx,
                })
                .map_err(refused)?;
                (json!({"report": conversion.report}), conversion.files, None)
            }
            Some(_) if psx_involved || psx.is_some() => {
                return Err(refused(
                    "PSX ADPCM and VAG use the built-in codec; omit codec",
                ));
            }
            Some(_) if policy == LoopPolicy::Preserve => {
                return Err(refused(
                    "loop_policy=preserve uses the built-in codec; omit codec",
                ));
            }
            Some(_)
                if input.encoding == crate::sample_codec::WAV
                    || wav.is_some()
                    || input.target_sample_rate.is_some()
                    || input.root.is_some() =>
            {
                return Err(refused(
                    "WAV input, resampling and root pitch use the built-in codec; omit codec",
                ));
            }
            Some(path) => self.external_codec(path, &input, &data, loop_offset)?,
        };
        if let Some(codec) = codec {
            files.insert("codec.py".into(), codec);
        }
        files.insert("original.bin".into(), original);
        files.insert("input.bin".into(), data);
        let store = self.store_mut(true)?;
        let objects = files
            .iter()
            .map(|(name, bytes)| Ok((name.clone(), store.put_object(bytes)?)))
            .collect::<Result<BTreeMap<_, _>, StoreError>>()?;
        let authority = if input.codec.is_some() {
            "caller-selected executable; conversion report is codec-reported, not independent hardware verification"
        } else if psx_involved {
            "built-in codec; documented SPU ADPCM decoding equation (and S-DSP integer BRR decoding where BRR is involved), not chip playback verification"
        } else {
            "built-in codec; S-DSP integer BRR decoding, not chip playback verification"
        };
        let mut report = json!({"conversion":result["report"],"source":input.source,"bank":input.bank,"slot":input.slot,"offset":input.offset,"length":input.length,"loop_offset":loop_offset,"loop_repeats":input.loop_repeats,"objects":objects,"loop_policy":input.loop_policy,"original_retained":true,"codec_authority":authority});
        for (key, value) in [
            (
                "wav",
                json!(input.wav.as_ref().filter(|w| **w != WavInput::default())),
            ),
            ("target_sample_rate", json!(input.target_sample_rate)),
            ("root", json!(input.root)),
            (
                "psx_adpcm",
                json!(
                    input
                        .psx_adpcm
                        .as_ref()
                        .filter(|p| **p != PsxAdpcmInput::default())
                ),
            ),
        ] {
            if !value.is_null() {
                report[key] = value;
            }
        }
        let record = SamplePlan {
            files: objects,
            report: report.clone(),
        };
        let id = store.put_object(&serde_json::to_vec(&record).expect("serializes"))?;
        let guide = report_guide(
            &report,
            files.get("sample.bin").map(Vec::as_slice),
            files.get("target.wav").map(Vec::as_slice),
        );
        let response = ToolResponse::ok(
            "sample candidate prepared; inspect measured changes before export",
            json!({"plan":id,"report":report,"report_guide":guide}),
        );
        commit(response, move |w| w.put_document("", PLANS, &id, &record))
    }
    /// Runs a caller-selected codec script over the JSON protocol.
    fn external_codec(
        &self,
        path: &str,
        input: &PrepareSampleInput,
        data: &[u8],
        loop_offset: Option<usize>,
    ) -> Result<CodecOutput, ToolResponse> {
        let codec = std::fs::read(self.workspace.join(path))
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        let script = std::str::from_utf8(&codec).map_err(|e| ToolResponse::error(e.to_string()))?;
        let request = json!({"data":data,"encoding":input.encoding,"target":input.target,"sample_rate":input.sample_rate,"loop_offset":loop_offset,"loop_repeats":input.loop_repeats});
        let mut child = Command::new(&input.python)
            .args(["-I", "-B", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ToolResponse::error(e.to_string()))?;
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
                "sample codec failed: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(2000)
                    .collect::<String>()
            )));
        }
        let result: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            return Err(refused(error));
        }
        let files: BTreeMap<String, Vec<u8>> = serde_json::from_value(result["files"].clone())
            .map_err(|e| ToolResponse::error(e.to_string()))?;
        if files.keys().map(String::as_str).collect::<Vec<_>>()
            != ["sample.bin", "source.wav", "target.wav"]
            || !result["report"]["changes"].is_object()
        {
            return Err(ToolResponse::error("invalid sample codec response"));
        }
        Ok((result, files, Some(codec)))
    }
    fn sample_plan(&mut self, id: &str) -> Result<(SamplePlan, SampleFiles), ToolResponse> {
        let store = self.store_ref()?;
        let record: SamplePlan = store
            .reader()
            .document("", PLANS, id)?
            .ok_or_else(|| refused("unknown sample plan"))?;
        if sha256_hex(&serde_json::to_vec(&record).expect("serializes")) != id {
            return Err(ToolResponse::error("sample plan identity mismatch"));
        }
        let mut files = Vec::new();
        for (name, hash) in &record.files {
            let bytes = std::fs::read(store.object_path(hash))
                .map_err(|e| ToolResponse::error(e.to_string()))?;
            if sha256_hex(&bytes) != *hash {
                return Err(ToolResponse::error("sample object identity mismatch"));
            }
            if name != "codec.py" {
                files.push((name.clone(), bytes));
            }
        }
        files.push((
            "report.json".into(),
            serde_json::to_vec_pretty(&record.report).expect("serializes"),
        ));
        Ok((record, files))
    }
    pub(super) fn inspect_sample(&mut self, input: InspectSampleInput) -> Handled {
        let (record, files) = self.sample_plan(&input.plan)?;
        let file = |wanted: &str| {
            files
                .iter()
                .find(|(name, _)| name == wanted)
                .map(|(_, bytes)| bytes.as_slice())
        };
        let guide = report_guide(&record.report, file("sample.bin"), file("target.wav"));
        done(match input.detail.unwrap_or_default() {
            SampleDetail::Full => ToolResponse::ok(
                "sample conversion and preservation report",
                json!({"plan":input.plan,"report":record.report,"report_guide":guide}),
            ),
            SampleDetail::Summary => ToolResponse::ok(
                "sample plan summary; detail full returns the stored report and the whole report_guide",
                sample_summary(&input.plan, &record.report, &guide),
            ),
        })
    }
    pub(super) fn export_sample(&mut self, input: ExportSampleInput) -> Handled {
        let (record, files) = self.sample_plan(&input.plan)?;
        let required = record.report["conversion"]["changes"]
            .as_object()
            .ok_or_else(|| ToolResponse::error("missing changes"))?;
        let mut accepted = input.accepted_changes.clone();
        accepted.sort();
        accepted.dedup();
        if accepted != required.keys().cloned().collect::<Vec<_>>() {
            return Err(refused(format!(
                "inspect the plan and select exactly these change kinds: {:?}",
                required.keys().collect::<Vec<_>>()
            )));
        }
        let mut response = self.export_sound_files(&input.job_id, &input.destination, files)?;
        response.result["destination"] = json!(self.workspace.join(&input.destination));
        response.result["files"] = response
            .artifacts
            .iter()
            .map(|a| (a.kind.clone(), json!(a.path)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        done(response)
    }
}

/// The compact inspect_sample result: what export_sample needs and what
/// decides whether the candidate is usable, without the chunk inventory,
/// object hashes, scope text and reading aids of the full report.
fn sample_summary(plan: &str, report: &Value, guide: &Value) -> Value {
    let conversion = &report["conversion"];
    let pick = |from: &Value, keys: &[&str]| -> Value {
        keys.iter()
            .filter(|k| !from[**k].is_null())
            .map(|k| ((*k).to_owned(), from[*k].clone()))
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let mut summary = json!({
        "plan": plan,
        "detail": "summary",
        "accepted_changes": conversion["changes"].as_object().map(|c| c.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
        "conversion": pick(conversion, &["encoding", "target", "sample_rate", "target_sample_rate",
            "source_samples", "target_samples", "sample_values_exact", "decoded_stream_exact", "changes", "loop"]),
        "selection": pick(report, &["source", "bank", "slot", "offset", "length", "loop_policy"]),
        "more": "detail: full returns the stored report (scope, WAV chunk inventory, object hashes, PSX ADPCM details) and the whole report_guide (histograms, value domains, null fields)",
    });
    if conversion["wav"].is_object() {
        summary["conversion"]["wav"] = pick(
            &conversion["wav"],
            &[
                "channels",
                "header_sample_rate",
                "bits_per_sample",
                "frames",
                "selection",
                "smpl_used",
            ],
        );
    }
    let tuning = &conversion["tuning"];
    if tuning.is_object() {
        let mut compact = pick(tuning, &["sample_rate", "root"]);
        for chip in ["snes_pitch", "spu_pitch"] {
            if tuning[chip].is_object() {
                compact[chip] = pick(
                    &tuning[chip],
                    &["original_pitch", "root_in_tune", "in_tune_note_range"],
                );
            }
        }
        summary["conversion"]["tuning"] = compact;
    }
    if let Some(brr) = guide.get("brr_target") {
        summary["brr_target"] = pick(
            brr,
            &[
                "blocks",
                "end_block",
                "end_flags",
                "loop_start_block",
                "loop_block_filter_0",
            ],
        );
    }
    if let Some(diagnostics) = guide.get("loop_diagnostics") {
        summary["loop_diagnostics"] = diagnostics.clone();
    }
    summary
}

/// How a preserved loop repeats in the decoded target (`target.wav`: the
/// first pass, then one traversal), with loop starts that would make it a
/// whole number of periods when it is not and the source loop is chosen in
/// samples or frames.
fn loop_diagnostics(conversion: &Value, target_wav: &[u8]) -> Option<Value> {
    let file = crate::wav::parse(target_wav).ok()?;
    if file.bits != 16 || file.channels != 1 {
        return None;
    }
    let stream: Vec<i32> = file
        .data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i32::from(i16::from_le_bytes(*b)))
        .collect();
    let first_pass = match &file.smpl {
        Some(Ok(smpl)) => smpl.loops.first()?.start as usize,
        _ => return None,
    };
    let source_rate = conversion["sample_rate"].as_u64()? as u32;
    let rate = conversion["target_sample_rate"]
        .as_u64()
        .map_or(source_rate, |r| r as u32);
    let loop_report = &conversion["loop"];
    let loop_start = loop_report["target_loop_start_sample"]
        .as_u64()
        .or(loop_report["loop_start_sample"].as_u64())? as usize;
    let root = conversion["tuning"]["root"]["note"].as_f64().map(|note| {
        note + conversion["tuning"]["root"]["cents"]
            .as_f64()
            .unwrap_or(0.0)
            / 100.0
    });
    let mut diagnostics =
        crate::sample_loop::diagnostics(&stream, first_pass, loop_start, rate, root);
    let jump = diagnostics["phase_jump_periods"].as_f64();
    let encoding = conversion["encoding"].as_str().unwrap_or_default();
    let chosen_in_samples =
        encoding == crate::sample_codec::WAV || ["u8", "s8", "s16le", "s16be"].contains(&encoding);
    if let (Some(jump), Some(hz), true) = (
        jump,
        diagnostics["fundamental_hz"].as_f64(),
        chosen_in_samples,
    ) && jump.abs() > crate::sample_loop::PHASE_TOLERANCE
    {
        let block = match conversion["target"].as_str() {
            Some("brr") => 16,
            Some(t) if crate::psx_adpcm::ENCODINGS.contains(&t) => {
                crate::psx_adpcm::BLOCK_SAMPLES as u64
            }
            _ => 1,
        };
        let step = crate::sample_loop::source_step(block, source_rate, rate);
        let length = conversion["source_samples"].as_u64()?;
        let start = loop_report["loop_start_sample"].as_u64()?;
        let whole = crate::sample_loop::whole_period_starts(
            length,
            start,
            step,
            f64::from(source_rate) / hz,
            3,
        );
        let rates = crate::sample_loop::whole_period_rates(
            (length, start),
            block,
            (source_rate, rate),
            f64::from(source_rate) / hz,
            (3, crate::sample_codec::MAX_SAMPLES as u64),
        );
        let field = if encoding == crate::sample_codec::WAV {
            "wav.loop_start_frame"
        } else {
            "loop_offset (bytes: the sample times its width)"
        };
        diagnostics["whole_period_starts"] = json!({
            "starts": whole.iter().map(|(s, p)| json!({"source_start": s, "periods": crate::measure::round(*p, 3)})).collect::<Vec<_>>(),
            "other_rates": rates.iter().map(|(t, s, p)| json!({"target_sample_rate": t, "source_start": s, "periods": crate::measure::round(*p, 3)})).collect::<Vec<_>>(),
            "set_with": field,
            "note": format!("source loop starts, nearest first, that keep the target's alignment at this rate (multiples of {step} source samples) or at other_rates, with the full source length, and make the loop a whole number of periods within {} period; the waveform there is not checked until you prepare it", crate::sample_loop::WHOLE_PERIOD_TOLERANCE),
        });
    }
    Some(diagnostics)
}

/// Reading aids for a sample plan's report, derived from the report and the
/// candidate's `sample.bin` and `target.wav`. They are not part of the stored
/// report, so plan identities and exported reports do not change.
fn report_guide(report: &Value, sample: Option<&[u8]>, target_wav: Option<&[u8]>) -> Value {
    let conversion = &report["conversion"];
    let keyed = |counts: &Value| -> Value {
        counts
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, n)| n.as_u64().is_some_and(|n| n > 0))
            .map(|(i, n)| (i.to_string(), n.clone()))
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let mut guide = serde_json::Map::new();
    let mut histograms = serde_json::Map::new();
    for side in ["psx_adpcm", "psx_adpcm_target"] {
        for name in ["filters", "shifts"] {
            if conversion[side][name].is_array() {
                histograms.insert(
                    format!("conversion.{side}.{name}"),
                    keyed(&conversion[side][name]),
                );
            }
        }
    }
    if conversion["target"] == "brr"
        && let Some(bytes) = sample.filter(|b| !b.is_empty() && b.len().is_multiple_of(9))
    {
        let headers: Vec<u8> = bytes.chunks(9).map(|b| b[0]).collect();
        let (mut filters, mut shifts) = ([0u64; 4], [0u64; 16]);
        for h in &headers {
            filters[usize::from((h >> 2) & 3)] += 1;
            shifts[usize::from(h >> 4)] += 1;
        }
        let end = headers.iter().position(|h| h & 1 == 1);
        let mut brr = json!({
            "blocks": headers.len(), "filters": filters, "shifts": shifts,
            "first_block_filter_0": (headers[0] >> 2) & 3 == 0,
            "end_block": end, "end_flags": end.map(|e| headers[e] & 3),
            "encoder": "greedy per-block search; filter 0 is forced on the first block and on the loop block, so neither depends on history at entry",
        });
        if let Some(offset) = conversion["loop"]["target_loop_offset"].as_u64() {
            let block = offset as usize / 9;
            brr["loop_start_block"] = json!(block);
            brr["loop_block_filter_0"] = json!(headers.get(block).map(|h| (h >> 2) & 3 == 0));
        }
        histograms.insert("brr_target.filters".into(), keyed(&brr["filters"]));
        histograms.insert("brr_target.shifts".into(), keyed(&brr["shifts"]));
        guide.insert("brr_target".into(), brr);
    }
    if conversion["loop"].is_object()
        && let Some(diagnostics) = target_wav.and_then(|w| loop_diagnostics(conversion, w))
    {
        guide.insert("loop_diagnostics".into(), diagnostics);
    }
    if !histograms.is_empty() {
        histograms.insert(
            "meaning".into(),
            json!("filters[i] and shifts[i] count the blocks that use filter i or shift i; these objects repeat the arrays keyed by that number and leave out numbers no block uses"),
        );
        guide.insert("histograms".into(), Value::Object(histograms));
    }
    let resampled = conversion["target_sample_rate"].is_u64();
    let mut domains = serde_json::Map::new();
    domains.insert(
        "conversion.sample_rate".into(),
        json!(if resampled {
            "source: the declared source rate; the target plays at conversion.target_sample_rate"
        } else {
            "source and target: the declared rate, unchanged"
        }),
    );
    domains.insert(
        "conversion.source_samples".into(),
        json!("source: decoded samples at conversion.sample_rate"),
    );
    domains.insert(
        "conversion.target_samples".into(),
        json!("target: decoded candidate samples at the target rate, block padding included (the first pass for a preserved loop)"),
    );
    let loop_report = &conversion["loop"];
    if loop_report.is_object() {
        domains.insert(
            "conversion.loop.loop_start_sample".into(),
            json!(if resampled {
                "source: loop start before resampling; the target's is conversion.loop.target_loop_start_sample"
            } else {
                "source and target: loop start sample (no resampling)"
            }),
        );
        if loop_report["source_loop_frame"].is_u64() {
            domains.insert(
                "conversion.loop.source_loop_frame".into(),
                json!("source: the WAV frame the loop starts at"),
            );
        }
        domains.insert(
            "conversion.loop.target_loop_offset".into(),
            json!("target: byte offset of the loop in sample.bin (a VAG header included)"),
        );
    }
    if conversion["tuning"].is_object() {
        domains.insert(
            "conversion.tuning.sample_rate".into(),
            json!("target: the rate the pitch values are computed for"),
        );
    }
    guide.insert("value_domains".into(), Value::Object(domains));
    let mut nulls = serde_json::Map::new();
    if report["loop_offset"].is_null() {
        nulls.insert(
            "loop_offset".into(),
            json!(if conversion["encoding"] == crate::sample_codec::WAV {
                "a raw-slice byte field; WAV loops are chosen with wav.loop_start_frame or wav.smpl_loop, so it is always null for WAV"
            } else {
                "no loop offset was given or taken from a bank; PSX ADPCM and VAG loops come from their flags"
            }),
        );
    }
    if report["length"].is_null() {
        nulls.insert(
            "length".into(),
            json!("the slice runs from offset to the end of the retained source"),
        );
    }
    if !nulls.is_empty() {
        guide.insert("null_fields".into(), Value::Object(nulls));
    }
    Value::Object(guide)
}
