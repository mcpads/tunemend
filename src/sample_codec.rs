//! Bounded mono BRR/PCM sample decoding, encoding and measured comparison.
//!
//! BRR arithmetic follows the S-DSP's documented integer decoder; PlayStation
//! SPU ADPCM and VAG are in [`crate::psx_adpcm`], packed four-bit Mega Drive
//! DAC codes in [`crate::md_dac`]. Sources may
//! also be RIFF/WAVE files ([`crate::sample_wav`]); an explicit target rate
//! resamples through [`crate::sample_resample`]. There is no interpolation,
//! envelope or mixing here; those belong to audition.
use crate::md_dac::{self, NibbleOrder};
use crate::psx_adpcm::{self, Stream, TargetEnd, Vag};
use crate::sample_loop;
use crate::sample_resample;
use crate::sample_wav::{self, Selection};
use crate::wav;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub const ENCODINGS: [&str; 5] = ["brr", "u8", "s8", "s16le", "s16be"];
/// Source-only RIFF/WAVE container encoding.
pub const WAV: &str = "wav";
/// Bound decoded and loop-expanded work independently of container bytes.
pub const MAX_SAMPLES: usize = 8 * 1024 * 1024;
pub const MAX_LOOP_REPEATS: u8 = 16;
/// S-DSP output rate and the 14-bit pitch register limit.
pub const DSP_RATE: u32 = 32000;
pub const MAX_PITCH: u32 = 0x3FFF;

/// How a looping source becomes the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopPolicy {
    /// Bake `loop_repeats` traversals into a one-shot target.
    Finite,
    /// Keep the loop: the target carries its own loop start.
    Preserve,
}

/// RIFF/WAVE interpretation choices. A multi-channel file needs `channel`
/// or `downmix_mean`; `smpl` data is used only when opted in.
#[derive(Debug, Clone, Default)]
pub struct WavOptions {
    pub channel: Option<u16>,
    pub downmix_mean: bool,
    pub loop_start_frame: Option<usize>,
    pub smpl_loop: Option<usize>,
    pub smpl_root: bool,
    pub reinterpret_rate: bool,
}

/// PlayStation SPU ADPCM and VAG choices; each applies only to the side
/// (source or target) it names.
#[derive(Debug, Clone, Default)]
pub struct PsxOptions {
    /// VAG target name field.
    pub name: Option<String>,
    /// One-shot PSX target: loop end without repeat on the last data block
    /// instead of the appended silent loop block.
    pub end_mute: bool,
    /// VAG source: play at `sample_rate` although the header declares another.
    pub reinterpret_rate: bool,
    /// PSX source: decode reserved shifts 13..15 as 9.
    pub reserved_shift_as_9: bool,
}

/// The note a sample sounds when played at its declared rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Root {
    pub note: u8,
    pub cents: f64,
}

pub struct SampleRequest<'a> {
    pub data: &'a [u8],
    pub encoding: &'a str,
    pub target: &'a str,
    pub sample_rate: u32,
    /// Byte offset of the loop start within `data` (raw encodings).
    pub loop_offset: Option<usize>,
    pub loop_repeats: u8,
    pub loop_policy: LoopPolicy,
    /// Required interpretation for `encoding == "wav"`.
    pub wav: Option<WavOptions>,
    /// Resample to this rate; `None` keeps `sample_rate`.
    pub target_sample_rate: Option<u32>,
    pub root: Option<Root>,
    /// Required only for PSX ADPCM/VAG choices.
    pub psx: Option<PsxOptions>,
    /// Declared nibble order; required when the encoding or target is md_dac4.
    pub md_dac: Option<NibbleOrder>,
    /// Level change in dB applied to the decoded PCM16 stream before resampling
    /// and target encoding.
    pub gain_db: Option<f64>,
}

/// Accepted `gain_db` range.
pub const GAIN_DB_RANGE: std::ops::RangeInclusive<f64> = -96.0..=24.0;

/// Scales PCM16 samples by `db`, rounding half away from zero and clamping to
/// 16 bits. `pcm` and `body` (a preserved loop traversal) scale alike; the
/// report counts the clamped samples of each and the measured level change.
fn apply_gain(pcm: &[i32], body: &[i32], db: f64) -> (Vec<i32>, Vec<i32>, Value) {
    let factor = 10f64.powf(db / 20.0);
    let mut clipped = [0usize; 2];
    let mut scale = |values: &[i32], which: usize| -> Vec<i32> {
        values
            .iter()
            .map(|&x| {
                let y = (f64::from(x) * factor).round();
                if !(-32768.0..=32767.0).contains(&y) {
                    clipped[which] += 1;
                }
                y.clamp(-32768.0, 32767.0) as i32
            })
            .collect()
    };
    let (scaled, scaled_body) = (scale(pcm, 0), scale(body, 1));
    let peak = |v: &[i32]| v.iter().map(|x| x.unsigned_abs()).max().unwrap_or(0);
    let energy = |v: &[i32]| v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>();
    let (before, after) = (energy(pcm), energy(&scaled));
    let mut report = json!({
        "gain_db": db,
        "factor": factor,
        "rounding": "x * factor rounded half away from zero, clamped to -32768..32767; applied to the decoded PCM16 stream at the source rate, before any resampling (whose own overshoot is counted in resampled.clipped_samples) and target encoding",
        "clipped_samples": clipped[0],
        "peak_before": peak(pcm),
        "peak_after": peak(&scaled),
        "measured_level_change_db": if before > 0.0 && after > 0.0 {
            json!(10.0 * (after / before).log10())
        } else {
            Value::Null
        },
    });
    if !body.is_empty() {
        report["loop_traversal_clipped_samples"] = json!(clipped[1]);
    }
    (scaled, scaled_body, report)
}

#[derive(Debug)]
pub struct Conversion {
    pub report: Value,
    /// `sample.bin`, `source.wav` and `target.wav`.
    pub files: BTreeMap<String, Vec<u8>>,
}

pub(crate) fn step(n: i32, shift: u8, filter: u8, p1: i32, p2: i32) -> i32 {
    let mut s = if shift <= 12 {
        (n << shift) >> 1
    } else if n < 0 {
        -2048
    } else {
        0
    };
    let (a, b) = (p1 >> 1, p2 >> 1);
    match filter {
        1 => s += a + ((-a) >> 4),
        2 => s += 2 * a + ((-3 * a) >> 5) - b + (b >> 4),
        3 => s += 2 * a + ((-13 * a) >> 6) - b + ((3 * b) >> 4),
        _ => {}
    }
    // The decoder clamps to 16 bits, then doubles with 16-bit wrap.
    (s.clamp(-32768, 32767) * 2) as i16 as i32
}

fn width(encoding: &str) -> usize {
    if encoding.starts_with("s16") { 2 } else { 1 }
}

fn decode_pcm(data: &[u8], encoding: &str) -> Result<Vec<i32>, String> {
    let pcm: Vec<i32> = match encoding {
        "u8" => data.iter().map(|&x| (x as i32 - 128) * 256).collect(),
        "s8" => data.iter().map(|&x| x as i8 as i32 * 256).collect(),
        "s16le" | "s16be" => {
            if !data.len().is_multiple_of(2) {
                return Err("incomplete PCM word".into());
            }
            data.chunks(2)
                .map(|w| {
                    let w = [w[0], w[1]];
                    (if encoding == "s16le" {
                        i16::from_le_bytes(w)
                    } else {
                        i16::from_be_bytes(w)
                    }) as i32
                })
                .collect()
        }
        _ => return Err("unsupported encoding".into()),
    };
    if pcm.len() > MAX_SAMPLES {
        return Err("sample limit exceeded".into());
    }
    Ok(pcm)
}

/// Decoded BRR over `ranges` of whole blocks, carrying predictor history.
fn decode_brr_ranges(data: &[u8], ranges: &[(usize, usize)]) -> Vec<i32> {
    let (mut pcm, mut p1, mut p2) = (Vec::new(), 0, 0);
    for &(start, end) in ranges {
        for block in data[start..end].chunks(9) {
            let header = block[0];
            for &packed in &block[1..] {
                for nibble in [packed >> 4, packed & 15] {
                    let n = if nibble < 8 {
                        nibble as i32
                    } else {
                        nibble as i32 - 16
                    };
                    let value = step(n, header >> 4, (header >> 2) & 3, p1, p2);
                    pcm.push(value);
                    (p2, p1) = (p1, value);
                }
            }
        }
    }
    pcm
}

/// Validates a BRR slice: whole blocks, a single terminating end flag and,
/// when looping, an aligned in-slice loop offset. Returns whether it loops.
fn check_brr(data: &[u8], loop_offset: Option<usize>) -> Result<bool, String> {
    if data.is_empty() || !data.len().is_multiple_of(9) || data.len() / 9 * 16 > MAX_SAMPLES {
        return Err("BRR requires complete bounded nine-byte blocks".into());
    }
    let last = data.len() - 9;
    if (0..last).step_by(9).any(|i| data[i] & 1 != 0) || data[last] & 1 == 0 {
        return Err("BRR end flag must terminate the supplied slice exactly".into());
    }
    let looping = data[last] & 2 != 0;
    if looping && !loop_offset.is_some_and(|o| o < data.len() && o.is_multiple_of(9)) {
        return Err("looping BRR requires an aligned in-slice loop_offset".into());
    }
    Ok(looping)
}

/// Decodes a finite stream: the whole sample, then `repeats` loop traversals.
pub fn decode(
    data: &[u8],
    encoding: &str,
    loop_offset: Option<usize>,
    repeats: u8,
) -> Result<(Vec<i32>, bool), String> {
    if !ENCODINGS.contains(&encoding) {
        return Err("unsupported encoding".into());
    }
    if data.is_empty() {
        return Err("empty sample".into());
    }
    if encoding != "brr" {
        if loop_offset.is_some() || repeats > 0 {
            return Err("PCM loop interpretation needs loop_policy=preserve".into());
        }
        return Ok((decode_pcm(data, encoding)?, false));
    }
    let looping = check_brr(data, loop_offset)?;
    if !looping && (loop_offset.is_some() || repeats > 0) {
        return Err("non-looping BRR cannot repeat".into());
    }
    let mut ranges = vec![(0, data.len())];
    if looping {
        let start = loop_offset.expect("checked");
        ranges.extend(std::iter::repeat_n((start, data.len()), repeats as usize));
    }
    let total: usize = ranges.iter().map(|(s, e)| (e - s) / 9 * 16).sum();
    if total > MAX_SAMPLES {
        return Err("expanded sample limit exceeded".into());
    }
    Ok((decode_brr_ranges(data, &ranges), looping))
}

/// Greedy BRR encoding: per block the lowest squared error over filters and
/// shifts, preferring lower filter then shift. The first block and
/// `restart_block` use filter 0 so decoding does not depend on prior history.
fn encode_brr(pcm: &[i32], restart_block: Option<usize>, looping: bool) -> Vec<u8> {
    let mut padded = pcm.to_vec();
    padded.resize(pcm.len().div_ceil(16) * 16, 0);
    let blocks = padded.len() / 16;
    let (mut output, mut p1, mut p2) = (Vec::with_capacity(blocks * 9), 0, 0);
    for (index, targets) in padded.chunks(16).enumerate() {
        let filters = if index == 0 || Some(index) == restart_block {
            1
        } else {
            4
        };
        let mut best: Option<(i64, u8, u8, [u8; 16], i32, i32)> = None;
        for filter in 0..filters {
            for shift in 0..13u8 {
                let (mut a, mut b, mut error, mut codes) = (p1, p2, 0i64, [0u8; 16]);
                for (slot, &target) in targets.iter().enumerate() {
                    // Exhaustive nibble choice handles clipping and signed wrap;
                    // ties keep the lowest nibble.
                    let (value, n) = (-8..8)
                        .map(|n| (step(n, shift, filter, a, b), n))
                        .min_by_key(|&(value, n)| ((target - value).abs(), n))
                        .expect("sixteen choices");
                    error += ((target - value) as i64).pow(2);
                    codes[slot] = (n & 15) as u8;
                    (b, a) = (a, value);
                }
                if best.as_ref().is_none_or(|x| error < x.0) {
                    best = Some((error, filter, shift, codes, a, b));
                }
            }
        }
        let (_, filter, shift, codes, a, b) = best.expect("at least one candidate");
        (p1, p2) = (a, b);
        let last = index + 1 == blocks;
        output.push(
            (shift << 4)
                | (filter << 2)
                | if last {
                    1 | if looping { 2 } else { 0 }
                } else {
                    0
                },
        );
        output.extend(codes.chunks(2).map(|c| (c[0] << 4) | c[1]));
    }
    output
}

fn encode_pcm(pcm: &[i32], target: &str) -> Vec<u8> {
    match target {
        "u8" | "s8" => pcm
            .iter()
            .map(|&x| {
                let q = (x + 128).div_euclid(256).clamp(-128, 127);
                if target == "u8" {
                    (q + 128) as u8
                } else {
                    q as i8 as u8
                }
            })
            .collect(),
        _ => pcm
            .iter()
            .flat_map(|&x| {
                let x = x as i16;
                if target == "s16le" {
                    x.to_le_bytes()
                } else {
                    x.to_be_bytes()
                }
            })
            .collect(),
    }
}

/// Mono 16-bit WAV of decoded samples at the declared rate.
pub fn mono_wav(pcm: &[i32], rate: u32) -> Vec<u8> {
    let data = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend(b"RIFF");
    out.extend((36 + data).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data.to_le_bytes());
    out.extend(pcm.iter().flat_map(|&x| (x as i16).to_le_bytes()));
    out
}

fn measure(expected: &[i32], actual: &[i32], changes: &mut serde_json::Map<String, Value>) {
    let errors: Vec<i64> = expected
        .iter()
        .zip(actual)
        .map(|(&a, &b)| (b - a) as i64)
        .collect();
    let changed = errors.iter().filter(|&&e| e != 0).count();
    if changed > 0 {
        let max = errors.iter().map(|e| e.abs()).max().unwrap_or(0);
        let rms = (errors.iter().map(|e| (e * e) as f64).sum::<f64>() / errors.len() as f64).sqrt();
        changes.insert(
            "sample_values".into(),
            json!({"changed_samples": changed, "max_absolute_error": max, "rms_error": rms}),
        );
    }
}

/// Decoded source at its declared rate, before resampling and encoding.
struct Source {
    /// Finite: the whole (loop-baked) stream. Preserve: the first pass.
    pcm: Vec<i32>,
    /// Preserve: the steady-state loop traversal, `pcm.len() - loop_start` long.
    body: Vec<i32>,
    loop_start: Option<usize>,
    loop_report: Map<String, Value>,
    changes: Map<String, Value>,
    wav: Option<Value>,
    root: Option<(Root, &'static str)>,
    /// PSX ADPCM/VAG source interpretation, and a VAG header to reuse.
    psx: Option<Value>,
    vag_header: Option<[u8; psx_adpcm::VAG_HEADER_BYTES]>,
    /// md_dac4 source interpretation.
    md: Option<Value>,
}

impl Source {
    fn new(pcm: Vec<i32>, request: &SampleRequest) -> Self {
        Source {
            pcm,
            body: Vec::new(),
            loop_start: None,
            loop_report: Map::new(),
            changes: Map::new(),
            wav: None,
            root: request.root.map(|r| (r, "explicit")),
            psx: None,
            vag_header: None,
            md: None,
        }
    }
}

pub fn convert(request: &SampleRequest) -> Result<Conversion, String> {
    let rate = request.sample_rate;
    if !(1..=192000).contains(&rate) {
        return Err("sample_rate must be 1..192000 Hz; it is declared, never inferred".into());
    }
    if request
        .target_sample_rate
        .is_some_and(|r| !(1..=192000).contains(&r))
    {
        return Err("target_sample_rate must be 1..192000 Hz".into());
    }
    if let Some(db) = request.gain_db
        && (!db.is_finite() || !GAIN_DB_RANGE.contains(&db) || db == 0.0)
    {
        return Err(format!(
            "gain_db must be a non-zero level change within {}..{} dB; omit it to keep the level",
            GAIN_DB_RANGE.start(),
            GAIN_DB_RANGE.end()
        ));
    }
    if request.loop_repeats > MAX_LOOP_REPEATS {
        return Err("loop_repeats must be 0..16 additional traversals".into());
    }
    let target = request.target;
    let psx_source = psx_adpcm::ENCODINGS.contains(&request.encoding);
    let psx_target = psx_adpcm::ENCODINGS.contains(&target);
    let md_source = request.encoding == md_dac::ENCODING;
    let md_target = target == md_dac::ENCODING;
    if !ENCODINGS.contains(&target) && !psx_target && !md_target {
        return Err("unsupported target".into());
    }
    if let Some(options) = &request.psx {
        check_psx_options(options, request, psx_source, psx_target)?;
    }
    let md_order = match (request.md_dac, md_source || md_target) {
        (Some(order), true) => Some(order),
        (None, true) => {
            return Err("md_dac4 needs md_dac.nibble_order (high_first or low_first): the nibble that plays first is a driver choice, declared, never inferred".into());
        }
        (Some(_), false) => {
            return Err("md_dac options need an md_dac4 encoding or target".into());
        }
        (None, false) => None,
    };
    if let Some(root) = request.root
        && (root.note > 127 || !(-100.0..=100.0).contains(&root.cents))
    {
        return Err("root needs note 0..127 and cents -100..100".into());
    }
    let source = if request.encoding == WAV {
        wav_source(request)?
    } else if request.wav.is_some() {
        return Err("wav options need encoding=wav".into());
    } else if psx_source {
        psx_source_stream(request)?
    } else if let (true, Some(order)) = (md_source, md_order) {
        md_source_stream(request, order)?
    } else if request.loop_policy == LoopPolicy::Finite {
        raw_finite(request)?
    } else {
        raw_preserved(request)?
    };
    let out_rate = request.target_sample_rate.unwrap_or(rate);
    if source.loop_start.is_some() {
        let block = if target == "brr" {
            16
        } else if psx_target {
            psx_adpcm::BLOCK_SAMPLES
        } else if md_target {
            2
        } else {
            1
        };
        check_loop_fit(block, request, &source, out_rate)?;
    }
    let mut changes = source.changes.clone();
    // Gain comes first, at the source rate, so an attenuated stream does not
    // clip on resampler overshoot; the previews keep the unscaled source.
    let (gained_pcm, gained_body) = match request.gain_db {
        None => (source.pcm.clone(), source.body.clone()),
        Some(db) => {
            let (pcm, body, report) = apply_gain(&source.pcm, &source.body, db);
            changes.insert("gain".into(), report);
            (pcm, body)
        }
    };
    let (pcm, body, loop_start) = if out_rate == rate {
        (gained_pcm, gained_body, source.loop_start)
    } else {
        let out = match source.loop_start {
            None => sample_resample::finite(&gained_pcm, rate, out_rate)?,
            Some(start) => {
                sample_resample::looped(&gained_pcm, &gained_body, start, rate, out_rate)?
            }
        };
        changes.insert("resampled".into(), out.report);
        let body = out
            .loop_start
            .map(|s| out.pcm[s..].to_vec())
            .unwrap_or_default();
        (out.pcm, body, out.loop_start)
    };
    let psx_choice = request.psx.clone().unwrap_or_default();
    let mut psx_report = None;
    let mut md_report = None;
    let (encoded, actual, loop_report) = match loop_start {
        None => {
            let (encoded, actual) = if psx_target {
                let end = if psx_choice.end_mute {
                    TargetEnd::EndMute
                } else {
                    TargetEnd::SilentLoopBlock
                };
                let encoded_frames = pcm.len().div_ceil(psx_adpcm::BLOCK_SAMPLES)
                    * psx_adpcm::BLOCK_SAMPLES
                    + if psx_choice.end_mute {
                        0
                    } else {
                        psx_adpcm::BLOCK_SAMPLES
                    };
                if encoded_frames > MAX_SAMPLES {
                    return Err(format!(
                        "PSX block padding and termination need {encoded_frames} decoded samples, above {MAX_SAMPLES}; shorten the input or select end_mute if its padded size fits"
                    ));
                }
                let target_psx = encode_psx(&pcm, end, target, &source, out_rate, &psx_choice)?;
                psx_report = Some(target_psx.report);
                (target_psx.bytes, target_psx.stream.first_pass())
            } else if let (true, Some(order)) = (md_target, md_order) {
                let (bytes, report) = md_dac::encode(&pcm, order);
                md_report = Some(report);
                let actual = md_dac::decode(&bytes, order, usize::MAX)?;
                (bytes, actual)
            } else {
                let encoded = if target == "brr" {
                    encode_brr(&pcm, None, false)
                } else {
                    encode_pcm(&pcm, target)
                };
                let (actual, _) = decode(&encoded, target, None, 0)?;
                (encoded, actual)
            };
            measure(&pcm, &actual, &mut changes);
            if actual.len() != pcm.len() {
                changes.insert(
                    "padding".into(),
                    json!({"added_samples": actual.len() - pcm.len()}),
                );
            }
            (encoded, actual, None)
        }
        Some(start) => {
            let (encoded, actual, target_offset) = if psx_target {
                // check_loop_fit refused unaligned loops before resampling.
                debug_assert!(
                    start.is_multiple_of(psx_adpcm::BLOCK_SAMPLES)
                        && pcm.len().is_multiple_of(psx_adpcm::BLOCK_SAMPLES)
                );
                let block = start / psx_adpcm::BLOCK_SAMPLES;
                let target_psx = encode_psx(
                    &pcm,
                    TargetEnd::Loop(block),
                    target,
                    &source,
                    out_rate,
                    &psx_choice,
                )?;
                psx_report = Some(target_psx.report);
                let offset = target_psx.header_bytes + block * psx_adpcm::BLOCK_BYTES;
                // Decode the written loop explicitly: a one-block silent loop
                // reads back as the one-shot terminator, but it is still the
                // loop this target declares.
                let blocks = target_psx.stream.end_block + 1;
                let actual = target_psx.stream.decode(&[(0, blocks), (block, blocks)]);
                (target_psx.bytes, actual, offset)
            } else if let (true, Some(order)) = (md_target, md_order) {
                // check_loop_fit refused loops off whole bytes.
                debug_assert!(start.is_multiple_of(2) && pcm.len().is_multiple_of(2));
                let (bytes, report) = md_dac::encode(&pcm, order);
                md_report = Some(report);
                let decoded = md_dac::decode(&bytes, order, usize::MAX)?;
                let actual = [&decoded[..], &decoded[start..]].concat();
                (bytes, actual, start / 2)
            } else {
                encode_preserved(&pcm, start, target)?
            };
            let reference = [&pcm[..], &body[..]].concat();
            measure(&reference, &actual, &mut changes);
            if let Some(Value::Object(values)) = changes.get_mut("sample_values") {
                locate(&reference, &actual, pcm.len(), values);
            }
            let mut report = source.loop_report.clone();
            report.extend([
                ("policy".into(), json!("preserve")),
                (
                    "loop_start_sample".into(),
                    json!(source.loop_start.expect("preserved")),
                ),
                ("target_loop_offset".into(), json!(target_offset)),
                (
                    "compared".into(),
                    json!("first pass plus one loop traversal"),
                ),
                (
                    "history_carried_across_loop".into(),
                    json!(request.encoding == "brr" || target == "brr" || psx_source || psx_target),
                ),
                (
                    "target_restarts_with_filter_0".into(),
                    json!(target == "brr" || psx_target),
                ),
                (
                    "previews".into(),
                    json!("source.wav and target.wav hold the compared stream; their smpl loop repeats the loop traversal after the first pass"),
                ),
                ("loop_in_sample_bin".into(), json!(if target == "brr" {
                    "BRR end and loop flags; the loop address is target_loop_offset"
                } else if psx_target {
                    "SPU ADPCM flags: loop start (bit 2) on the block at target_loop_offset, loop end and repeat (bits 0-1) on the last block"
                } else if md_target {
                    "md_dac4 carries no loop; target_loop_offset (a byte) is in report.json only and the driver must be given it"
                } else {
                    "raw PCM carries no loop; target_loop_offset is in report.json only"
                })),
            ]);
            if out_rate != rate {
                report.insert("target_loop_start_sample".into(), json!(start));
            }
            let previews = (
                looped_wav(
                    &[&source.pcm[..], &source.body[..]].concat(),
                    rate,
                    source.pcm.len(),
                    source.root,
                ),
                looped_wav(&actual, out_rate, pcm.len(), source.root),
            );
            (
                encoded,
                actual[..pcm.len()].to_vec(),
                Some((report, previews)),
            )
        }
    };
    let (loop_report, previews) = match loop_report {
        Some((report, previews)) => (Some(report), Some(previews)),
        None => (None, None),
    };
    let changed = changes.contains_key("sample_values");
    let policy = if loop_report.is_some() {
        "loop preserved"
    } else {
        "finite stream"
    };
    let mut report = json!({"encoding": request.encoding, "target": target,
        "sample_rate": rate, "source_samples": source.pcm.len(), "target_samples": actual.len(),
        "sample_values_exact": !changed, "decoded_stream_exact": changes.is_empty(),
        "changes": changes, "scope": scope(policy, source.wav.is_some(), out_rate != rate, psx_source || psx_target, psx_target, md_source || md_target)});
    if let Some(loop_report) = loop_report {
        report["loop"] = Value::Object(loop_report);
    }
    if request.gain_db.is_some() {
        let text = format!(
            "{} The requested gain scales the stream before encoding (changes.gain); sample_values compare the target with the scaled stream.",
            report["scope"].as_str().unwrap_or_default()
        );
        report["scope"] = json!(text);
    }
    if out_rate != rate {
        report["target_sample_rate"] = json!(out_rate);
    }
    if let Some(wav) = source.wav {
        report["wav"] = wav;
    }
    if let Some(psx) = source.psx {
        report["psx_adpcm"] = psx;
    }
    if let Some(psx) = psx_report {
        report["psx_adpcm_target"] = psx;
    }
    if let Some(md) = source.md {
        report["md_dac"] = md;
    }
    if let Some(md) = md_report {
        report["md_dac_target"] = md;
    }
    if let Some(tuning) = tuning(out_rate, target, source.root) {
        report["tuning"] = tuning;
    }
    let (source_wav, target_wav) =
        previews.unwrap_or_else(|| (mono_wav(&source.pcm, rate), mono_wav(&actual, out_rate)));
    let files = BTreeMap::from([
        ("sample.bin".to_owned(), encoded),
        ("source.wav".to_owned(), source_wav),
        ("target.wav".to_owned(), target_wav),
    ]);
    Ok(Conversion { report, files })
}

/// Refuses a preserved loop whose start or length does not scale to whole
/// samples at `out_rate`, or to whole `block`-sample blocks (16 for BRR, 28
/// for PSX ADPCM, 1 for PCM), with concrete recoveries.
fn check_loop_fit(
    block: usize,
    request: &SampleRequest,
    source: &Source,
    out_rate: u32,
) -> Result<(), String> {
    let Some(start) = source.loop_start else {
        return Ok(());
    };
    let (rate, t) = (u64::from(request.sample_rate), u64::from(out_rate));
    let (s, l) = (start as u64, source.pcm.len() as u64);
    let scales = (s * t).is_multiple_of(rate) && (l * t).is_multiple_of(rate);
    let (target_start, target_length) = (s * t / rate, l * t / rate);
    let block_u = block as u64;
    if scales && target_start.is_multiple_of(block_u) && target_length.is_multiple_of(block_u) {
        return Ok(());
    }
    let at = if t != rate {
        format!(" at {out_rate} Hz after resampling")
    } else {
        String::new()
    };
    let problem = if !scales {
        format!(
            "a preserved loop must scale to whole samples: loop start {s} and length {l} at {rate} Hz are not whole at {out_rate} Hz"
        )
    } else if block == 16 {
        format!(
            "a BRR loop needs a 16-sample-aligned start and length; the source loops at sample {target_start} of {target_length}{at}"
        )
    } else if block == 2 {
        format!(
            "an md_dac4 loop needs a start and length on whole bytes (2 samples); the source loops at sample {target_start} of {target_length}{at}"
        )
    } else {
        format!(
            "a PSX ADPCM loop needs a 28-sample-aligned start and length; the source loops at sample {target_start} of {target_length}{at}"
        )
    };
    Err(format!(
        "{problem}. {}",
        loop_fit_recovery(block_u, request, source, out_rate)
    ))
}

/// Concrete ways out of a loop [`check_loop_fit`] refuses, in source units:
/// the nearest loop starts and lengths that fit at `out_rate`, the fitting
/// starts at which the loop also spans a whole number of periods of its
/// fundamental, the target rates at which the source loop fits as it is,
/// and the one-shot alternative.
fn loop_fit_recovery(
    block: u64,
    request: &SampleRequest,
    source: &Source,
    out_rate: u32,
) -> String {
    let rate = request.sample_rate;
    let (s, l) = (
        source.loop_start.unwrap_or(0) as u64,
        source.pcm.len() as u64,
    );
    // Source positions that land on whole target blocks are multiples of m.
    let m = sample_loop::source_step(block, rate, out_rate);
    let around = |value: u64| {
        let below = value / m * m;
        if below == value {
            vec![value]
        } else {
            vec![below, below + m]
        }
    };
    let list = |values: &[u64]| {
        values
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(" or ")
    };
    let wav = request.encoding == WAV;
    let unit = if wav { "frame" } else { "sample" };
    let start_hint = |starts: &[u64]| {
        if wav {
            format!(" (wav.loop_start_frame {})", list(starts))
        } else if ENCODINGS.contains(&request.encoding) && request.encoding != "brr" {
            let bytes: Vec<u64> = starts
                .iter()
                .map(|s| s * width(request.encoding) as u64)
                .collect();
            format!(" (loop_offset {} bytes)", list(&bytes))
        } else if request.encoding == md_dac::ENCODING && starts.iter().all(|s| s % 2 == 0) {
            let bytes: Vec<u64> = starts.iter().map(|s| s / 2).collect();
            format!(" (loop_offset {} bytes)", list(&bytes))
        } else {
            " (a slice whose loop starts there)".to_owned()
        }
    };
    let blocks = if block == 1 {
        "whole samples".to_owned()
    } else {
        format!("whole {block}-sample blocks")
    };
    let resampled = out_rate != rate;
    let mut steps = Vec::new();
    let starts: Vec<u64> = around(s).into_iter().filter(|&x| x < l).collect();
    if !s.is_multiple_of(m) && !starts.is_empty() {
        steps.push(format!(
            "start the loop at {unit} {}{}{}",
            list(&starts),
            start_hint(&starts),
            if resampled {
                format!(" (multiples of {m} {unit}s land on {blocks} at {out_rate} Hz)")
            } else {
                String::new()
            }
        ));
    }
    let lengths: Vec<u64> = around(l).into_iter().filter(|&x| x > 0).collect();
    if !l.is_multiple_of(m) {
        let whose = if wav {
            "the loop runs to the last frame, so use a WAV"
        } else {
            "the loop runs to the end, so use a slice"
        };
        steps.push(format!(
            "end at a total length of {} {unit}s ({whose} of that length)",
            list(&lengths)
        ));
    }
    if let Some(found) = sample_loop::sustain(&source.body, rate) {
        let period = f64::from(rate) / found.hz;
        let periods = (l - s) as f64 / period;
        let fitting_length = l / m * m;
        let whole = sample_loop::whole_period_starts(fitting_length, s, m, period, 2);
        let length_note = if fitting_length == l {
            String::new()
        } else {
            format!(" with a total length of {fitting_length} {unit}s")
        };
        let spans = format!(
            "the loop spans {periods:.3} periods of its {:.2} Hz fundamental at any rate{}",
            found.hz,
            if (periods - periods.round()).abs() > sample_loop::PHASE_TOLERANCE {
                ", so its seam jumps in phase"
            } else {
                " (nearly whole)"
            }
        );
        if whole.is_empty() {
            steps.push(format!(
                "{spans}, and no fitting start{length_note} makes it a whole number"
            ));
        } else {
            let starts: Vec<u64> = whole.iter().map(|(x, _)| *x).collect();
            steps.push(format!(
                "{spans}; starting at {unit} {}{}{length_note} fits and spans {} periods (prepare it and read report_guide.loop_diagnostics for the waveform there)",
                list(&starts),
                start_hint(&starts),
                whole
                    .iter()
                    .map(|(_, p)| format!("{p:.3}"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ));
        }
        let rates = sample_loop::whole_period_rates(
            (l, s),
            block,
            (rate, out_rate),
            period,
            (3, MAX_SAMPLES as u64),
        );
        if !rates.is_empty() {
            steps.push(format!(
                "for a whole-period loop at the full length, target_sample_rate {}",
                rates
                    .iter()
                    .map(|(t, start, p)| format!(
                        "{t} with {} {start} ({p:.3} periods)",
                        if wav {
                            "wav.loop_start_frame".to_owned()
                        } else {
                            format!("loop start {unit}")
                        }
                    ))
                    .collect::<Vec<_>>()
                    .join(", or ")
            ));
        }
    }
    // A target rate t keeps the loop whole and aligned exactly when the
    // source loop start S and length L scale to multiples of `block`:
    // t = block * rate * j / gcd(S, L), so t is a multiple of `step`.
    let common = sample_loop::gcd(s, l).max(1);
    let scaled = block * u64::from(rate);
    let step = scaled / sample_loop::gcd(common, scaled);
    let fits = |t: u64| {
        let new_length = l * t / u64::from(rate);
        let new_start = s * t / u64::from(rate);
        (1..=192_000).contains(&t)
            && t != u64::from(out_rate)
            && (new_length + (new_length - new_start)) as usize <= MAX_SAMPLES
    };
    let anchor = u64::from(out_rate);
    let below = anchor / step * step;
    let rates: Vec<u64> = [below, below + step]
        .into_iter()
        .chain((1..=4).map(|k| below + step * (k + 1)))
        .filter(|&t| t > 0 && fits(t))
        .take(2)
        .collect();
    if rates.is_empty() {
        steps.push(format!(
            "no target_sample_rate up to 192000 Hz scales this source loop ({s} of {l} samples at {rate} Hz) to {blocks}"
        ));
    } else {
        steps.push(format!(
            "set target_sample_rate {} (multiples of {step} Hz scale the source loop, {s} of {l} samples at {rate} Hz, to {blocks})",
            list(&rates)
        ));
    }
    steps.push(if wav {
        "or drop wav.loop_start_frame/wav.smpl_loop and use loop_policy=finite for a one-shot without a loop (WAV loops are not baked)".to_owned()
    } else {
        "or loop_policy=finite to bake loop_repeats traversals into a one-shot".to_owned()
    });
    format!("Recoveries: {}", steps.join("; "))
}

/// Where a preserved loop's compared stream differs: the first index and the
/// counts in the first pass (`split` samples) and in the loop traversal.
fn locate(expected: &[i32], actual: &[i32], split: usize, values: &mut Map<String, Value>) {
    let differs: Vec<usize> = (0..expected.len().min(actual.len()))
        .filter(|&i| expected[i] != actual[i])
        .collect();
    let first_pass = differs.iter().filter(|&&i| i < split).count();
    values.insert("first_changed_sample".into(), json!(differs.first()));
    values.insert("first_pass_changed_samples".into(), json!(first_pass));
    values.insert(
        "loop_traversal_changed_samples".into(),
        json!(differs.len() - first_pass),
    );
    values.insert("first_pass_samples".into(), json!(split));
}

/// A mono PCM16 preview whose `smpl` loop repeats `[loop_start, end)` forever:
/// the steady loop traversal after the first pass. The root, when known, is
/// the unity note; otherwise 60 is a placeholder.
fn looped_wav(pcm: &[i32], rate: u32, loop_start: usize, root: Option<(Root, &str)>) -> Vec<u8> {
    let mut out = mono_wav(pcm, rate);
    let (unity, fraction) = match root {
        Some((r, _)) => {
            let pitch = (r.note as f64 + r.cents / 100.0).clamp(0.0, 127.0);
            let unity = pitch.floor();
            (unity as u32, ((pitch - unity) * 4294967296.0) as u32)
        }
        None => (60, 0),
    };
    let period = (1e9 / rate as f64).round() as u32;
    let mut smpl = Vec::with_capacity(60);
    for v in [0, 0, period, unity, fraction, 0, 0, 1, 0, 0, 0] {
        smpl.extend(u32::to_le_bytes(v));
    }
    for v in [loop_start as u32, pcm.len() as u32 - 1, 0, 0] {
        smpl.extend(v.to_le_bytes());
    }
    out.extend(b"smpl");
    out.extend((smpl.len() as u32).to_le_bytes());
    out.extend(smpl);
    let riff = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff.to_le_bytes());
    out
}

fn scope(
    policy: &str,
    wav: bool,
    resampled: bool,
    psx: bool,
    psx_target: bool,
    md: bool,
) -> String {
    let history = if psx {
        "initial decoder history zero (BRR or PSX SPU ADPCM)"
    } else {
        "initial BRR history zero"
    };
    let mut scope = format!("Mono decoded samples with {history} ({policy}).");
    if psx {
        scope += " PSX SPU ADPCM follows the documented decoding equation; other decoders may round differently.";
    }
    if md {
        scope += " md_dac4 nibbles are linear upper-four-bit DAC values in the declared nibble order (an observed reading of one driver family); the format holds no rate or loop.";
    }
    if wav {
        scope += " WAV frames are reduced to one PCM16 channel as reported.";
    }
    if resampled {
        scope += " Resampled by a Kaiser-windowed sinc filter; sample_values compare the target with the resampled stream.";
    }
    scope += " Tuning values are computed from the declared rate, not observed playback. No ";
    if !resampled {
        scope += "resampling, ";
    }
    scope
        + "interpolation, envelope, mixing, DAC timing, native insertion or playback equivalence. "
        + if psx_target {
            "Target PSX ADPCM uses an exhaustive per-block filter/shift search with nearest nibbles, not optimal compression."
        } else {
            "Target BRR uses greedy block search, not optimal compression."
        }
}

/// Encodes a looping stream and decodes the first pass plus one traversal.
fn encode_preserved(
    pcm: &[i32],
    start: usize,
    target: &str,
) -> Result<(Vec<u8>, Vec<i32>, usize), String> {
    if target == "brr" {
        // check_loop_fit refused unaligned loops before resampling.
        debug_assert!(start.is_multiple_of(16) && pcm.len().is_multiple_of(16));
        let encoded = encode_brr(pcm, Some(start / 16), true);
        let offset = start / 16 * 9;
        let actual = decode_brr_ranges(&encoded, &[(0, encoded.len()), (offset, encoded.len())]);
        Ok((encoded, actual, offset))
    } else {
        let encoded = encode_pcm(pcm, target);
        let decoded = decode_pcm(&encoded, target)?;
        let actual = [&decoded[..], &decoded[start..]].concat();
        Ok((encoded, actual, start * width(target)))
    }
}

fn raw_finite(request: &SampleRequest) -> Result<Source, String> {
    let (pcm, looping) = decode(
        request.data,
        request.encoding,
        request.loop_offset,
        request.loop_repeats,
    )?;
    let mut source = Source::new(pcm, request);
    if looping {
        source.changes.insert(
            "finite_loop".into(),
            json!({"additional_traversals": request.loop_repeats, "history_carried_across_loop": true, "target_loop": false}),
        );
    }
    Ok(source)
}

/// First pass and steady loop traversal of a raw source, so loop re-entry
/// history is compared too.
fn raw_preserved(request: &SampleRequest) -> Result<Source, String> {
    if request.loop_repeats > 0 {
        return Err("loop_policy=preserve keeps the loop; loop_repeats must be 0".into());
    }
    let (data, encoding) = (request.data, request.encoding);
    let offset = request
        .loop_offset
        .ok_or("loop_policy=preserve needs a looping source: loop_offset for a raw slice; a bank slot uses its stored loop")?;
    if !ENCODINGS.contains(&encoding) || data.is_empty() {
        return Err("unsupported or empty source".into());
    }
    let (pcm, reference, loop_start) = if encoding == "brr" {
        if !check_brr(data, Some(offset))? {
            return Err("the source BRR end block has no loop flag".into());
        }
        let pcm = decode_brr_ranges(data, &[(0, data.len())]);
        let reference = decode_brr_ranges(data, &[(0, data.len()), (offset, data.len())]);
        (pcm, reference, offset / 9 * 16)
    } else {
        let w = width(encoding);
        if !offset.is_multiple_of(w) || offset >= data.len() {
            return Err("PCM loop_offset must be an in-slice sample boundary".into());
        }
        let pcm = decode_pcm(data, encoding)?;
        let start = offset / w;
        let reference = [&pcm[..], &pcm[start..]].concat();
        (pcm, reference, start)
    };
    if reference.len() > MAX_SAMPLES {
        return Err("expanded sample limit exceeded".into());
    }
    let body = reference[pcm.len()..].to_vec();
    let mut source = Source::new(pcm, request);
    source.body = body;
    source.loop_start = Some(loop_start);
    source
        .loop_report
        .insert("source_loop_offset".into(), json!(offset));
    Ok(source)
}

/// An md_dac4 slice: linear nibbles in the declared order. The format holds
/// no loop, so a preserved loop needs `loop_offset` (a slice byte).
fn md_source_stream(request: &SampleRequest, order: NibbleOrder) -> Result<Source, String> {
    let pcm = md_dac::decode(request.data, order, MAX_SAMPLES)?;
    let mut source = Source::new(Vec::new(), request);
    match request.loop_policy {
        LoopPolicy::Finite => {
            if request.loop_offset.is_some() || request.loop_repeats > 0 {
                return Err("md_dac4 holds no loop; a declared loop_offset needs loop_policy=preserve, and loop_repeats cannot bake one".into());
            }
            source.pcm = pcm;
        }
        LoopPolicy::Preserve => {
            if request.loop_repeats > 0 {
                return Err("loop_policy=preserve keeps the loop; loop_repeats must be 0".into());
            }
            let offset = request.loop_offset.ok_or(
                "loop_policy=preserve needs loop_offset for md_dac4: the format holds no loop, so the loop start byte is declared",
            )?;
            if offset >= request.data.len() {
                return Err(format!(
                    "loop_offset {offset} is outside the md_dac4 slice of {} bytes",
                    request.data.len()
                ));
            }
            let start = offset * 2;
            if pcm.len() * 2 - start > MAX_SAMPLES {
                return Err("expanded sample limit exceeded".into());
            }
            source.body = pcm[start..].to_vec();
            source.loop_start = Some(start);
            source
                .loop_report
                .insert("source_loop_offset".into(), json!(offset));
            source.pcm = pcm;
        }
    }
    source.md = Some(md_dac::report(request.data, order));
    Ok(source)
}

fn wav_source(request: &SampleRequest) -> Result<Source, String> {
    let options = request.wav.clone().unwrap_or_default();
    let file = wav::parse(request.data).map_err(|e| e.to_string())?;
    if file.frames > MAX_SAMPLES {
        return Err(format!(
            "the WAV holds {} frames; at most {MAX_SAMPLES} are supported",
            file.frames
        ));
    }
    if request.loop_offset.is_some() {
        return Err("WAV loops are chosen in frames with wav.loop_start_frame or wav.smpl_loop; loop_offset is for raw slices".into());
    }
    if request.loop_repeats > 0 {
        return Err("WAV loop baking is not supported; loop_repeats must be 0".into());
    }
    let selection = match (options.channel, options.downmix_mean) {
        (Some(_), true) => return Err("choose wav.channel or wav.downmix, not both".into()),
        (Some(c), false) => Selection::Channel(c),
        (None, true) => Selection::Mean,
        (None, false) if file.channels == 1 => Selection::Channel(0),
        (None, false) => {
            return Err(format!(
                "the WAV has {} channels; choose wav.channel (0-based) or wav.downmix=\"mean\"",
                file.channels
            ));
        }
    };
    let mut changes = Map::new();
    if file.sample_rate != request.sample_rate {
        if !options.reinterpret_rate {
            return Err(format!(
                "the WAV header declares {} Hz but sample_rate is {}. Pass sample_rate={} (add target_sample_rate to resample), or set wav.reinterpret_rate=true to play these frames at {} Hz, which changes pitch and duration",
                file.sample_rate, request.sample_rate, file.sample_rate, request.sample_rate
            ));
        }
        changes.insert(
            "rate_reinterpreted".into(),
            json!({"header_rate": file.sample_rate, "declared_rate": request.sample_rate,
                "pitch_change_cents": 1200.0 * (request.sample_rate as f64 / file.sample_rate as f64).log2()}),
        );
    }
    let interpreted = sample_wav::interpret(&file, selection)?;
    changes.extend(interpreted.changes);
    let smpl = || match &file.smpl {
        Some(Ok(s)) => Ok(s),
        Some(Err(e)) => Err(format!("the smpl chunk is unusable: {e}")),
        None => Err("the WAV has no smpl chunk".to_string()),
    };
    let frames = file.frames;
    let (loop_start, loop_source) = match (options.loop_start_frame, options.smpl_loop) {
        (Some(_), Some(_)) => {
            return Err("choose wav.loop_start_frame or wav.smpl_loop, not both".into());
        }
        (Some(frame), None) => (Some(frame), "explicit".to_string()),
        (None, Some(index)) => {
            let s = smpl()?;
            let l = s.loops.get(index).ok_or_else(|| {
                format!(
                    "smpl loop {index} does not exist; the chunk has {}",
                    s.loops.len()
                )
            })?;
            if let Some(problem) = smpl_loop_problem(l, frames) {
                return Err(format!("smpl loop {index} {problem}"));
            }
            (Some(l.start as usize), format!("smpl loop {index}"))
        }
        (None, None) => (None, String::new()),
    };
    match (request.loop_policy, loop_start) {
        (LoopPolicy::Finite, Some(_)) => {
            return Err("a WAV loop needs loop_policy=preserve".into());
        }
        (LoopPolicy::Preserve, None) => {
            return Err(
                "loop_policy=preserve needs a WAV loop: wav.loop_start_frame or wav.smpl_loop"
                    .into(),
            );
        }
        (_, Some(start)) if start >= frames => {
            return Err(format!(
                "loop start frame {start} is outside the WAV's {frames} frames"
            ));
        }
        _ => {}
    }
    let root = if options.smpl_root {
        if request.root.is_some() {
            return Err("root and wav.smpl_root are exclusive".into());
        }
        let s = smpl()?;
        let note = u8::try_from(s.unity_note)
            .ok()
            .filter(|&n| n <= 127)
            .ok_or_else(|| format!("smpl unity note {} is not a MIDI note", s.unity_note))?;
        Some((
            Root {
                note,
                cents: s.pitch_fraction as f64 * 100.0 / 4294967296.0,
            },
            "smpl",
        ))
    } else {
        request.root.map(|r| (r, "explicit"))
    };
    let pcm = interpreted.pcm;
    let body = loop_start.map(|s| pcm[s..].to_vec()).unwrap_or_default();
    if pcm.len() + body.len() > MAX_SAMPLES {
        return Err("expanded sample limit exceeded".into());
    }
    let mut wav = file.report();
    wav["selection"] = match selection {
        Selection::Channel(c) => json!({"channel": c}),
        Selection::Mean => json!({"downmix": "mean"}),
    };
    wav["smpl_used"] = json!({"loop": options.smpl_loop, "root": options.smpl_root});
    let mut loop_report = Map::new();
    if let Some(start) = loop_start {
        loop_report.insert("source_loop_frame".into(), json!(start));
        loop_report.insert("loop_source".into(), json!(loop_source));
    }
    Ok(Source {
        pcm,
        body,
        loop_start,
        loop_report,
        changes,
        wav: Some(wav),
        root,
        psx: None,
        vag_header: None,
        md: None,
    })
}

/// Why `wav.smpl_loop` cannot use a `smpl` loop of a WAV with `frames`
/// frames: a preserved loop is forward, whole-sample, infinite and runs to
/// the last frame.
pub fn smpl_loop_problem(l: &wav::SmplLoop, frames: usize) -> Option<String> {
    if l.kind != 0 {
        Some(format!(
            "has type {}; only forward loops (type 0) are supported",
            l.kind
        ))
    } else if l.fraction != 0 {
        Some("has a fractional loop point; the target loop starts on a whole sample".into())
    } else if l.play_count != 0 {
        Some(format!(
            "plays {} times; a preserved target loop repeats indefinitely",
            l.play_count
        ))
    } else if l.start > l.end {
        Some("starts after it ends".into())
    } else if l.end as usize == frames {
        Some(format!(
            "ends at frame {}, one past the last frame {}; smpl end frames are inclusive",
            l.end,
            frames.saturating_sub(1)
        ))
    } else if l.end as usize + 1 != frames {
        Some(format!(
            "ends at frame {}, but the target loop runs to the last frame {}; use a WAV that ends at the loop end",
            l.end,
            frames.saturating_sub(1)
        ))
    } else {
        None
    }
}

/// The facts prepare_sample reads from a RIFF/WAVE file: format, channels,
/// rate, bit depth, frames and duration, and each `smpl` loop and the root
/// with whether `wav.smpl_loop` and `wav.smpl_root` can use them.
pub fn wav_facts(bytes: &[u8]) -> Value {
    let file = match wav::parse(bytes) {
        Ok(file) => file,
        Err(e) => return json!({"unusable": e.to_string()}),
    };
    let mut facts = json!({
        "format": if file.format == wav::Format::Int { "pcm_integer" } else { "ieee_float" },
        "extensible": file.extensible,
        "channels": file.channels,
        "sample_rate": file.sample_rate,
        "bits_per_sample": file.bits,
        "frames": file.frames,
        "duration_seconds": file.frames as f64 / f64::from(file.sample_rate.max(1)),
        "sample_rate_note": "declared by the header; pass it as prepare_sample sample_rate (target_sample_rate resamples)",
        "channel_choice": if file.channels == 1 { "mono: no wav choice needed" } else { "several channels: prepare_sample needs wav.channel (0-based) or wav.downmix \"mean\"" },
    });
    if file.frames > MAX_SAMPLES {
        facts["prepare_sample_limit"] = json!(format!(
            "prepare_sample takes at most {MAX_SAMPLES} frames; this file is longer"
        ));
    }
    facts["smpl"] = match &file.smpl {
        None => json!(null),
        Some(Err(e)) => json!({"unusable": e.to_string()}),
        Some(Ok(s)) => {
            let root = u8::try_from(s.unity_note).ok().filter(|&n| n <= 127);
            json!({
                "unity_note": s.unity_note,
                "pitch_fraction_cents": s.pitch_fraction as f64 * 100.0 / 4294967296.0,
                "smpl_root": match root {
                    Some(_) => json!("usable"),
                    None => json!(format!("unusable: unity note {} is not a MIDI note", s.unity_note)),
                },
                "loops": s.loops.iter().enumerate().map(|(index, l)| {
                    let mut entry = json!({"index": index, "type": l.kind,
                        "start_frame": l.start, "end_frame_inclusive": l.end,
                        "length_frames": (l.end as u64 + 1).saturating_sub(l.start as u64),
                        "fraction": l.fraction, "play_count": l.play_count,
                        "aligned_without_resampling": {
                            "brr_16_samples": l.start % 16 == 0 && file.frames % 16 == 0,
                            "psx_adpcm_28_samples": l.start % 28 == 0 && file.frames % 28 == 0}});
                    entry["smpl_loop"] = match smpl_loop_problem(l, file.frames) {
                        None => json!("usable with loop_policy preserve"),
                        Some(problem) => json!(format!("unusable: smpl loop {index} {problem}")),
                    };
                    entry
                }).collect::<Vec<_>>(),
            })
        }
    };
    facts
}

/// Declared rate, root note and, for BRR, the S-DSP pitch register values;
/// for PSX ADPCM, the SPU pitch register values.
fn tuning(rate: u32, target: &str, root: Option<(Root, &str)>) -> Option<Value> {
    let psx_target = psx_adpcm::ENCODINGS.contains(&target);
    if target != "brr" && !psx_target && root.is_none() {
        return None;
    }
    let mut tuning = json!({"sample_rate": rate,
        "root": root.map(|(r, source)| json!({"note": r.note, "cents": r.cents, "source": source}))});
    if target == "brr" {
        let unity = rate as f64 * 4096.0 / DSP_RATE as f64;
        let register = |exact: f64| {
            let value = exact.round();
            if (1.0..=MAX_PITCH as f64).contains(&value) {
                json!({"value": value as u32, "hex": format!("0x{:04X}", value as u32),
                    "exact": exact, "played_rate": value * DSP_RATE as f64 / 4096.0,
                    "rounding_cents": 1200.0 * (value / exact).log2(), "in_range": true})
            } else {
                json!({"value": null, "exact": exact, "in_range": false})
            }
        };
        let mut snes = json!({"dsp_output_rate": DSP_RATE, "register_limit": MAX_PITCH,
            "formula": "pitch = sample_rate * 4096 / 32000",
            "original_pitch": register(unity)});
        if let Some((r, _)) = root {
            let at =
                |note: i32| unity * ((note as f64 - r.note as f64 - r.cents / 100.0) / 12.0).exp2();
            snes["root_in_tune"] = register(at(r.note as i32));
            let playable: Vec<i32> = (0..=127)
                .filter(|&n| (1.0..=MAX_PITCH as f64).contains(&at(n).round()))
                .collect();
            snes["in_tune_note_range"] = match (playable.first(), playable.last()) {
                (Some(low), Some(high)) => json!({"lowest": low, "highest": high}),
                _ => Value::Null,
            };
        }
        tuning["snes_pitch"] = snes;
    }
    if psx_target {
        tuning["spu_pitch"] = spu_pitch(rate, root);
    }
    Some(tuning)
}

/// SPU pitch register values: 0x1000 plays at 44100 Hz, 0x4000 is fastest.
fn spu_pitch(rate: u32, root: Option<(Root, &str)>) -> Value {
    let limit = psx_adpcm::MAX_PITCH as f64;
    let unity = rate as f64 * 4096.0 / psx_adpcm::SPU_RATE as f64;
    let register = |exact: f64| {
        let value = exact.round();
        if (1.0..=limit).contains(&value) {
            json!({"value": value as u32, "hex": format!("0x{:04X}", value as u32),
                "exact": exact, "played_rate": value * psx_adpcm::SPU_RATE as f64 / 4096.0,
                "rounding_cents": 1200.0 * (value / exact).log2(), "in_range": true})
        } else {
            json!({"value": null, "exact": exact, "in_range": false})
        }
    };
    let mut spu = json!({"spu_output_rate": psx_adpcm::SPU_RATE, "register_limit": psx_adpcm::MAX_PITCH,
        "formula": "pitch = sample_rate * 4096 / 44100",
        "original_pitch": register(unity)});
    if let Some((r, _)) = root {
        let at =
            |note: i32| unity * ((note as f64 - r.note as f64 - r.cents / 100.0) / 12.0).exp2();
        spu["root_in_tune"] = register(at(r.note as i32));
        let playable: Vec<i32> = (0..=127)
            .filter(|&n| (1.0..=limit).contains(&at(n).round()))
            .collect();
        spu["in_tune_note_range"] = match (playable.first(), playable.last()) {
            (Some(low), Some(high)) => json!({"lowest": low, "highest": high}),
            _ => Value::Null,
        };
    }
    spu
}

/// Option combinations that name a side the request does not have.
fn check_psx_options(
    options: &PsxOptions,
    request: &SampleRequest,
    psx_source: bool,
    psx_target: bool,
) -> Result<(), String> {
    if !psx_source && !psx_target {
        return Err("psx_adpcm options need a psx_adpcm or vag encoding or target".into());
    }
    if let Some(name) = &options.name {
        if request.target != psx_adpcm::VAG {
            return Err("psx_adpcm.name sets a VAG header; it needs target vag".into());
        }
        psx_adpcm::check_name(name)?;
    }
    if options.end_mute && (!psx_target || request.loop_policy == LoopPolicy::Preserve) {
        return Err(
            "psx_adpcm.one_shot_end applies to one-shot psx_adpcm or vag targets (loop_policy finite)"
                .into(),
        );
    }
    if options.reinterpret_rate && request.encoding != psx_adpcm::VAG {
        return Err("psx_adpcm.reinterpret_rate applies to a vag source".into());
    }
    if options.reserved_shift_as_9 && !psx_source {
        return Err("psx_adpcm.reserved_shift_as_9 applies to a psx_adpcm or vag source".into());
    }
    Ok(())
}

/// A PSX target: file bytes (with a VAG header when `target` is vag), the
/// parsed stream for readback and its placement report.
struct PsxTarget {
    bytes: Vec<u8>,
    stream: Stream,
    header_bytes: usize,
    report: Value,
}

fn encode_psx(
    pcm: &[i32],
    end: TargetEnd,
    target: &str,
    source: &Source,
    rate: u32,
    options: &PsxOptions,
) -> Result<PsxTarget, String> {
    let (blocks, mut report) = psx_adpcm::encode(pcm, end);
    let stream = Stream::parse(&blocks, 0, None, false, usize::MAX)
        .map_err(|e| format!("internal PSX ADPCM readback failed: {e}"))?;
    let (bytes, header_bytes) = if target == psx_adpcm::VAG {
        let (header, header_report) = psx_adpcm::build_header(
            source.vag_header.as_ref(),
            blocks.len(),
            rate,
            options.name.as_deref(),
        )?;
        report["vag"] = header_report;
        (
            [&header[..], &blocks[..]].concat(),
            psx_adpcm::VAG_HEADER_BYTES,
        )
    } else {
        (blocks, 0)
    };
    report["readback"] = json!(
        "the written blocks were parsed again; their flags and decoded samples are what this report compares"
    );
    Ok(PsxTarget {
        bytes,
        stream,
        header_bytes,
        report,
    })
}

/// A PSX ADPCM stream or VAG file as a finite stream or a preserved loop.
fn psx_source_stream(request: &SampleRequest) -> Result<Source, String> {
    let options = request.psx.clone().unwrap_or_default();
    let mut changes = Map::new();
    let (blocks, base, vag) = if request.encoding == psx_adpcm::VAG {
        let vag = Vag::parse(request.data)?;
        if vag.sample_rate != request.sample_rate {
            if !options.reinterpret_rate {
                return Err(format!(
                    "the VAG header declares {} Hz but sample_rate is {}. Pass sample_rate={} (add target_sample_rate to resample), or set psx_adpcm.reinterpret_rate=true to play these blocks at {} Hz, which changes pitch and duration",
                    vag.sample_rate, request.sample_rate, vag.sample_rate, request.sample_rate
                ));
            }
            changes.insert(
                "rate_reinterpreted".into(),
                json!({"header_rate": vag.sample_rate, "declared_rate": request.sample_rate,
                    "pitch_change_cents": 1200.0 * (request.sample_rate as f64 / vag.sample_rate as f64).log2()}),
            );
        }
        let start = psx_adpcm::VAG_HEADER_BYTES;
        (
            &request.data[start..start + vag.data_size],
            start,
            Some(vag),
        )
    } else {
        (request.data, 0, None)
    };
    let stream = Stream::parse(
        blocks,
        base,
        request.loop_offset,
        options.reserved_shift_as_9,
        MAX_SAMPLES,
    )?;
    if !stream.reserved_shift_blocks.is_empty() {
        changes.insert(
            "reserved_shift_interpreted".into(),
            json!({"blocks": stream.reserved_shift_blocks.len(), "decoded_as_shift": psx_adpcm::RESERVED_SHIFT_AS,
                "basis": "documented for CD-XA headers, which the SPU header reportedly shares; not verified on hardware"}),
        );
    }
    if !stream.unknown_flag_blocks.is_empty() {
        changes.insert(
            "flag_bits_not_carried".into(),
            json!({"blocks": stream.unknown_flag_blocks.len(), "bits": "3-7 (documented as unknown, usually 0)",
                "note": "retained in original.bin and input.bin; the target does not reproduce them"}),
        );
    }
    let looping = stream.loop_start_block().is_some();
    let mut source = Source::new(Vec::new(), request);
    let blocks = stream.end_block + 1;
    let (pcm, body, loop_start) = match request.loop_policy {
        LoopPolicy::Finite => {
            if !looping && request.loop_repeats > 0 {
                return Err(format!(
                    "the source ends with {} and does not loop, so it cannot repeat",
                    stream.report()["ending"].as_str().unwrap_or("")
                ));
            }
            let start = stream.loop_start_block().unwrap_or(0);
            let total = blocks + (blocks - start) * request.loop_repeats as usize;
            if total * psx_adpcm::BLOCK_SAMPLES > MAX_SAMPLES {
                return Err("expanded sample limit exceeded".into());
            }
            if looping {
                changes.insert(
                    "finite_loop".into(),
                    json!({"additional_traversals": request.loop_repeats, "history_carried_across_loop": true, "target_loop": false}),
                );
            }
            (
                stream.with_traversals(request.loop_repeats as usize),
                Vec::new(),
                None,
            )
        }
        LoopPolicy::Preserve => {
            if request.loop_repeats > 0 {
                return Err("loop_policy=preserve keeps the loop; loop_repeats must be 0".into());
            }
            let start = stream.loop_start_block().ok_or_else(|| {
                format!(
                    "loop_policy=preserve needs a looping source, but this one ends with {}; use loop_policy=finite",
                    stream.report()["ending"].as_str().unwrap_or("")
                )
            })?;
            if (2 * blocks - start) * psx_adpcm::BLOCK_SAMPLES > MAX_SAMPLES {
                return Err("expanded sample limit exceeded".into());
            }
            let reference = stream.with_traversals(1);
            let first = blocks * psx_adpcm::BLOCK_SAMPLES;
            source.loop_report.insert(
                "source_loop_offset".into(),
                json!(base + start * psx_adpcm::BLOCK_BYTES),
            );
            (
                reference[..first].to_vec(),
                reference[first..].to_vec(),
                Some(start * psx_adpcm::BLOCK_SAMPLES),
            )
        }
    };
    let mut report = stream.report().clone();
    if let Some(vag) = &vag {
        report["vag"] = vag.report();
        source.vag_header = Some(vag.header);
    }
    report["container"] = json!(request.encoding);
    source.pcm = pcm;
    source.body = body;
    source.loop_start = loop_start;
    source.changes = changes;
    source.psx = Some(report);
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(data: &'a [u8], encoding: &'a str, target: &'a str) -> SampleRequest<'a> {
        SampleRequest {
            data,
            encoding,
            target,
            sample_rate: 32000,
            loop_offset: None,
            loop_repeats: 0,
            loop_policy: LoopPolicy::Finite,
            wav: None,
            target_sample_rate: None,
            root: None,
            psx: None,
            md_dac: None,
            gain_db: None,
        }
    }

    #[test]
    fn psx_padding_cannot_create_an_output_beyond_the_reinput_bound() {
        let pcm = vec![0; MAX_SAMPLES * 2];
        let error = convert(&request(&pcm, "s16le", "psx_adpcm"))
            .expect_err("padded output would exceed the decoded admission bound");
        assert!(
            error.contains("PSX block padding and termination"),
            "{error}"
        );
    }

    #[test]
    fn brr_decoder_follows_the_documented_integer_filters() {
        // Shift 12 filter 0: nibble 7 -> (7 << 12) >> 1 = 14336, doubled.
        assert_eq!(step(7, 12, 0, 0, 0), 28672);
        // Doubling wraps at 16 bits; clamping happens first.
        assert_eq!(step(7, 12, 1, 32766, 0), -6146);
        assert_eq!(step(7, 12, 3, 32766, 0), -2);
        // Shifts above 12 collapse to -2048 or 0.
        assert_eq!(step(-1, 13, 0, 0, 0), -4096);
        assert_eq!(step(5, 15, 0, 0, 0), 0);
        // Filter 2 uses both history samples.
        assert_eq!(
            step(0, 0, 2, 1000, 400),
            2 * (1000 + (-1500 >> 5) - 200 + (200 >> 4))
        );
    }

    #[test]
    fn pcm16_round_trips_and_pcm8_quantization_is_measured() {
        let data: Vec<u8> = [0i16, 1000, -1000, 32767, -32768]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        let exact = convert(&request(&data, "s16le", "s16be")).unwrap();
        assert_eq!(exact.report["decoded_stream_exact"], true);
        let lossy = convert(&request(&data, "s16le", "u8")).unwrap();
        assert_eq!(
            lossy.report["changes"]["sample_values"]["changed_samples"],
            3
        );
        assert_eq!(lossy.files["sample.bin"], [128, 132, 124, 255, 0]);
    }

    #[test]
    fn brr_encoding_pads_to_blocks_and_marks_the_end() {
        let pcm: Vec<u8> = (0..20i16).flat_map(|i| (i * 512).to_le_bytes()).collect();
        let out = convert(&request(&pcm, "s16le", "brr")).unwrap();
        let brr = &out.files["sample.bin"];
        assert_eq!(brr.len(), 18);
        assert_eq!(brr[0] & 0x0c, 0, "first block uses filter 0");
        assert_eq!((brr[0] & 1, brr[9] & 3), (0, 1));
        assert_eq!(out.report["changes"]["padding"]["added_samples"], 12);
    }

    #[test]
    fn preserved_loops_need_compatible_alignment_and_compare_reentry() {
        let pcm: Vec<u8> = (0..48i16).flat_map(|i| (i * 256).to_le_bytes()).collect();
        let mut r = request(&pcm, "s16le", "brr");
        r.loop_policy = LoopPolicy::Preserve;
        r.loop_offset = Some(16 * 2);
        let out = convert(&r).unwrap();
        let brr = &out.files["sample.bin"];
        assert_eq!(brr[18] & 3, 3, "end and loop flags");
        assert_eq!(brr[9] & 0x0c, 0, "loop block restarts with filter 0");
        assert_eq!(out.report["loop"]["target_loop_offset"], 9);
        assert!(out.report["changes"].get("finite_loop").is_none());
        // Round trip the preserved BRR: exact loop interpretation, no changes.
        let mut again = request(brr, "brr", "brr");
        again.loop_policy = LoopPolicy::Preserve;
        again.loop_offset = Some(9);
        let again = convert(&again).unwrap();
        assert_eq!(again.files["sample.bin"], *brr);
        assert_eq!(again.report["decoded_stream_exact"], true);
        r.loop_offset = Some(10);
        assert!(convert(&r).unwrap_err().contains("16-sample-aligned"));
        r.loop_offset = None;
        assert!(convert(&r).is_err());
    }

    #[test]
    fn brr_targets_report_the_dsp_pitch_register_for_the_declared_rate() {
        let pcm: Vec<u8> = (0..16i16).flat_map(|i| (i * 100).to_le_bytes()).collect();
        let mut r = request(&pcm, "s16le", "brr");
        let pitch = |r: &SampleRequest| convert(r).unwrap().report["tuning"]["snes_pitch"].clone();
        assert_eq!(pitch(&r)["original_pitch"]["value"], 0x1000);
        r.sample_rate = 16000;
        r.root = Some(Root {
            note: 60,
            cents: -50.0,
        });
        let p = pitch(&r);
        assert_eq!(p["original_pitch"]["hex"], "0x0800");
        // A flat root plays in tune with a higher register: 2048 * 2^(50/1200).
        assert_eq!(p["root_in_tune"]["value"], 2108);
        assert_eq!(p["in_tune_note_range"]["highest"], 95);
        r.sample_rate = 128000;
        assert_eq!(pitch(&r)["original_pitch"]["in_range"], false);
        // PCM targets record the root and rate without a register.
        let mut r = request(&pcm, "s16le", "u8");
        assert!(convert(&r).unwrap().report.get("tuning").is_none());
        r.root = Some(Root {
            note: 69,
            cents: 0.0,
        });
        let t = convert(&r).unwrap().report["tuning"].clone();
        assert_eq!(
            (t["root"]["note"].as_u64(), t.get("snes_pitch")),
            (Some(69), None)
        );
    }

    #[test]
    fn wav_loops_resample_with_their_loop_points() {
        let frames: Vec<u8> = (0..64i16)
            .flat_map(|i| ((i % 32) * 400).to_le_bytes())
            .collect();
        let smpl = wav::smpl_body(60, 0, &[(0, 32, 63, 0)]);
        let wav = wav::build(1, 1, 32000, 16, None, &frames, &[(b"smpl", smpl)]);
        let mut r = request(&wav, WAV, "brr");
        r.loop_policy = LoopPolicy::Preserve;
        r.wav = Some(WavOptions {
            smpl_loop: Some(0),
            ..Default::default()
        });
        r.target_sample_rate = Some(16000);
        let out = convert(&r).unwrap();
        let report = &out.report;
        assert_eq!(report["loop"]["target_loop_start_sample"], 16);
        assert_eq!(report["loop"]["loop_source"], "smpl loop 0");
        assert_eq!(report["changes"]["resampled"]["loop"]["target_length"], 16);
        assert_eq!(out.files["sample.bin"].len(), 18);
        assert_eq!(out.files["sample.bin"][9] & 3, 3);
        // 32000 -> 24000 would put the loop at sample 24: not BRR-aligned.
        r.target_sample_rate = Some(24000);
        assert!(
            convert(&r)
                .unwrap_err()
                .contains("at 24000 Hz after resampling")
        );
        // 32000 -> 44100 cannot scale the loop to whole samples.
        r.target_sample_rate = Some(44100);
        assert!(convert(&r).unwrap_err().contains("whole samples"));
    }

    #[test]
    fn preserved_loop_previews_show_reentry_differences_and_carry_smpl_loops() {
        // Block 1 (the loop) predicts from history: entering it after block 0
        // differs from re-entering it after itself.
        let mut brr = vec![0xC0];
        brr.extend([0x77; 8]);
        brr.push(0xC7);
        brr.extend([0x00; 8]);
        let mut r = request(&brr, "brr", "s16le");
        r.loop_policy = LoopPolicy::Preserve;
        r.loop_offset = Some(9);
        let out = convert(&r).unwrap();
        let values = &out.report["changes"]["sample_values"];
        assert_eq!(values["first_pass_changed_samples"], 0);
        assert_eq!(values["first_pass_samples"], 32);
        assert!(values["loop_traversal_changed_samples"].as_u64().unwrap() > 0);
        assert!(values["first_changed_sample"].as_u64().unwrap() >= 32);
        assert!(
            out.report["loop"]["loop_in_sample_bin"]
                .as_str()
                .unwrap()
                .contains("report.json")
        );
        // Both previews hold first pass plus one traversal and differ there.
        assert_ne!(out.files["source.wav"], out.files["target.wav"]);
        for name in ["source.wav", "target.wav"] {
            let preview = wav::parse(&out.files[name]).unwrap();
            assert_eq!(preview.frames, 48, "{name}");
            let smpl = preview.smpl.unwrap().unwrap();
            assert_eq!((smpl.loops[0].start, smpl.loops[0].end), (32, 47), "{name}");
            assert_eq!(smpl.unity_note, 60);
        }
        // A root becomes the previews' unity note and pitch fraction.
        r.root = Some(Root {
            note: 64,
            cents: 50.0,
        });
        let out = convert(&r).unwrap();
        let smpl = wav::parse(&out.files["target.wav"])
            .unwrap()
            .smpl
            .unwrap()
            .unwrap();
        assert_eq!((smpl.unity_note, smpl.pitch_fraction), (64, 1 << 31));
        // Finite previews keep the plain 44-byte header.
        let mut finite = request(&brr, "brr", "s16le");
        finite.loop_offset = Some(9);
        let finite = convert(&finite).unwrap();
        assert_eq!(finite.files["target.wav"].len(), 44 + 32 * 2);
        assert!(
            wav::parse(&finite.files["target.wav"])
                .unwrap()
                .smpl
                .is_none()
        );
    }
}
