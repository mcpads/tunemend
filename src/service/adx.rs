//! CRI ADX streams: `inspect_adx`, `render_adx` and `encode_adx` over
//! sound sources retained with `load_instrument_source`.
use super::instrument_sources::{SOURCES, SoundSourceRecord};
use super::*;
use crate::adx::{self, Adx, Arithmetic, Decoder, EncodeParams};

/// Block equation reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdxArithmetic {
    /// Coefficients rounded to 4.12 fixed point, floor shift, clamped history.
    #[default]
    #[serde(rename = "fixed_12")]
    Fixed12,
    /// The described floating-point example: truncated prediction,
    /// unclamped history.
    FloatReference,
}

impl AdxArithmetic {
    fn is_default(&self) -> bool {
        *self == Self::Fixed12
    }
    fn reading(self) -> Arithmetic {
        match self {
            Self::Fixed12 => Arithmetic::Fixed12,
            Self::FloatReference => Arithmetic::FloatReference,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdxView {
    /// Header interpretation, loop, ending, unknown bytes and coefficients.
    #[default]
    Summary,
    /// Every header field with offset, size, raw bytes and value.
    Header,
    /// Paged block rows: per channel the offset, scale and nibble bytes.
    Blocks,
    /// Paged decoded frames with each value's byte, nibble, scale and prediction.
    Samples,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectAdxInput {
    /// A retained ADX sound source (load_instrument_source).
    pub source: String,
    #[serde(default)]
    pub view: AdxView,
    /// First block row (blocks) or frame (samples); follow next_cursor.
    #[serde(default)]
    pub offset: u64,
    /// Rows per page (default 16, at most 128) or frames (default 64, at most 500).
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "AdxArithmetic::is_default")]
    pub arithmetic: AdxArithmetic,
}

/// What `render_adx` decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdxSpan {
    /// Every sample once, as stored (default).
    Stream,
    /// The stream to the loop end, then `passes - 1` more traversals from
    /// the loop start with carried history (1..16).
    Loop { passes: u32 },
    /// Frames `start..end` of the stream decoded from its beginning.
    Frames { start: u64, end: u64 },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderAdxInput {
    pub job_id: String,
    /// A retained ADX sound source (load_instrument_source).
    pub source: String,
    /// New folder relative to the workspace for `decoded.wav`,
    /// `original.adx` and `render.json`.
    pub destination: String,
    /// Default `{"kind": "stream"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<AdxSpan>,
    #[serde(default, skip_serializing_if = "AdxArithmetic::is_default")]
    pub arithmetic: AdxArithmetic,
}

/// A change `encode_adx` reports and refuses until accepted.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AdxChange {
    /// Decoded samples differ from the PCM16 source (ADPCM is lossy).
    AdpcmQuantization,
    /// WAV chunks other than fmt and data (including smpl loops or a root
    /// note not written as the ADX loop) are not carried into the ADX.
    WavMetadataNotCarried,
    /// Silent frames were prepended (`loop_start_padding`); every frame and
    /// the loop move later by their count.
    LeadInPadding,
}

impl AdxChange {
    fn name(self) -> &'static str {
        match self {
            Self::AdpcmQuantization => "adpcm_quantization",
            Self::WavMetadataNotCarried => "wav_metadata_not_carried",
            Self::LeadInPadding => "lead_in_padding",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdxLoopFrames {
    /// First looped frame; a multiple of 32 (block boundary).
    pub start: u32,
    /// Exclusive end frame, at most the frame count.
    pub end: u32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EncodeAdxInput {
    pub job_id: String,
    /// A retained 16-bit integer PCM WAV (load_instrument_source), 1..16
    /// channels, at most 192000 Hz. Its rate becomes the ADX rate.
    pub source: String,
    /// Declared highpass cutoff in Hz written to the header; it derives the
    /// predictor coefficients. Observed CRI streams use 500.
    pub highpass_frequency: u16,
    /// Loop written into the version 3 header; omit for a one-shot stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_frames: Option<AdxLoopFrames>,
    /// Prepend silent frames so a loop start off a 32-frame boundary lands
    /// on one, and write their count to the loop alignment field (as observed
    /// CRI streams do). Every frame moves later by that count, reported as
    /// `lead_in_padding`. Without it such a loop start refuses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub loop_start_padding: bool,
    /// Align the loop-start byte and the file length to this many bytes,
    /// such as 2048 for CD sectors; omit for the smallest header and an
    /// 18-byte end block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector_bytes: Option<u32>,
    /// Change kinds you accept; any other reported change refuses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_changes: Vec<AdxChange>,
    /// New folder relative to the workspace for the ADX, `decoded.wav` and
    /// `encode.json`.
    pub destination: String,
    /// File name of the written stream (default `encoded.adx`); a plain
    /// name ending in `.adx`. It also names the new retained source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adx_name: Option<String>,
}

/// Frames `from..=to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdxFrameRange {
    pub from: u32,
    pub to: u32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RankAdxLoopsInput {
    /// A retained 16-bit integer PCM WAV (load_instrument_source or a
    /// render_adx decoded_source), the input encode_adx would take.
    pub source: String,
    /// The highpass encode_adx will declare; with the WAV rate it sets the
    /// predictor whose response is predicted.
    pub highpass_frequency: u16,
    /// Loop starts to consider: every multiple of 32 within `from..=to`.
    pub start: AdxFrameRange,
    /// Exclusive loop ends to consider: every frame within `from..=to`, at
    /// most the frame count. At most 1,000,000 start x end pairs.
    pub end: AdxFrameRange,
    /// Shortest loop in frames (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    /// Candidates returned, best first (default 10, at most 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

pub(super) fn adx_located(field: &str, error: adx::AdxError) -> ToolResponse {
    let mut response = ToolResponse::refused(
        Code::InvalidRange,
        &format!("{field}/bytes/{}", error.offset),
        format!(
            "byte {} (0x{:x}) {}: {}; the retained original is unchanged",
            error.offset, error.offset, error.field, error.message
        ),
    );
    response.result = json!({"error_offset":error.offset,"field":error.field,"message":error.message,"retained":true});
    response
}

fn refused(location: &str, message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::InvalidRange, location, message)
}

/// Facts `load_instrument_source` reports for a stream with the ADX signature.
pub(super) fn discovery_hint(bytes: &[u8]) -> Value {
    let mut hint = json!({"format":"adx","inspect_with":"inspect_adx","render_with":"render_adx"});
    match Adx::parse(bytes) {
        Ok(a) => {
            let r = a.report(Arithmetic::Fixed12);
            for key in [
                "version",
                "encoding_type",
                "channels",
                "sample_rate",
                "total_samples",
                "duration_seconds",
                "highpass_frequency",
            ] {
                hint[key] = r[key].clone();
            }
            hint["loop"] = r["loop"]["state"].clone();
        }
        Err(e) => hint["unusable"] = json!(format!("byte {} {}: {}", e.offset, e.field, e.message)),
    }
    hint
}

fn stats(source: &[i16], decoded: &[i16], channels: usize) -> Value {
    let frames = source.len() / channels;
    let mut rows = vec![];
    for c in 0..channels {
        let (mut err, mut sig, mut max, mut differing, mut first) = (0f64, 0f64, 0i32, 0u64, None);
        for i in 0..frames {
            let (x, y) = (source[i * channels + c], decoded[i * channels + c]);
            let d = i32::from(y) - i32::from(x);
            if d != 0 {
                differing += 1;
                first.get_or_insert(i);
            }
            max = max.max(d.abs());
            err += f64::from(d).powi(2);
            sig += f64::from(x).powi(2);
        }
        let snr = if err == 0.0 {
            Value::Null
        } else if sig == 0.0 {
            json!(f64::NEG_INFINITY.to_string())
        } else {
            json!(10.0 * (sig / err).log10())
        };
        rows.push(json!({
            "channel": c, "differing_samples": differing, "first_differing_frame": first,
            "max_abs_difference": max, "rms_difference": (err / frames.max(1) as f64).sqrt(),
            "snr_db": snr,
        }));
    }
    json!(rows)
}

impl Service {
    fn retained_adx(&mut self, source: &str) -> Result<Vec<u8>, ToolResponse> {
        self.sound_source_bytes(source)
    }

    pub(super) fn inspect_adx(&mut self, input: InspectAdxInput) -> Handled {
        let bytes = self.retained_adx(&input.source)?;
        let a = Adx::parse(&bytes).map_err(|e| adx_located("/source", e))?;
        let arithmetic = input.arithmetic.reading();
        let source = json!(input.source);
        match input.view {
            AdxView::Summary => {
                let mut result = a.report(arithmetic);
                result["source"] = source;
                done(ToolResponse::ok(
                    format!(
                        "ADX version {}, {} channel(s) at {} Hz, {} samples ({:.2} s); loop {}",
                        a.version,
                        a.channels,
                        a.sample_rate,
                        a.total_samples,
                        f64::from(a.total_samples) / f64::from(a.sample_rate),
                        result["loop"]["state"].as_str().unwrap_or("absent")
                    ),
                    result,
                ))
            }
            AdxView::Header => done(ToolResponse::ok(
                "ADX header fields",
                json!({"source":source,"version":a.version,"data_start":a.data_start,
                       "fields":a.fields(),"unknown_header_bytes":a.report(arithmetic)["unknown_header_bytes"]}),
            )),
            AdxView::Blocks => {
                let limit = input.limit.unwrap_or(16).clamp(1, 128) as u64;
                let start = input.offset.min(a.rows as u64) as usize;
                let end = (start as u64 + limit).min(a.rows as u64) as usize;
                let spb = a.samples_per_block();
                let rows: Vec<Value> = (start..end)
                    .map(|row| {
                        let blocks: Vec<Value> = (0..a.channels)
                            .map(|c| {
                                let at = a.block_offset(row, c);
                                json!({"channel": c, "offset": at, "scale": a.scale(row, c),
                                       "nibbles_hex": crate::adx::hex_bytes(&bytes[at + 2..at + a.block_size])})
                            })
                            .collect();
                        let first = row * spb;
                        json!({"row": row, "first_frame": first,
                               "frames": spb.min(a.total_samples as usize - first), "blocks": blocks})
                    })
                    .collect();
                done(ToolResponse::ok(
                    format!("block rows {start}..{end} of {}", a.rows),
                    json!({"source":source,"rows":rows,"total_rows":a.rows,
                           "next_cursor":(end < a.rows).then_some(end)}),
                ))
            }
            AdxView::Samples => {
                let limit = input.limit.unwrap_or(64).clamp(1, 500) as u64;
                let total = u64::from(a.total_samples);
                if input.offset >= total {
                    return Err(refused(
                        "/offset",
                        format!("offset {} is not below the {total} frames", input.offset),
                    ));
                }
                let start = input.offset as usize;
                let end = (input.offset + limit).min(total) as usize;
                let mut decoder = Decoder::new(&a, arithmetic);
                let mut skipped = vec![];
                decoder.decode(0, start, &mut skipped);
                let spb = a.samples_per_block();
                let frames: Vec<Value> = (start..end)
                    .map(|frame| {
                        let values: Vec<Value> = (0..a.channels)
                            .map(|c| {
                                let s = decoder.step(frame, c);
                                json!({"channel": c, "value": s.output, "byte": s.byte,
                                       "nibble": if s.high_nibble {"high"} else {"low"},
                                       "code": s.nibble, "scale": s.scale,
                                       "prediction": s.prediction, "sum": s.sum,
                                       "clamped": i64::from(s.output) != s.sum})
                            })
                            .collect();
                        json!({"frame": frame, "row": frame / spb, "index_in_block": frame % spb, "channels": values})
                    })
                    .collect();
                done(ToolResponse::ok(
                    format!("decoded frames {start}..{end} of {total}"),
                    json!({"source":source,"arithmetic":arithmetic.name(),
                           "history":"zero at frame 0; earlier frames decoded first","frames":frames,
                           "total_frames":total,"next_cursor":(end < total as usize).then_some(end)}),
                ))
            }
        }
    }

    pub(super) fn render_adx(&mut self, input: RenderAdxInput) -> Handled {
        let bytes = self.retained_adx(&input.source)?;
        let a = Adx::parse(&bytes).map_err(|e| adx_located("/source", e))?;
        let arithmetic = input.arithmetic.reading();
        let span = input.span.unwrap_or(AdxSpan::Stream);
        let ch = a.channels;
        let total = u64::from(a.total_samples);
        let (pcm, mut report, clamped, smpl) = match span {
            AdxSpan::Stream => {
                let (pcm, clamped) =
                    adx::decode_stream(&a, arithmetic).map_err(|e| refused("/span", e))?;
                let smpl = a.looping.map(|l| (l.start_sample, l.end_sample - 1));
                (pcm, json!({"span":"stream","frames":total}), clamped, smpl)
            }
            AdxSpan::Loop { passes } => {
                if !(1..=16).contains(&passes) {
                    return Err(refused("/span/passes", "passes must be 1..16"));
                }
                if a.looping.is_none() {
                    return Err(refused(
                        "/span",
                        format!(
                            "the header declares no loop (loop {}); render {{\"kind\": \"stream\"}}",
                            a.report(arithmetic)["loop"]["state"]
                        ),
                    ));
                }
                let (pcm, mut r, clamped) =
                    adx::render_loop(&a, arithmetic, passes).map_err(|e| refused("/span", e))?;
                r["span"] = json!("loop");
                (pcm, r, clamped, None)
            }
            AdxSpan::Frames { start, end } => {
                if start >= end || end > total {
                    return Err(refused(
                        "/span",
                        format!("frames need start < end <= {total}"),
                    ));
                }
                adx::check_budget(end * ch as u64).map_err(|e| refused("/span", e))?;
                let mut decoder = Decoder::new(&a, arithmetic);
                let mut skipped = vec![];
                decoder.decode(0, start as usize, &mut skipped);
                let before = decoder.clamped;
                let mut pcm = vec![];
                decoder.decode(start as usize, end as usize, &mut pcm);
                let smpl = a
                    .looping
                    .filter(|l| {
                        u64::from(l.start_sample) >= start && u64::from(l.end_sample) <= end
                    })
                    .map(|l| {
                        (
                            l.start_sample - start as u32,
                            l.end_sample - 1 - start as u32,
                        )
                    });
                (
                    pcm,
                    json!({"span":"frames","start":start,"end":end,"frames":end - start,
                           "history":"decoded from frame 0, so values equal the stream span"}),
                    decoder.clamped - before,
                    smpl,
                )
            }
        };
        let wav = adx::pcm16_wav(&pcm, ch, a.sample_rate, smpl);
        let frames = pcm.len() / ch;
        report["source"] = json!(input.source);
        report["header"] = a.report(arithmetic);
        report["arithmetic"] = json!({"name": arithmetic.name(), "rule": arithmetic.rule()});
        report["wav"] = json!({
            "file": "decoded.wav", "format": "pcm16", "channels": ch, "sample_rate": a.sample_rate,
            "frames": frames, "seconds": frames as f64 / f64::from(a.sample_rate),
            "smpl_loop": smpl.map(|(s, e)| json!({"start_frame": s, "end_frame_inclusive": e})),
            "sha256": sha256_hex(&wav),
        });
        report["clamped_samples"] = json!(clamped);
        report["peak"] = json!(pcm.iter().map(|v| i32::from(*v).abs()).max().unwrap_or(0));
        report["scope"] = json!(
            "decoded ADX samples at the header rate; no console mixer, resampling, volume or CD playback timing"
        );
        let report_bytes = serde_json::to_vec_pretty(&report).expect("serializes");
        let mut response = self.export_sound_files(
            &input.job_id,
            &input.destination,
            vec![
                ("decoded.wav".into(), wav.clone()),
                ("original.adx".into(), bytes.clone()),
                ("render.json".into(), report_bytes),
            ],
        )?;
        let wav_path = Path::new(&input.destination).join("decoded.wav");
        let wav_hash = report["wav"]["sha256"].as_str().expect("hash").to_owned();
        report["measure_with"] =
            json!({"audio": {"path": wav_path.display().to_string(), "expected_sha256": wav_hash}});
        report["decoded_source"] = json!(wav_hash);
        let mut encode = json!({"source": wav_hash, "highpass_frequency": a.highpass});
        if let Some((start, end)) = smpl {
            encode["loop_frames"] = json!({"start": start, "end": end + 1});
        }
        report["encode_adx"] = json!({"arguments": encode,
            "note": "add job_id, destination and accept_changes; set loop_start_padding for a loop start off a 32-frame boundary"});
        if ch == 1 && frames <= crate::sample_codec::MAX_SAMPLES {
            let mut prepare =
                json!({"source": wav_hash, "encoding": "wav", "sample_rate": a.sample_rate});
            if smpl.is_some() {
                prepare["wav"] = json!({"smpl_loop": 0});
                prepare["loop_policy"] = json!("preserve");
            }
            report["prepare_sample"] =
                json!({"arguments": prepare, "note": "add job_id and target"});
        }
        report["next"] = json!(
            "decoded.wav is retained as decoded_source: measure_audio and compare_audio take measure_with.audio; encode_adx and prepare_sample (one channel, at most 8388608 frames) take decoded_source without another load"
        );
        report.as_object_mut().expect("object").remove("header");
        response.summary = format!(
            "decoded {frames} frames x {ch} channel(s) at {} Hz ({}); {} clamped sample(s); decoded.wav retained as source {}",
            a.sample_rate,
            report["span"].as_str().unwrap_or("stream"),
            clamped,
            &wav_hash[..12]
        );
        response.result = report;
        let store = self.store_mut(true)?;
        store.put_object(&wav)?;
        let record = SoundSourceRecord {
            source: wav_hash.clone(),
            name: format!("{}-decoded.wav", input.job_id),
            bytes: wav.len(),
            derived_from: Some(
                json!({"source": input.source, "tool": "render_adx", "job_id": input.job_id,
                                      "span": response.result["span"], "arithmetic": arithmetic.name()}),
            ),
        };
        response.artifacts.push(Artifact {
            kind: "decoded_adx_source".into(),
            path: store.object_path(&wav_hash).display().to_string(),
            sha256: wav_hash.clone(),
        });
        commit(response, move |writer| {
            writer.put_document("", SOURCES, &wav_hash, &record)
        })
    }

    pub(super) fn rank_adx_loops(&mut self, input: RankAdxLoopsInput) -> Handled {
        let bytes = self.sound_source_bytes(&input.source)?;
        let wav = crate::wav::parse(&bytes).map_err(|e| {
            refused(
                &format!("/source/bytes/{}", e.offset),
                format!("the source is not a usable WAV: {e}"),
            )
        })?;
        let ch = wav.channels as usize;
        if wav.format != crate::wav::Format::Int
            || wav.bits != 16
            || ch == 0
            || ch > adx::MAX_CHANNELS as usize
            || wav.sample_rate == 0
            || wav.sample_rate > adx::MAX_SAMPLE_RATE
            || wav.frames == 0
        {
            return Err(refused(
                "/source",
                "rank_adx_loops reads what encode_adx reads: a 16-bit integer PCM WAV with 1..16 channels at 1..192000 Hz",
            ));
        }
        let frames = wav.frames;
        let (s, e) = (input.start, input.end);
        if s.from > s.to || e.from > e.to || e.to as usize > frames {
            return Err(refused(
                "/end",
                format!("ranges need from <= to, and end.to at most the {frames} frames"),
            ));
        }
        let starts: Vec<usize> = (s.from.div_ceil(32) * 32..=s.to)
            .step_by(adx::BLOCK_SAMPLES)
            .map(|v| v as usize)
            .filter(|&v| v < frames)
            .collect();
        if starts.is_empty() {
            return Err(refused(
                "/start",
                format!(
                    "no multiple of 32 lies in {}..={}; encode_adx loop starts are 32-frame aligned (or use loop_start_padding)",
                    s.from, s.to
                ),
            ));
        }
        let ends: Vec<usize> = (e.from.max(1)..=e.to).map(|v| v as usize).collect();
        let pairs = starts.len() as u64 * ends.len() as u64;
        if pairs > adx::MAX_LOOP_PAIRS {
            return Err(refused(
                "/end",
                format!(
                    "{} starts x {} ends = {pairs} pairs exceed {}; narrow start or end",
                    starts.len(),
                    ends.len(),
                    adx::MAX_LOOP_PAIRS
                ),
            ));
        }
        let interleaved: Vec<i16> = wav
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .take(frames * ch)
            .map(|w| i16::from_le_bytes(*w))
            .collect();
        let channels: Vec<Vec<i16>> = (0..ch)
            .map(|c| interleaved.iter().skip(c).step_by(ch).copied().collect())
            .collect();
        let limit = input.limit.unwrap_or(10).clamp(1, 50) as usize;
        let (ranked, evaluated) = adx::rank_loops(
            &channels,
            input.highpass_frequency,
            wav.sample_rate,
            &starts,
            &ends,
            input.min_length.unwrap_or(1) as usize,
            limit,
        );
        if ranked.is_empty() {
            return Err(refused(
                "/min_length",
                "no start and end pair satisfies min_length",
            ));
        }
        let candidates: Vec<Value> = ranked
            .iter()
            .enumerate()
            .map(|(i, c)| {
                json!({
                    "rank": i + 1, "loop_frames": {"start": c.start, "end": c.end},
                    "length": c.end - c.start,
                    "predicted_reentry_difference": (c.predicted_reentry * 10.0).round() / 10.0,
                    "predicted_settle_frames": adx::predicted_settle(&channels, input.highpass_frequency, wav.sample_rate, c.start, c.end),
                    "seam_value_difference": c.seam_value, "seam_slope_difference": c.seam_slope,
                })
            })
            .collect();
        let best = &ranked[0];
        done(ToolResponse::ok(
            format!(
                "ranked {evaluated} loop pairs ({} starts x {} ends); best {}..{} predicts a re-entry difference of {:.1}",
                starts.len(),
                ends.len(),
                best.start,
                best.end,
                best.predicted_reentry
            ),
            json!({
                "source": input.source, "frames": frames, "channels": ch, "sample_rate": wav.sample_rate,
                "highpass_frequency": input.highpass_frequency,
                "starts": starts.len(), "ends": ends.len(), "pairs_evaluated": evaluated,
                "candidates": candidates,
                "model": adx::LOOP_RANK_RULE,
                "next": "nothing is applied: pass a candidate's loop_frames to encode_adx, then check its loop_reentry (or render_adx loop passes) and listen; quantization and clamping are not in the prediction",
            }),
        ))
    }

    pub(super) fn encode_adx(&mut self, input: EncodeAdxInput) -> Handled {
        let name = input.adx_name.as_deref().unwrap_or("encoded.adx");
        let plain = !name.is_empty()
            && name.len() <= 128
            && !name.starts_with('.')
            && !name.contains(['/', '\\', ':'])
            && name.ends_with(".adx");
        if !plain {
            return Err(refused(
                "/adx_name",
                "give a plain file name ending in .adx, without a folder",
            ));
        }
        let bytes = self.sound_source_bytes(&input.source)?;
        let wav = crate::wav::parse(&bytes).map_err(|e| {
            refused(
                &format!("/source/bytes/{}", e.offset),
                format!("the source is not a usable WAV: {e}"),
            )
        })?;
        if wav.format != crate::wav::Format::Int || wav.bits != 16 {
            return Err(refused(
                "/source",
                format!(
                    "encode_adx reads 16-bit integer PCM; this WAV is {} {}-bit. Prepare a PCM16 WAV first (render_adx and render tools write PCM16)",
                    if wav.format == crate::wav::Format::Int {
                        "integer"
                    } else {
                        "float"
                    },
                    wav.bits
                ),
            ));
        }
        let ch = wav.channels as usize;
        if ch == 0 || ch > adx::MAX_CHANNELS as usize {
            return Err(refused(
                "/source",
                format!("{ch} channels; 1..16 are written"),
            ));
        }
        if wav.sample_rate == 0 || wav.sample_rate > adx::MAX_SAMPLE_RATE {
            return Err(refused(
                "/source",
                format!("sample rate {} is outside 1..192000 Hz", wav.sample_rate),
            ));
        }
        let frames = wav.frames;
        if frames == 0 {
            return Err(refused("/source", "the WAV has no frames"));
        }
        adx::check_budget((frames * ch) as u64).map_err(|e| refused("/source", e))?;
        if let Some(sector) = input.sector_bytes
            && !(16..=32768).contains(&sector)
        {
            return Err(refused("/sector_bytes", "sector_bytes must be 16..32768"));
        }
        let interleaved: Vec<i16> = wav
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .take(frames * ch)
            .map(|w| i16::from_le_bytes(*w))
            .collect();
        if let Some(l) = input.loop_frames {
            if l.start >= l.end || l.end as usize > frames {
                return Err(refused(
                    "/loop_frames",
                    format!("loop_frames needs start < end <= {frames}"),
                ));
            }
            if !(l.start as usize).is_multiple_of(adx::BLOCK_SAMPLES) && !input.loop_start_padding {
                let below = l.start - l.start % 32;
                return Err(refused(
                    "/loop_frames/start",
                    format!(
                        "loop start {} is not on a 32-frame block boundary, which every observed looping stream uses; choose {below} or {} (the loop end may stay {}), or set loop_start_padding to prepend {} silent frames as CRI streams do (lead_in_padding)",
                        l.start,
                        below + 32,
                        l.end,
                        (32 - l.start % 32) % 32
                    ),
                ));
            }
        }
        let padding = match input.loop_frames {
            Some(l) if input.loop_start_padding => (32 - l.start % 32) % 32,
            _ => 0,
        };
        let interleaved: Vec<i16> = std::iter::repeat_n(0, padding as usize * ch)
            .chain(interleaved)
            .collect();
        let frames = frames + padding as usize;
        let channels: Vec<Vec<i16>> = (0..ch)
            .map(|c| interleaved.iter().skip(c).step_by(ch).copied().collect())
            .collect();
        let params = EncodeParams {
            sample_rate: wav.sample_rate,
            highpass: input.highpass_frequency,
            looping: input
                .loop_frames
                .map(|l| (l.start + padding, l.end + padding)),
            sector_bytes: input.sector_bytes,
            alignment_samples: padding as u16,
        };
        let encoded = adx::encode(&channels, params).map_err(|e| refused("/source", e))?;
        // Encoded readback: the written bytes parsed and decoded again.
        let readback = Adx::parse(&encoded)
            .map_err(|e| ToolResponse::error(format!("encoded readback failed: {e}")))?;
        let (decoded, clamped) =
            adx::decode_stream(&readback, Arithmetic::Fixed12).map_err(ToolResponse::error)?;
        let header = readback.report(Arithmetic::Fixed12);
        let loop_readback = readback.looping.map(|l| (l.start_sample, l.end_sample));
        if loop_readback != params.looping || readback.total_samples as usize != frames {
            return Err(ToolResponse::error("encoded header readback differs"));
        }
        let mut changes = vec![];
        if padding > 0 {
            changes.push(json!({"kind": "lead_in_padding", "frames": padding,
                "loop_frames_written": params.looping.map(|(s, e)| json!({"start": s, "end": e})),
                "note": "silent frames prepended; source frame f is ADX frame f + padding, and the loop alignment field holds the count"}));
        }
        let quality = stats(&interleaved, &decoded, ch);
        if decoded != interleaved {
            changes.push(json!({"kind": "adpcm_quantization", "channels": quality}));
        }
        let smpl_loops: Vec<(u32, u32)> = match &wav.smpl {
            Some(Ok(s)) => s.loops.iter().map(|l| (l.start, l.end + 1)).collect(),
            _ => vec![],
        };
        let carried = |c: &crate::wav::Chunk| match &c.id {
            b"fmt " | b"data" => true,
            b"smpl" => {
                smpl_loops.len() == 1
                    && input.loop_frames.map(|l| (l.start, l.end)) == Some(smpl_loops[0])
            }
            _ => false,
        };
        let dropped: Vec<String> = wav
            .chunks
            .iter()
            .filter(|c| !carried(c))
            .map(|c| String::from_utf8_lossy(&c.id).into_owned())
            .collect();
        if !dropped.is_empty() {
            changes.push(json!({"kind": "wav_metadata_not_carried", "chunks": dropped,
                                "smpl_loops": smpl_loops.iter().map(|(s, e)| json!({"start": s, "end_exclusive": e})).collect::<Vec<_>>()}));
        }
        let kinds: Vec<&str> = changes.iter().filter_map(|c| c["kind"].as_str()).collect();
        let accepted: Vec<&str> = input.accept_changes.iter().map(|c| c.name()).collect();
        let unaccepted: Vec<&str> = kinds
            .iter()
            .copied()
            .filter(|k| !accepted.contains(k))
            .collect();
        if !unaccepted.is_empty() {
            let mut response = ToolResponse::blocked(
                format!(
                    "the encoding reports unaccepted change(s): {}; repeat with a new job_id and accept_changes listing those compatible with the request",
                    unaccepted.join(", ")
                ),
                unaccepted
                    .iter()
                    .map(|k| Diagnostic {
                        code: Code::InvalidRange,
                        location: "/accept_changes".into(),
                        message: format!("{k} is reported and not accepted"),
                    })
                    .collect(),
            );
            response.result = json!({"written": false, "changes": changes});
            return Err(response);
        }
        let mut loop_check = Value::Null;
        if params.looping.is_some() {
            let (_, r, _) =
                adx::render_loop(&readback, Arithmetic::Fixed12, 2).map_err(ToolResponse::error)?;
            loop_check = r["reentry_check"].clone();
        }
        let hash = sha256_hex(&encoded);
        let smpl = params.looping.map(|(s, e)| (s, e - 1));
        let decoded_wav = adx::pcm16_wav(&decoded, ch, wav.sample_rate, smpl);
        let report = json!({
            "source": hash, "from": input.source,
            "parameters": {"version": 3, "encoding_type": 3, "block_size": 18, "sample_bitdepth": 4,
                           "channels": ch, "sample_rate": wav.sample_rate,
                           "highpass_frequency": input.highpass_frequency,
                           "loop_frames": input.loop_frames, "sector_bytes": input.sector_bytes,
                           "loop_start_padding": input.loop_start_padding,
                           "loop_alignment_samples_written": padding},
            "encoder": adx::SEARCH_RULE,
            "header": header,
            "readback": {"parsed": true, "frames": frames, "loop": loop_readback.map(|(s, e)| json!({"start": s, "end_exclusive": e})),
                         "clamped_samples": clamped, "arithmetic": "fixed_12"},
            "changes": changes, "accepted_changes": accepted,
            "loop_reentry": loop_check,
            "files": {"adx": name, "decoded": "decoded.wav", "report": "encode.json"},
        });
        let report_bytes = serde_json::to_vec_pretty(&report).expect("serializes");
        let mut response = self.export_sound_files(
            &input.job_id,
            &input.destination,
            vec![
                (name.into(), encoded.clone()),
                ("decoded.wav".into(), decoded_wav),
                ("encode.json".into(), report_bytes.clone()),
            ],
        )?;
        let store = self.store_mut(true)?;
        store.put_object(&encoded)?;
        let report_hash = store.put_object(&report_bytes)?;
        let record = SoundSourceRecord {
            source: hash.clone(),
            name: name.to_owned(),
            bytes: encoded.len(),
            derived_from: Some(
                json!({"source":input.source,"tool":"encode_adx","job_id":input.job_id,"report_sha256":report_hash}),
            ),
        };
        let mut result = report;
        result["original_unchanged"] = json!(true);
        result["next"] = json!(
            "inspect_adx reads the new source back; render_adx renders it, and compare_audio compares decoded.wav with the source WAV"
        );
        result.as_object_mut().expect("object").remove("header");
        response.summary = format!(
            "encoded {frames} frames x {ch} channel(s) into a {}-byte ADX; changes: {}",
            encoded.len(),
            if kinds.is_empty() {
                "none".to_owned()
            } else {
                kinds.join(", ")
            }
        );
        response.result = result;
        response.artifacts.push(Artifact {
            kind: "derived_adx_source".into(),
            path: store.object_path(&hash).display().to_string(),
            sha256: hash.clone(),
        });
        commit(response, move |writer| {
            writer.put_document("", SOURCES, &hash, &record)
        })
    }
}
