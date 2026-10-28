//! CRI ADX audio streams: header interpretation, decoding and encoding.
//!
//! Only the standard ADPCM encoding (type 3) with 4-bit samples is decoded.
//! Every header byte is either an interpreted field or reported as unknown
//! with its offset; refusals name the byte they stop at. The block arithmetic
//! is an explicit interpretation of the format:
//! [`Arithmetic::Fixed12`] by default, [`Arithmetic::FloatReference`] for the
//! described floating-point example. Encryption is detected and refused; no
//! key handling exists here.
use serde_json::{Value, json};
use std::f64::consts::{PI, SQRT_2};

pub const SIGNATURE: u16 = 0x8000;
pub const COPYRIGHT: &[u8; 6] = b"(c)CRI";
/// Encoding type of standard ADX, the only one decoded.
pub const STANDARD_ENCODING: u8 = 3;
/// The documented encryption value of the flags byte.
pub const ENCRYPTION_FLAG: u8 = 0x08;
/// Block size the encoder writes (2-byte scale, 32 nibbles).
pub const BLOCK_BYTES: usize = 18;
pub const BLOCK_SAMPLES: usize = 32;
/// The documented scale field is 13 bits wide.
pub const MAX_SCALE: u16 = 0x1FFF;
/// Scale word observed on the block after the last data row.
pub const END_SCALE: u16 = 0x8001;
pub const MAX_CHANNELS: u8 = 16;
pub const MAX_SAMPLE_RATE: u32 = 192_000;
/// Decoded samples (frames x channels) one decode may produce.
pub const MAX_DECODED_SAMPLES: u64 = 64 * 1024 * 1024;
/// Fixed-point precision of [`Arithmetic::Fixed12`] coefficients.
pub const FIXED_BITS: u32 = 12;
/// Header bytes before the loop fields (signature through flags).
const BASE_HEADER: usize = 0x14;
/// Copyright offset the encoder writes without a loop (as observed).
pub const SHORT_COPYRIGHT_OFFSET: u16 = 0x20;
/// Smallest copyright offset whose header holds every version 3 loop field.
pub const LOOP_COPYRIGHT_OFFSET: u16 = 0x2E;

/// A malformed or unadmitted stream, located at a byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdxError {
    pub offset: usize,
    pub field: &'static str,
    pub message: String,
}

impl std::fmt::Display for AdxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn error(offset: usize, field: &'static str, message: impl Into<String>) -> AdxError {
    AdxError {
        offset,
        field,
        message: message.into(),
    }
}

/// How the block equation is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Arithmetic {
    /// Coefficients rounded to 4.12 fixed point; prediction
    /// `(k0 * h1 + k1 * h2) >> 12` (floor); the clamped sample is history.
    #[default]
    Fixed12,
    /// The described floating-point example: double coefficients, the
    /// prediction truncated toward zero, the unclamped sum kept as history.
    FloatReference,
}

impl Arithmetic {
    pub fn name(self) -> &'static str {
        match self {
            Arithmetic::Fixed12 => "fixed_12",
            Arithmetic::FloatReference => "float_reference",
        }
    }
    pub fn rule(self) -> &'static str {
        match self {
            Arithmetic::Fixed12 => {
                "k = round(4096 * coefficient); sample = nibble * scale + ((k0 * h1 + k1 * h2) >> 12), clamped to 16 bits; the clamped sample becomes history"
            }
            Arithmetic::FloatReference => {
                "sample = nibble * scale + trunc(c0 * h1 + c1 * h2) in double precision; output clamped to 16 bits; the unclamped sum becomes history"
            }
        }
    }
}

/// The two predictor coefficients from the highpass cutoff and sample rate:
/// a = sqrt(2) - cos(2 pi f / rate), b = sqrt(2) - 1,
/// c = (a - sqrt((a + b)(a - b))) / b, coefficients 2c and -c^2.
pub fn coefficients(highpass: u16, rate: u32) -> (f64, f64) {
    let a = SQRT_2 - (2.0 * PI * f64::from(highpass) / f64::from(rate)).cos();
    let b = SQRT_2 - 1.0;
    let c = (a - ((a + b) * (a - b)).max(0.0).sqrt()) / b;
    (2.0 * c, -(c * c))
}

pub fn fixed_coefficients(highpass: u16, rate: u32) -> (i32, i32) {
    let (c0, c1) = coefficients(highpass, rate);
    let scale = f64::from(1u32 << FIXED_BITS);
    ((c0 * scale).round() as i32, (c1 * scale).round() as i32)
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

/// Lowercase hex of raw bytes.
pub fn hex_bytes(b: &[u8]) -> String {
    hex(b)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The loop the header declares, in samples and absolute byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loop {
    pub start_sample: u32,
    pub start_byte: u32,
    /// Exclusive: the last looped sample is `end_sample - 1` (declared reading).
    pub end_sample: u32,
    pub end_byte: u32,
}

#[derive(Debug, Clone)]
pub struct Adx<'a> {
    pub bytes: &'a [u8],
    pub copyright_offset: u16,
    pub encoding: u8,
    pub block_size: usize,
    pub bit_depth: u8,
    pub channels: usize,
    pub sample_rate: u32,
    pub total_samples: u32,
    pub highpass: u16,
    pub version: u8,
    pub flags: u8,
    pub data_start: usize,
    /// Block rows (one block per channel each) holding `total_samples`.
    pub rows: usize,
    pub data_end: usize,
    pub looping: Option<Loop>,
    fields: Vec<Value>,
    unknown: Vec<Value>,
    loop_report: Value,
    end_report: Value,
    pub trailing_bytes: usize,
}

struct Fields<'b> {
    bytes: &'b [u8],
    /// First byte of the copyright marker: header fields must end at or before it.
    limit: usize,
    rows: Vec<Value>,
    covered: Vec<(usize, usize)>,
}

impl Fields<'_> {
    /// A big-endian field, if it fits wholly before the copyright marker.
    fn read(&mut self, offset: usize, size: usize, name: &str, meaning: Value) -> Option<u32> {
        if offset + size > self.limit {
            return None;
        }
        let raw = &self.bytes[offset..offset + size];
        let value = raw.iter().fold(0u32, |v, &b| (v << 8) | u32::from(b));
        self.rows.push(json!({"offset": offset, "bytes": size, "name": name, "raw_hex": hex(raw), "value": value, "meaning": meaning}));
        self.covered.push((offset, offset + size));
        Some(value)
    }
    fn mark(&mut self, start: usize, end: usize) {
        self.covered.push((start, end));
    }
}

impl<'a> Adx<'a> {
    /// Interprets the header and locates the blocks; refuses what is not
    /// admitted. The bytes are never changed.
    pub fn parse(bytes: &'a [u8]) -> Result<Adx<'a>, AdxError> {
        if bytes.len() < 4 {
            return Err(error(
                0,
                "signature",
                format!("{} bytes cannot hold an ADX header", bytes.len()),
            ));
        }
        let signature = be16(bytes, 0);
        if signature != SIGNATURE {
            return Err(error(
                0,
                "signature",
                format!("signature 0x{signature:04x} is not 0x8000"),
            ));
        }
        let copyright_offset = be16(bytes, 2);
        let marker = copyright_offset as usize;
        if marker < BASE_HEADER + 2 {
            return Err(error(
                2,
                "copyright_offset",
                format!(
                    "copyright offset {copyright_offset} leaves no room for the 20-byte base header before the (c)CRI marker at offset - 2; the smallest is 22"
                ),
            ));
        }
        let data_start = marker + 4;
        if data_start > bytes.len() {
            return Err(error(
                2,
                "copyright_offset",
                format!(
                    "copyright offset {copyright_offset} puts the data start at byte {data_start}, beyond the {}-byte file",
                    bytes.len()
                ),
            ));
        }
        let cri = &bytes[marker - 2..data_start];
        if cri != COPYRIGHT {
            return Err(error(
                marker - 2,
                "copyright",
                format!(
                    "bytes {}..{} are {:?} ({}), not \"(c)CRI\"",
                    marker - 2,
                    data_start,
                    String::from_utf8_lossy(cri),
                    hex(cri)
                ),
            ));
        }
        let mut f = Fields {
            bytes,
            limit: marker - 2,
            rows: vec![],
            covered: vec![],
        };
        f.read(0, 2, "signature", json!("0x8000"));
        f.read(
            2,
            2,
            "copyright_offset",
            json!(format!("(c)CRI at {}, data at {data_start}", marker - 2)),
        );
        f.mark(marker - 2, data_start);
        let encoding = f.read(4, 1, "encoding_type", json!(null)).unwrap_or(0) as u8;
        let block_size = f
            .read(5, 1, "block_size", json!("bytes per channel block"))
            .unwrap_or(0) as usize;
        let bit_depth = f.read(6, 1, "sample_bitdepth", json!(null)).unwrap_or(0) as u8;
        let channels = f.read(7, 1, "channel_count", json!(null)).unwrap_or(0) as usize;
        let sample_rate = f.read(8, 4, "sample_rate", json!("Hz")).unwrap_or(0);
        let total_samples = f
            .read(12, 4, "total_samples", json!("per channel"))
            .unwrap_or(0);
        let highpass = f
            .read(
                16,
                2,
                "highpass_frequency",
                json!("Hz; derives the predictor coefficients"),
            )
            .unwrap_or(0) as u16;
        let version = f.read(18, 1, "version", json!(null)).unwrap_or(0) as u8;
        let flags = f.read(19, 1, "flags", json!(null)).unwrap_or(0) as u8;
        match encoding {
            STANDARD_ENCODING => {}
            2 => {
                return Err(error(
                    4,
                    "encoding_type",
                    "encoding type 2 (preset prediction coefficients) is not admitted; only standard ADX (3) is decoded",
                ));
            }
            4 => {
                return Err(error(
                    4,
                    "encoding_type",
                    "encoding type 4 (exponential scale) is not admitted: the public description gives no semantics; only standard ADX (3) is decoded",
                ));
            }
            0x10 | 0x11 => {
                return Err(error(
                    4,
                    "encoding_type",
                    format!(
                        "encoding type 0x{encoding:02x} is AHX (MPEG-1 Audio Layer II in the ADX container), not ADPCM; it is not decoded"
                    ),
                ));
            }
            other => {
                return Err(error(
                    4,
                    "encoding_type",
                    format!(
                        "encoding type 0x{other:02x} is undocumented; only standard ADX (3) is decoded"
                    ),
                ));
            }
        }
        if flags == ENCRYPTION_FLAG {
            return Err(error(
                19,
                "flags",
                "flags 0x08 marks an encrypted stream (scales XORed with a key-derived sequence); encrypted ADX is refused and no key recovery is attempted",
            ));
        }
        if flags != 0 {
            return Err(error(
                19,
                "flags",
                format!(
                    "flags 0x{flags:02x} are undocumented (0x08 is the documented encryption flag); the stream may be encrypted or a different variant and is not decoded"
                ),
            ));
        }
        if bit_depth != 4 {
            return Err(error(
                6,
                "sample_bitdepth",
                format!("sample bit depth {bit_depth} is not admitted; standard ADX uses 4"),
            ));
        }
        if block_size < 3 {
            return Err(error(
                5,
                "block_size",
                format!("block size {block_size} leaves no samples after the 2-byte scale"),
            ));
        }
        if channels == 0 || channels > MAX_CHANNELS as usize {
            return Err(error(
                7,
                "channel_count",
                format!("channel count {channels} is outside 1..{MAX_CHANNELS}"),
            ));
        }
        if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
            return Err(error(
                8,
                "sample_rate",
                format!("sample rate {sample_rate} is outside 1..{MAX_SAMPLE_RATE} Hz"),
            ));
        }
        if total_samples == 0 {
            return Err(error(12, "total_samples", "total samples is 0"));
        }
        let spb = (block_size - 2) * 2;
        let rows = (total_samples as usize).div_ceil(spb);
        let row_bytes = block_size * channels;
        let data_end = data_start + rows * row_bytes;
        if data_end > bytes.len() {
            let missing_row = (bytes.len() - data_start) / row_bytes;
            return Err(error(
                bytes.len(),
                "data",
                format!(
                    "{total_samples} samples need {rows} block rows of {row_bytes} bytes ({data_start}..{data_end}); the file ends at {} inside row {missing_row}",
                    bytes.len()
                ),
            ));
        }
        // Scale words: the documented field is 13 bits; for type 3 the top
        // three bits carry nothing described, and encryption alters scales.
        for row in 0..rows {
            for c in 0..channels {
                let at = data_start + row * row_bytes + c * block_size;
                let scale = be16(bytes, at);
                if scale > MAX_SCALE {
                    return Err(error(
                        at,
                        "scale",
                        format!(
                            "row {row} channel {c}: scale word 0x{scale:04x} sets bits above the 13-bit scale; undocumented for encoding 3 (the stream may be encrypted without the flag)"
                        ),
                    ));
                }
            }
        }
        let (looping, loop_report) = match version {
            3 => Self::loop_v3(&mut f, data_start, spb, row_bytes, total_samples)?,
            4 => Self::loop_v4(&mut f, data_start, spb, row_bytes, total_samples)?,
            5 => (
                None,
                json!({"state": "not_supported_by_version", "note": "version 5 is described as a version 4 variant without looping; bytes after the flags are reported as unknown"}),
            ),
            other => {
                return Err(error(
                    18,
                    "version",
                    format!("version {other} is undocumented; versions 3, 4 and 5 are admitted"),
                ));
            }
        };
        let unknown = unknown_ranges(bytes, &mut f.covered, marker - 2);
        let (end_report, trailing_bytes) = end_marker(bytes, data_end);
        Ok(Adx {
            bytes,
            copyright_offset,
            encoding,
            block_size,
            bit_depth,
            channels,
            sample_rate,
            total_samples,
            highpass,
            version,
            flags,
            data_start,
            rows,
            data_end,
            looping,
            fields: f.rows,
            unknown,
            loop_report,
            end_report,
            trailing_bytes,
        })
    }

    #[allow(clippy::type_complexity)]
    fn loop_v3(
        f: &mut Fields,
        data_start: usize,
        spb: usize,
        row_bytes: usize,
        total: u32,
    ) -> Result<(Option<Loop>, Value), AdxError> {
        let alignment = f.read(
            0x14,
            2,
            "loop_alignment_samples",
            json!("reported; meaning not described"),
        );
        let enabled16 = f.read(0x16, 2, "loop_enabled", json!(null));
        let enabled32 = f.read(0x18, 4, "loop_enabled", json!(null));
        let start = f.read(0x1C, 4, "loop_begin_sample", json!(null));
        let start_byte = f.read(0x20, 4, "loop_begin_byte", json!("absolute file offset"));
        let end = f.read(
            0x24,
            4,
            "loop_end_sample",
            json!("exclusive (declared reading)"),
        );
        let end_byte = f.read(0x28, 4, "loop_end_byte", json!("absolute file offset"));
        let enabled: Vec<(usize, u32)> = [(0x16, enabled16), (0x18, enabled32)]
            .into_iter()
            .filter_map(|(o, v)| v.map(|v| (o, v)))
            .collect();
        for &(o, v) in &enabled {
            if v > 1 {
                return Err(error(
                    o,
                    "loop_enabled",
                    format!("loop enabled value {v} is neither 0 nor 1"),
                ));
            }
        }
        if enabled.len() == 2 && enabled[0].1 != enabled[1].1 {
            return Err(error(
                0x16,
                "loop_enabled",
                format!(
                    "the two loop-enabled fields disagree (0x16 = {}, 0x18 = {})",
                    enabled[0].1, enabled[1].1
                ),
            ));
        }
        let on = (!enabled.is_empty()).then(|| enabled.iter().any(|&(_, v)| v == 1));
        Self::finish_loop(
            on,
            [start, start_byte, end, end_byte],
            [0x1C, 0x20, 0x24, 0x28],
            alignment,
            data_start,
            spb,
            row_bytes,
            total,
        )
    }

    fn loop_v4(
        f: &mut Fields,
        data_start: usize,
        spb: usize,
        row_bytes: usize,
        total: u32,
    ) -> Result<(Option<Loop>, Value), AdxError> {
        let enabled = f.read(0x24, 4, "loop_enabled", json!(null));
        let start = f.read(0x28, 4, "loop_begin_sample", json!(null));
        let start_byte = f.read(0x2C, 4, "loop_begin_byte", json!("absolute file offset"));
        let end = f.read(
            0x30,
            4,
            "loop_end_sample",
            json!("exclusive (declared reading)"),
        );
        let end_byte = f.read(0x34, 4, "loop_end_byte", json!("absolute file offset"));
        if let Some(v) = enabled
            && v > 1
        {
            return Err(error(
                0x24,
                "loop_enabled",
                format!("loop enabled value {v} is neither 0 nor 1"),
            ));
        }
        Self::finish_loop(
            enabled.map(|v| v == 1),
            [start, start_byte, end, end_byte],
            [0x28, 0x2C, 0x30, 0x34],
            None,
            data_start,
            spb,
            row_bytes,
            total,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_loop(
        on: Option<bool>,
        values: [Option<u32>; 4],
        offsets: [usize; 4],
        alignment: Option<u32>,
        data_start: usize,
        spb: usize,
        row_bytes: usize,
        total: u32,
    ) -> Result<(Option<Loop>, Value), AdxError> {
        match on {
            None => {
                return Ok((
                    None,
                    json!({"state": "absent", "note": "the header ends before the loop-enabled fields"}),
                ));
            }
            Some(false) => return Ok((None, json!({"state": "disabled"}))),
            Some(true) => {}
        }
        let [Some(start), Some(start_byte), Some(end), Some(end_byte)] = values else {
            let missing = values
                .iter()
                .position(Option::is_none)
                .expect("one missing");
            return Err(error(
                offsets[missing],
                "loop",
                format!(
                    "the loop is enabled but the header ends before the loop field at 0x{:x}",
                    offsets[missing]
                ),
            ));
        };
        if start >= end {
            return Err(error(
                offsets[0],
                "loop_begin_sample",
                format!("loop begin sample {start} is not before loop end sample {end}"),
            ));
        }
        if end > total {
            return Err(error(
                offsets[2],
                "loop_end_sample",
                format!("loop end sample {end} lies beyond the {total} total samples"),
            ));
        }
        let expected_start = data_start + (start as usize / spb) * row_bytes;
        let expected_end = data_start + (end as usize).div_ceil(spb) * row_bytes;
        let looping = Loop {
            start_sample: start,
            start_byte,
            end_sample: end,
            end_byte,
        };
        Ok((
            Some(looping),
            json!({
                "state": "enabled",
                "start_sample": start, "end_sample_exclusive": end,
                "loop_frames": end - start,
                "start_byte": start_byte, "end_byte": end_byte,
                "start_on_block_boundary": (start as usize).is_multiple_of(spb),
                "start_byte_expected": expected_start,
                "start_byte_consistent": start_byte as usize == expected_start,
                "end_byte_expected": expected_end,
                "end_byte_consistent": end_byte as usize == expected_end,
                "byte_rule": "observed relation: begin byte = data start + floor(begin / samples per block) x row bytes; end byte = data start + ceil(end / samples per block) x row bytes. Playback uses the sample indices",
                "alignment_samples": alignment,
                "alignment_reading": "declared: silent lead-in frames the encoder prepended so the source loop start lands on a block boundary (source loop start = begin - alignment); observed as leading silence of at least that length in every corpus stream, not described in the cited documentation",
                "frames_after_loop_end": total - end,
            }),
        ))
    }

    pub fn samples_per_block(&self) -> usize {
        (self.block_size - 2) * 2
    }
    pub fn row_bytes(&self) -> usize {
        self.block_size * self.channels
    }
    pub fn block_offset(&self, row: usize, channel: usize) -> usize {
        self.data_start + row * self.row_bytes() + channel * self.block_size
    }
    pub fn scale(&self, row: usize, channel: usize) -> u16 {
        be16(self.bytes, self.block_offset(row, channel))
    }
    /// Signed nibble `index` of a block, high nibble first.
    pub fn nibble(&self, row: usize, channel: usize, index: usize) -> (usize, bool, i32) {
        let at = self.block_offset(row, channel) + 2 + index / 2;
        let byte = self.bytes[at];
        let high = index.is_multiple_of(2);
        let raw = if high { byte >> 4 } else { byte & 0x0F } as i32;
        (at, high, if raw >= 8 { raw - 16 } else { raw })
    }
    pub fn decoded_samples(&self) -> u64 {
        u64::from(self.total_samples) * self.channels as u64
    }

    pub fn fields(&self) -> &[Value] {
        &self.fields
    }

    /// Header interpretation: every field, unknown range, loop and ending.
    pub fn report(&self, arithmetic: Arithmetic) -> Value {
        let (c0, c1) = coefficients(self.highpass, self.sample_rate);
        let (k0, k1) = fixed_coefficients(self.highpass, self.sample_rate);
        json!({
            "format": "cri_adx",
            "encoding_type": self.encoding, "version": self.version, "flags": self.flags,
            "encrypted": false,
            "channels": self.channels, "sample_rate": self.sample_rate,
            "total_samples": self.total_samples,
            "duration_seconds": f64::from(self.total_samples) / f64::from(self.sample_rate),
            "block_size": self.block_size, "samples_per_block": self.samples_per_block(),
            "block_documented_size": self.block_size == BLOCK_BYTES,
            "sample_bitdepth": self.bit_depth, "highpass_frequency": self.highpass,
            "copyright_offset": self.copyright_offset,
            "copyright_marker": {"offset": self.copyright_offset as usize - 2, "text": "(c)CRI"},
            "data_start": self.data_start, "block_rows": self.rows,
            "row_bytes": self.row_bytes(), "data_end": self.data_end,
            "interleave": "each row holds one block per channel in ascending channel order",
            "nibble_order": "high nibble first; signed -8..7",
            "coefficients": {
                "float": [c0, c1], "fixed_12": [k0, k1],
                "fixed_12_hex": [format!("0x{:04x}", k0 as u16), format!("0x{:04x}", k1 as u16)],
                "rule": "a = sqrt(2) - cos(2 pi highpass / rate), b = sqrt(2) - 1, c = (a - sqrt((a + b)(a - b))) / b; coefficients 2c and -c^2"
            },
            "arithmetic": {"name": arithmetic.name(), "rule": arithmetic.rule()},
            "loop": self.loop_with_lead_in(),
            "end": self.end_report,
            "trailing_bytes": self.trailing_bytes,
            "unknown_header_bytes": self.unknown,
            "file_bytes": self.bytes.len(),
        })
    }
}

impl Adx<'_> {
    /// The loop report plus whether the first `alignment_samples` frames
    /// decode to silence, as the declared reading of that field expects.
    fn loop_with_lead_in(&self) -> Value {
        let mut report = self.loop_report.clone();
        if let Some(n) = report["alignment_samples"].as_u64().filter(|&n| n > 0) {
            let n = (n as usize).min(self.total_samples as usize);
            let mut decoder = Decoder::new(self, Arithmetic::Fixed12);
            let mut lead = vec![];
            decoder.decode(0, n, &mut lead);
            report["alignment_lead_in_silent"] = json!(lead.iter().all(|&v| v == 0));
        }
        report
    }
}

/// Header bytes not covered by an interpreted field, as located ranges.
fn unknown_ranges(bytes: &[u8], covered: &mut [(usize, usize)], limit: usize) -> Vec<Value> {
    covered.sort();
    let mut out = vec![];
    let mut at = 0;
    let mut gaps = vec![];
    for &(s, e) in covered.iter() {
        if s > at && at < limit {
            gaps.push((at, s.min(limit)));
        }
        at = at.max(e);
    }
    if at < limit {
        gaps.push((at, limit));
    }
    for (s, e) in gaps {
        let range = &bytes[s..e];
        let nonzero: Vec<usize> = (s..e).filter(|&i| bytes[i] != 0).collect();
        let mut row = json!({"offset": s, "bytes": e - s, "nonzero_bytes": nonzero.len()});
        if range.len() <= 64 {
            row["hex"] = json!(hex(range));
        } else if let Some(&first) = nonzero.first() {
            row["first_nonzero_offset"] = json!(first);
            row["hex_from_first_nonzero"] = json!(hex(&bytes[first..e.min(first + 64)]));
        }
        out.push(row);
    }
    out
}

/// The end marker after the last data row: an observed `0x8001` word, a
/// 16-bit length and that many bytes. Returns the report and trailing bytes.
fn end_marker(bytes: &[u8], data_end: usize) -> (Value, usize) {
    let rest = bytes.len() - data_end;
    if rest < 4 || be16(bytes, data_end) != END_SCALE {
        let note = if rest == 0 {
            "the file ends at the last data row"
        } else {
            "no 0x8001 end marker after the last data row"
        };
        return (
            json!({"marker": null, "offset": data_end, "note": note, "bytes_after_data": rest,
                   "first_bytes_hex": hex(&bytes[data_end..bytes.len().min(data_end + 16)])}),
            rest,
        );
    }
    let declared = be16(bytes, data_end + 2) as usize;
    let extent = (data_end + 4 + declared).min(bytes.len());
    let padding = &bytes[(data_end + 4).min(bytes.len())..extent];
    let nonzero = padding.iter().filter(|&&b| b != 0).count();
    let trailing = bytes.len() - extent;
    (
        json!({
            "marker": "0x8001", "offset": data_end, "declared_padding_bytes": declared,
            "padding_present": padding.len(), "padding_nonzero_bytes": nonzero,
            "end": extent, "truncated": data_end + 4 + declared > bytes.len(),
            "authority": "observed in every corpus file, not described in the cited documentation; decoding is bounded by total_samples",
        }),
        trailing,
    )
}

/// One decoded value with its stored coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub byte: usize,
    pub high_nibble: bool,
    pub nibble: i32,
    pub scale: u16,
    pub prediction: i64,
    /// Before clamping.
    pub sum: i64,
    pub output: i16,
}

/// Per-channel decoder state over one stream.
pub struct Decoder<'s, 'a> {
    adx: &'s Adx<'a>,
    arithmetic: Arithmetic,
    fixed: (i64, i64),
    float: (f64, f64),
    pub history: Vec<[i64; 2]>,
    pub clamped: u64,
}

impl<'s, 'a> Decoder<'s, 'a> {
    pub fn new(adx: &'s Adx<'a>, arithmetic: Arithmetic) -> Self {
        let (k0, k1) = fixed_coefficients(adx.highpass, adx.sample_rate);
        Decoder {
            adx,
            arithmetic,
            fixed: (i64::from(k0), i64::from(k1)),
            float: coefficients(adx.highpass, adx.sample_rate),
            history: vec![[0, 0]; adx.channels],
            clamped: 0,
        }
    }

    /// Decodes sample `frame` of `channel` with the current history.
    pub fn step(&mut self, frame: usize, channel: usize) -> Step {
        let spb = self.adx.samples_per_block();
        let (row, index) = (frame / spb, frame % spb);
        let scale = self.adx.scale(row, channel);
        let (byte, high_nibble, nibble) = self.adx.nibble(row, channel, index);
        let [h1, h2] = self.history[channel];
        let error = i64::from(nibble) * i64::from(scale);
        let (prediction, sum, history) = match self.arithmetic {
            Arithmetic::Fixed12 => {
                let p = (self.fixed.0 * h1 + self.fixed.1 * h2) >> FIXED_BITS;
                let s = error + p;
                (p, s, s.clamp(-32768, 32767))
            }
            Arithmetic::FloatReference => {
                let p = (self.float.0 * h1 as f64 + self.float.1 * h2 as f64) as i64;
                let s = error + p;
                (p, s, s)
            }
        };
        let output = sum.clamp(-32768, 32767) as i16;
        if i64::from(output) != sum {
            self.clamped += 1;
        }
        self.history[channel] = [history, h1];
        Step {
            byte,
            high_nibble,
            nibble,
            scale,
            prediction,
            sum,
            output,
        }
    }

    /// Appends frames `start..end`, interleaved, continuing the history.
    pub fn decode(&mut self, start: usize, end: usize, out: &mut Vec<i16>) {
        out.reserve((end - start) * self.adx.channels);
        for frame in start..end {
            for c in 0..self.adx.channels {
                out.push(self.step(frame, c).output);
            }
        }
    }
}

/// Every sample of the stream once from zero history, interleaved.
pub fn decode_stream(adx: &Adx, arithmetic: Arithmetic) -> Result<(Vec<i16>, u64), String> {
    check_budget(adx.decoded_samples())?;
    let mut decoder = Decoder::new(adx, arithmetic);
    let mut out = vec![];
    decoder.decode(0, adx.total_samples as usize, &mut out);
    Ok((out, decoder.clamped))
}

pub fn check_budget(samples: u64) -> Result<(), String> {
    if samples > MAX_DECODED_SAMPLES {
        return Err(format!(
            "{samples} decoded samples exceed the {MAX_DECODED_SAMPLES}-sample limit of one decode; choose a frames span"
        ));
    }
    Ok(())
}

/// A loop-aware render: the stream to the loop end, then `passes - 1`
/// further traversals from the loop start with carried history, and the
/// re-entry check comparing the second traversal with the first.
pub fn render_loop(
    adx: &Adx,
    arithmetic: Arithmetic,
    passes: u32,
) -> Result<(Vec<i16>, Value, u64), String> {
    let Some(l) = adx.looping else {
        return Err("the stream declares no loop; render the stream span".into());
    };
    let (ls, le) = (l.start_sample as usize, l.end_sample as usize);
    let ch = adx.channels;
    let passes = passes.max(1);
    let frames = le as u64 + u64::from(passes - 1) * (le - ls) as u64;
    check_budget(frames * ch as u64)?;
    let mut decoder = Decoder::new(adx, arithmetic);
    let mut out = vec![];
    decoder.decode(0, ls, &mut out);
    let entry_history = decoder.history.clone();
    decoder.decode(ls, le, &mut out);
    let reentry_history = decoder.history.clone();
    let clamped_first_pass = decoder.clamped;
    let mut second = vec![];
    decoder.decode(ls, le, &mut second);
    let traversal = reentry_report(&out[ls * ch..le * ch], &second, ch, &out, ls, le);
    let clamped = if passes == 1 {
        clamped_first_pass
    } else {
        out.extend_from_slice(&second);
        for _ in 2..passes {
            decoder.decode(ls, le, &mut out);
        }
        decoder.clamped
    };
    let rows = |h: &[[i64; 2]]| h.iter().map(|h| h.to_vec()).collect::<Vec<_>>();
    let report = json!({
        "loop_start": ls, "loop_end_exclusive": le, "passes": passes, "frames": frames,
        "reentry_check": {
            "first_entry_history": rows(&entry_history),
            "reentry_history": rows(&reentry_history),
            "history_equal": entry_history == reentry_history,
            "second_traversal": traversal,
            "rule": "after frame loop_end - 1 decoding continues at loop_start with each channel's history carried; the second traversal is decoded and compared with the first, whether or not passes includes it",
        },
    });
    Ok((out, report, clamped))
}

fn reentry_report(
    first: &[i16],
    second: &[i16],
    ch: usize,
    stream: &[i16],
    ls: usize,
    le: usize,
) -> Value {
    let frames = le - ls;
    let mut per_channel = vec![];
    for c in 0..ch {
        let diff: Vec<usize> = (0..frames)
            .filter(|&i| first[i * ch + c] != second[i * ch + c])
            .collect();
        let max = (0..frames)
            .map(|i| (i32::from(first[i * ch + c]) - i32::from(second[i * ch + c])).abs())
            .max()
            .unwrap_or(0);
        let last = stream[(le - 1) * ch + c];
        let seam = (i32::from(second[c]) - i32::from(last)).abs();
        let entry_step =
            (ls > 0).then(|| (i32::from(first[c]) - i32::from(stream[(ls - 1) * ch + c])).abs());
        per_channel.push(json!({
            "channel": c,
            "differing_frames": diff.len(),
            "max_abs_difference": max,
            "first_differing_frame": diff.first().map(|i| ls + i),
            "last_differing_frame": diff.last().map(|i| ls + i),
            "identical_from_frame": diff.last().map_or(Some(ls), |&i| (i + 1 < frames).then_some(ls + i + 1)),
            "seam_step": seam,
            "first_entry_step": entry_step,
        }));
    }
    json!({"frames": frames, "channels": per_channel})
}

/// Parameters of a written stream (version 3 header, encoding 3, 18-byte blocks).
#[derive(Debug, Clone, Copy)]
pub struct EncodeParams {
    pub sample_rate: u32,
    pub highpass: u16,
    /// Start (a multiple of 32) and exclusive end, in frames.
    pub looping: Option<(u32, u32)>,
    /// Align the loop-start byte and the file length to this many bytes.
    pub sector_bytes: Option<u32>,
    /// Loop alignment field (0x14): lead-in frames the caller prepended.
    pub alignment_samples: u16,
}

/// Encodes per-channel PCM16 into a complete ADX file.
pub fn encode(channels: &[Vec<i16>], p: EncodeParams) -> Result<Vec<u8>, String> {
    let ch = channels.len();
    if ch == 0 || ch > MAX_CHANNELS as usize {
        return Err(format!("{ch} channels; 1..{MAX_CHANNELS} are written"));
    }
    let frames = channels[0].len();
    if frames == 0 || frames > u32::MAX as usize || channels.iter().any(|c| c.len() != frames) {
        return Err("channels must have the same nonzero length".into());
    }
    let rows = frames.div_ceil(BLOCK_SAMPLES);
    let row_bytes = BLOCK_BYTES * ch;
    let mut copyright_offset = if p.looping.is_some() {
        LOOP_COPYRIGHT_OFFSET as usize
    } else {
        SHORT_COPYRIGHT_OFFSET as usize
    };
    if let (Some((start, _)), Some(sector)) = (p.looping, p.sector_bytes) {
        let sector = sector as usize;
        let loop_byte = copyright_offset + 4 + (start as usize / BLOCK_SAMPLES) * row_bytes;
        copyright_offset += (sector - loop_byte % sector) % sector;
    }
    if copyright_offset > u16::MAX as usize {
        return Err(format!(
            "the aligned header needs copyright offset {copyright_offset}, beyond 65535"
        ));
    }
    let data_start = copyright_offset + 4;
    let mut out = vec![0u8; data_start];
    out[0..2].copy_from_slice(&SIGNATURE.to_be_bytes());
    out[2..4].copy_from_slice(&(copyright_offset as u16).to_be_bytes());
    out[4] = STANDARD_ENCODING;
    out[5] = BLOCK_BYTES as u8;
    out[6] = 4;
    out[7] = ch as u8;
    out[8..12].copy_from_slice(&p.sample_rate.to_be_bytes());
    out[12..16].copy_from_slice(&(frames as u32).to_be_bytes());
    out[16..18].copy_from_slice(&p.highpass.to_be_bytes());
    out[18] = 3;
    out[19] = 0;
    if let Some((start, end)) = p.looping {
        let start_byte = data_start + (start as usize / BLOCK_SAMPLES) * row_bytes;
        let end_byte = data_start + (end as usize).div_ceil(BLOCK_SAMPLES) * row_bytes;
        out[0x14..0x16].copy_from_slice(&p.alignment_samples.to_be_bytes());
        out[0x16..0x18].copy_from_slice(&1u16.to_be_bytes());
        out[0x18..0x1C].copy_from_slice(&1u32.to_be_bytes());
        out[0x1C..0x20].copy_from_slice(&start.to_be_bytes());
        out[0x20..0x24].copy_from_slice(&(start_byte as u32).to_be_bytes());
        out[0x24..0x28].copy_from_slice(&end.to_be_bytes());
        out[0x28..0x2C].copy_from_slice(&(end_byte as u32).to_be_bytes());
    }
    out[copyright_offset - 2..data_start].copy_from_slice(COPYRIGHT);
    let (k0, k1) = fixed_coefficients(p.highpass, p.sample_rate);
    let k = (i64::from(k0), i64::from(k1));
    let mut history = vec![[0i64; 2]; ch];
    for row in 0..rows {
        for (c, pcm) in channels.iter().enumerate() {
            let start = row * BLOCK_SAMPLES;
            let target: Vec<i64> = pcm[start..(start + BLOCK_SAMPLES).min(frames)]
                .iter()
                .map(|&v| i64::from(v))
                .collect();
            let (scale, nibbles, next) = encode_block(&target, history[c], k);
            history[c] = next;
            out.extend(scale.to_be_bytes());
            for pair in nibbles.chunks(2) {
                out.push((((pair[0] & 0x0F) << 4) | (pair[1] & 0x0F)) as u8);
            }
        }
    }
    let data_end = out.len();
    let padding = match p.sector_bytes {
        Some(sector) => {
            let sector = sector as usize;
            (sector - (data_end + 4) % sector) % sector
        }
        None => BLOCK_BYTES - 4,
    };
    if padding > u16::MAX as usize {
        return Err("the end padding exceeds 65535 bytes".into());
    }
    out.extend(END_SCALE.to_be_bytes());
    out.extend((padding as u16).to_be_bytes());
    out.resize(out.len() + padding, 0);
    Ok(out)
}

/// Decodes a candidate block; returns squared error, nibbles and history.
fn trial(target: &[i64], h: [i64; 2], k: (i64, i64), scale: i64) -> (i64, [i8; 32], [i64; 2]) {
    let [mut h1, mut h2] = h;
    let mut nibbles = [0i8; 32];
    let mut err = 0;
    for (i, &x) in target.iter().enumerate() {
        let p = (k.0 * h1 + k.1 * h2) >> FIXED_BITS;
        let n = if scale == 0 {
            0
        } else {
            let r = x - p;
            let lo = r.div_euclid(scale).clamp(-8, 7);
            let hi = (lo + 1).clamp(-8, 7);
            let y = |n: i64| (n * scale + p).clamp(-32768, 32767);
            if (y(hi) - x).abs() < (y(lo) - x).abs() {
                hi
            } else {
                lo
            }
        };
        let y = (n * scale + p).clamp(-32768, 32767);
        err += (y - x) * (y - x);
        nibbles[i] = n as i8;
        h2 = h1;
        h1 = y;
    }
    (err, nibbles, [h1, h2])
}

/// The deterministic block search: candidate scales around the largest
/// prediction residual / 7, each evaluated closed-loop; least squared error
/// wins, ties to the smaller scale.
fn encode_block(target: &[i64], h: [i64; 2], k: (i64, i64)) -> (u16, [i8; 32], [i64; 2]) {
    let [mut h1, mut h2] = h;
    let mut residual = 0i64;
    for &x in target {
        let p = (k.0 * h1 + k.1 * h2) >> FIXED_BITS;
        residual = residual.max((x - p).abs());
        h2 = h1;
        h1 = x;
    }
    let base = (residual + 6) / 7;
    let mut candidates: Vec<i64> = vec![0, 1, base - 1, base, base + 1];
    for (num, den) in [
        (1, 2),
        (5, 8),
        (3, 4),
        (7, 8),
        (9, 8),
        (5, 4),
        (3, 2),
        (2, 1),
    ] {
        candidates.push(base * num / den);
    }
    // Every scale from 5/8 to 9/8 of the estimate, or 96 evenly spaced ones.
    let (lo, hi) = (base * 5 / 8, base * 9 / 8 + 1);
    let stride = ((hi - lo) / 96).max(1);
    candidates.extend((lo..=hi).step_by(stride as usize));
    let candidates: std::collections::BTreeSet<i64> = candidates
        .into_iter()
        .map(|s| s.clamp(0, i64::from(MAX_SCALE)))
        .collect();
    let mut best: Option<(i64, i64, [i8; 32], [i64; 2])> = None;
    for scale in candidates {
        let (err, nibbles, next) = trial(target, h, k, scale);
        if best.as_ref().is_none_or(|b| err < b.0) {
            best = Some((err, scale, nibbles, next));
        }
    }
    let (_, scale, nibbles, next) = best.expect("at least one candidate");
    (scale as u16, nibbles, next)
}

/// The encoder's search rule, for reports and capabilities.
pub const SEARCH_RULE: &str = "per block and channel: candidate scales 0, 1, ceil(r / 7) x {1/2, 5/8, 3/4, 7/8, 1, 9/8, 5/4, 3/2, 2} and +-1, and every scale from 5/8 to 9/8 of ceil(r / 7) (at most 97, evenly strided), where r is the largest residual against the prediction from the ideal signal, limited to 0..0x1FFF; each nibble is the nearer of the two values bracketing the residual (ties to the lower); each candidate decodes closed-loop with fixed_12 arithmetic; least squared error wins, ties to the smaller scale";

/// The ranking model, for reports and capabilities.
pub const LOOP_RANK_RULE: &str = "per channel, d1 = x[end-1] - x[start-1] and d2 = x[end-2] - x[start-2] (zero before frame 0) are the decoder-history differences between re-entry and first entry; the predicted re-entry difference is the peak of e[n] = c0 e[n-1] + c1 e[n-2] from e[-1] = d1, e[-2] = d2 over 512 frames with the derived coefficients, the largest over channels; settle frames are where it stays below 1; ranked by predicted difference, then |d1| (seam value) and |d1 - d2| (seam slope), then earlier start and end. The source PCM stands in for the decoded values; ADPCM quantization and clamping are not modelled";

/// Loop-point candidates one ranking may evaluate (start x end pairs).
pub const MAX_LOOP_PAIRS: u64 = 1_000_000;
/// Predicted-transient terms evaluated per pair.
const PREDICTION_TERMS: usize = 512;
/// Frames searched for the settle point of a reported candidate.
const SETTLE_LIMIT: usize = 8192;

/// One ranked loop: 32-frame-aligned start, exclusive end and its predictions.
#[derive(Debug, Clone)]
pub struct LoopCandidate {
    pub start: usize,
    pub end: usize,
    /// Largest predicted re-entry difference over channels.
    pub predicted_reentry: f64,
    /// Largest |x[end-1] - x[start-1]| over channels: the value the loop
    /// jump changes.
    pub seam_value: f64,
    /// Largest |(x[end-1] - x[end-2]) - (x[start-1] - x[start-2])|: the
    /// slope the loop jump changes.
    pub seam_slope: f64,
}

/// Coefficient pair of the history-mismatch response: e[n] = a[n] d1 + b[n] d2
/// for history differences d1 (last frame) and d2 (the frame before).
fn mismatch_response(highpass: u16, rate: u32, terms: usize) -> Vec<(f64, f64)> {
    let (c0, c1) = coefficients(highpass, rate);
    // e[-1] = d1, e[-2] = d2; e[n] = c0 e[n-1] + c1 e[n-2].
    let (mut p1, mut p2) = ((1.0, 0.0), (0.0, 1.0));
    (0..terms)
        .map(|_| {
            let e = (c0 * p1.0 + c1 * p2.0, c0 * p1.1 + c1 * p2.1);
            p2 = p1;
            p1 = e;
            e
        })
        .collect()
}

fn at(pcm: &[i16], i: isize) -> f64 {
    if i < 0 {
        0.0
    } else {
        f64::from(pcm[i as usize])
    }
}

/// History differences between re-entry (frames end-1, end-2) and first entry
/// (frames start-1, start-2, zero before the stream) for one channel.
fn mismatch(pcm: &[i16], start: usize, end: usize) -> (f64, f64) {
    let (s, e) = (start as isize, end as isize);
    (
        at(pcm, e - 1) - at(pcm, s - 1),
        at(pcm, e - 2) - at(pcm, s - 2),
    )
}

/// Ranks loop points of per-channel PCM by the predicted re-entry
/// difference: the decoder-history mismatch (frames end-1, end-2 against
/// start-1, start-2) propagated through the predictor, ignoring quantization
/// and clamping. Ties go to the smaller seam value and slope change, then the
/// earlier start and end. Returns the best `limit` candidates and the pairs
/// evaluated.
pub fn rank_loops(
    channels: &[Vec<i16>],
    highpass: u16,
    rate: u32,
    starts: &[usize],
    ends: &[usize],
    min_length: usize,
    limit: usize,
) -> (Vec<LoopCandidate>, u64) {
    let response = mismatch_response(highpass, rate, PREDICTION_TERMS);
    let mut best: Vec<LoopCandidate> = vec![];
    let mut evaluated = 0u64;
    for &start in starts {
        for &end in ends {
            if end < start + min_length.max(1) {
                continue;
            }
            evaluated += 1;
            let (mut predicted, mut seam_value, mut seam_slope) = (0f64, 0f64, 0f64);
            for pcm in channels {
                let (d1, d2) = mismatch(pcm, start, end);
                if d1 != 0.0 || d2 != 0.0 {
                    let peak = response
                        .iter()
                        .map(|&(a, b)| (a * d1 + b * d2).abs())
                        .fold(0.0, f64::max);
                    predicted = predicted.max(peak);
                }
                seam_value = seam_value.max(d1.abs());
                seam_slope = seam_slope.max((d1 - d2).abs());
            }
            let candidate = LoopCandidate {
                start,
                end,
                predicted_reentry: predicted,
                seam_value,
                seam_slope,
            };
            let key = |c: &LoopCandidate| {
                (
                    c.predicted_reentry,
                    c.seam_value,
                    c.seam_slope,
                    c.start,
                    c.end,
                )
            };
            let position = best
                .iter()
                .position(|b| {
                    key(&candidate).partial_cmp(&key(b)) == Some(std::cmp::Ordering::Less)
                })
                .unwrap_or(best.len());
            if position < limit {
                best.insert(position, candidate);
                best.truncate(limit);
            }
        }
    }
    (best, evaluated)
}

/// Frames after the loop start until the predicted difference stays below
/// one unit, within [`SETTLE_LIMIT`].
pub fn predicted_settle(
    channels: &[Vec<i16>],
    highpass: u16,
    rate: u32,
    start: usize,
    end: usize,
) -> usize {
    let response = mismatch_response(highpass, rate, SETTLE_LIMIT);
    channels
        .iter()
        .map(|pcm| {
            let (d1, d2) = mismatch(pcm, start, end);
            response
                .iter()
                .rposition(|&(a, b)| (a * d1 + b * d2).abs() >= 1.0)
                .map_or(0, |n| n + 1)
        })
        .max()
        .unwrap_or(0)
}

/// A PCM16 WAV of interleaved samples, with an optional forward `smpl` loop
/// (start, inclusive end).
pub fn pcm16_wav(
    samples: &[i16],
    channels: usize,
    rate: u32,
    smpl_loop: Option<(u32, u32)>,
) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut extra = vec![];
    if let Some((start, end)) = smpl_loop {
        let period = (1e9 / f64::from(rate)).round() as u32;
        let mut body = Vec::with_capacity(60);
        for v in [0, 0, period, 60, 0, 0, 0, 1, 0, 0, 0, start, end, 0, 0] {
            body.extend(u32::to_le_bytes(v));
        }
        extra.push((b"smpl", body));
    }
    crate::wav::build(1, channels as u16, rate, 16, None, &data, &extra)
}

/// The `adx_streams` capability section.
pub fn capability() -> Value {
    json!({
        "tools": ["load_instrument_source", "inspect_adx", "render_adx", "encode_adx", "export_instrument_source"],
        "retention": "load_instrument_source keeps every byte and reports the ADX header in discovery; export_instrument_source returns the exact original whether or not it is admitted",
        "admitted": "signature 0x8000, (c)CRI at copyright offset - 2, encoding type 3 (standard), 4-bit samples, block size 3..255 (18 written and observed), 1..16 channels, 1..192000 Hz, versions 3 and 4 (loop fields) and 5 (no loop), flags 0",
        "refused": "encoding types 2 (preset coefficients), 4 (exponential scale), 0x10/0x11 (AHX) and others; flags 0x08 (encryption, no key recovery) and any other nonzero flags; scale words above 0x1FFF; truncated data; malformed or inconsistent loop fields; all refusals name the byte",
        "header": "every field with offset, size, raw bytes and value; header bytes no field covers are reported as unknown ranges with offsets",
        "blocks": "rows of one block per channel in ascending order; 2-byte big-endian scale then 4-bit codes, high nibble first, signed -8..7",
        "coefficients": "a = sqrt(2) - cos(2 pi highpass / rate), b = sqrt(2) - 1, c = (a - sqrt((a + b)(a - b))) / b; 2c and -c^2",
        "arithmetic": {"fixed_12": Arithmetic::Fixed12.rule(), "float_reference": Arithmetic::FloatReference.rule(), "default": "fixed_12"},
        "loops": "version 3 fields at 0x14..0x2C and version 4 at 0x24..0x38, read only where they end before the (c)CRI marker; the end sample is exclusive (declared reading); byte offsets are checked against the observed block relation and reported; loop alignment samples are read as the silent lead-in prepended to align the loop start (declared; alignment_lead_in_silent checks it); render_adx loop passes carry history across the jump and compare the second traversal with the first",
        "end": "the 0x8001 end word, its 16-bit padding length and the padding are reported (observed, not documented); decoding stops at total_samples",
        "render": "PCM16 WAV at the header rate with an smpl loop (end inclusive = loop end - 1) for the stream span, the exact original and render.json; spans stream, frames, loop with 1..16 passes; decoded.wav is retained as decoded_source with ready encode_adx (and, for one channel up to 8388608 frames, prepare_sample) arguments",
        "encode": {"input": "retained PCM16 integer WAV, 1..16 channels", "header": "version 3, encoding 3, 18-byte blocks, flags 0; loop alignment field = prepended lead-in frames (0 without loop_start_padding)", "loop": "start on a 32-frame boundary, or any start with loop_start_padding (silent lead-in, lead_in_padding); exclusive end", "search": SEARCH_RULE, "changes": ["adpcm_quantization", "wav_metadata_not_carried", "lead_in_padding"]},
        "loop_ranking": {"tool": "rank_adx_loops", "model": LOOP_RANK_RULE, "limits": format!("at most {MAX_LOOP_PAIRS} start x end pairs; 1..50 candidates"), "applies": "nothing; pass loop_frames to encode_adx and verify loop_reentry"},
        "limits": format!("{MAX_DECODED_SAMPLES} decoded samples per call; no CD streaming, AHX, encrypted or type 2/4 decoding, and no console playback or mixer model"),
        "authority": "public format descriptions; fixed_12 is a declared reading that matches a black-box FFmpeg decode of the corpus; hardware and CRI library behavior are not established",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, period: f64, amplitude: f64, phase: f64) -> Vec<i16> {
        (0..frames)
            .map(|i| ((i as f64 * 2.0 * PI / period + phase).sin() * amplitude).round() as i16)
            .collect()
    }

    fn params(looping: Option<(u32, u32)>) -> EncodeParams {
        EncodeParams {
            sample_rate: 22050,
            highpass: 500,
            looping,
            sector_bytes: None,
            alignment_samples: 0,
        }
    }

    #[test]
    fn coefficients_follow_the_described_formula() {
        let (c0, c1) = coefficients(500, 22050);
        let a = SQRT_2 - (2.0 * PI * 500.0 / 22050.0).cos();
        let b = SQRT_2 - 1.0;
        let c = (a - (a * a - b * b).sqrt()) / b;
        assert!((c0 - 2.0 * c).abs() < 1e-12 && (c1 + c * c).abs() < 1e-12);
        let (k0, k1) = fixed_coefficients(500, 22050);
        assert_eq!(
            (k0, k1),
            ((c0 * 4096.0).round() as i32, (c1 * 4096.0).round() as i32)
        );
        assert_eq!((k0, k1), (6569, -2634));
    }

    /// A hand-built mono stream: header fields, nibble order, scale, history.
    #[test]
    fn hand_built_block_decodes_with_the_fixed_equation() {
        let mut b = vec![0u8; 36];
        b[0] = 0x80;
        b[2..4].copy_from_slice(&32u16.to_be_bytes());
        b[4] = 3;
        b[5] = 18;
        b[6] = 4;
        b[7] = 1;
        b[8..12].copy_from_slice(&22050u32.to_be_bytes());
        b[12..16].copy_from_slice(&3u32.to_be_bytes());
        b[16..18].copy_from_slice(&500u16.to_be_bytes());
        b[18] = 3;
        b[30..36].copy_from_slice(COPYRIGHT);
        b.extend(100u16.to_be_bytes());
        b.extend([0x7F, 0x80]);
        b.extend([0u8; 14]);
        b.extend([0x80, 0x01, 0, 14]);
        b.extend([0u8; 14]);
        let adx = Adx::parse(&b).unwrap();
        assert_eq!((adx.data_start, adx.rows, adx.trailing_bytes), (36, 1, 0));
        assert!(adx.looping.is_none());
        let (k0, k1) = fixed_coefficients(500, 22050);
        let s0 = 700i64;
        let s1 = -100 + ((i64::from(k0) * s0) >> 12);
        let s2 = -800 + ((i64::from(k0) * s1 + i64::from(k1) * s0) >> 12);
        let (pcm, clamped) = decode_stream(&adx, Arithmetic::Fixed12).unwrap();
        assert_eq!(pcm, vec![s0 as i16, s1 as i16, s2 as i16]);
        assert_eq!(clamped, 0);
        let report = adx.report(Arithmetic::Fixed12);
        assert_eq!(report["end"]["marker"], "0x8001");
        assert_eq!(report["unknown_header_bytes"][0]["offset"], 0x1C);
        assert_eq!(report["loop"]["state"], "disabled");
    }

    #[test]
    fn refusals_name_their_byte() {
        let pcm = tone(100, 20.0, 8000.0, 0.0);
        let good = encode(&[pcm], params(None)).unwrap();
        let cases: Vec<(usize, u8, usize, &str)> = vec![
            (0, 0x81, 0, "signature"),
            (4, 4, 4, "encoding_type"),
            (4, 0x11, 4, "encoding_type"),
            (19, 0x08, 19, "flags"),
            (19, 0x01, 19, "flags"),
            (6, 8, 6, "sample_bitdepth"),
            (7, 0, 7, "channel_count"),
            (18, 9, 18, "version"),
            (30, b'x', 30, "copyright"),
            (36, 0x20, 36, "scale"),
        ];
        for (at, value, offset, field) in cases {
            let mut b = good.clone();
            b[at] = value;
            let e = Adx::parse(&b).unwrap_err();
            assert_eq!((e.offset, e.field), (offset, field), "{e:?}");
        }
        let e = Adx::parse(&good[..60]).unwrap_err();
        assert_eq!(e.field, "data");
    }

    #[test]
    fn encoder_roundtrip_is_bounded_and_deterministic() {
        let left = tone(1000, 37.3, 12000.0, 0.0);
        let right = tone(1000, 11.1, 4000.0, 1.0);
        let bytes = encode(&[left.clone(), right.clone()], params(None)).unwrap();
        assert_eq!(
            bytes,
            encode(&[left.clone(), right.clone()], params(None)).unwrap()
        );
        let adx = Adx::parse(&bytes).unwrap();
        assert_eq!((adx.channels, adx.total_samples, adx.rows), (2, 1000, 32));
        assert_eq!(adx.trailing_bytes, 0);
        let (pcm, _) = decode_stream(&adx, Arithmetic::Fixed12).unwrap();
        for (c, source) in [&left, &right].into_iter().enumerate() {
            let err: f64 = source
                .iter()
                .enumerate()
                .map(|(i, &x)| (f64::from(x) - f64::from(pcm[i * 2 + c])).powi(2))
                .sum::<f64>();
            let sig: f64 = source.iter().map(|&x| f64::from(x).powi(2)).sum();
            assert!(10.0 * (sig / err).log10() > 20.0, "channel {c}");
        }
        let silence = encode(&[vec![0; 64]], params(None)).unwrap();
        let (pcm, _) = decode_stream(&Adx::parse(&silence).unwrap(), Arithmetic::Fixed12).unwrap();
        assert!(pcm.iter().all(|&v| v == 0));
    }

    #[test]
    fn loop_fields_and_sector_alignment_read_back() {
        let pcm = tone(5000, 64.0, 9000.0, 0.0);
        let p = EncodeParams {
            sector_bytes: Some(2048),
            alignment_samples: 12,
            ..params(Some((1024, 4096)))
        };
        let bytes = encode(&[pcm], p).unwrap();
        assert_eq!(bytes.len() % 2048, 0);
        let adx = Adx::parse(&bytes).unwrap();
        let l = adx.looping.unwrap();
        assert_eq!((l.start_sample, l.end_sample), (1024, 4096));
        assert_eq!(l.start_byte % 2048, 0);
        let report = adx.report(Arithmetic::Fixed12);
        assert_eq!(report["loop"]["start_byte_consistent"], true);
        assert_eq!(report["loop"]["end_byte_consistent"], true);
        assert_eq!(report["loop"]["alignment_samples"], 12);
        assert_eq!(report["loop"]["alignment_lead_in_silent"], false);
        let (out, check, _) = render_loop(&adx, Arithmetic::Fixed12, 3).unwrap();
        assert_eq!(out.len(), 4096 + 2 * 3072);
        let second = &check["reentry_check"]["second_traversal"]["channels"][0];
        // A whole-period loop of a steady tone re-enters close to the first pass.
        assert!(
            second["max_abs_difference"].as_i64().unwrap() < 2000,
            "{second}"
        );
        let (stream, _) = decode_stream(&adx, Arithmetic::Fixed12).unwrap();
        assert_eq!(&out[..4096], &stream[..4096]);
    }

    #[test]
    fn ranking_predicts_the_reentry_difference_of_an_encoded_loop() {
        // A cosine with period 64: loop ends at whole periods match the entry history.
        let pcm = tone(64 * 40, 64.0, 12000.0, PI / 2.0);
        let starts: Vec<usize> = (0..=1024).step_by(32).collect();
        let ends: Vec<usize> = (2000..=2560).collect();
        let (ranked, evaluated) =
            rank_loops(std::slice::from_ref(&pcm), 500, 22050, &starts, &ends, 0, 5);
        assert_eq!(evaluated, (starts.len() * ends.len()) as u64);
        assert!(
            ranked[0].start > 0 && (ranked[0].end - ranked[0].start).is_multiple_of(64),
            "{ranked:?}"
        );
        assert!(ranked[0].predicted_reentry < 1.0, "{:?}", ranked[0]);
        // Start 0 against the full length: zero entry history, loud end.
        let (whole, _) = rank_loops(std::slice::from_ref(&pcm), 500, 22050, &[0], &[2560], 0, 1);
        let p = EncodeParams {
            looping: Some((0, 2560)),
            ..params(None)
        };
        let adx = encode(&[pcm[..2560].to_vec()], p).unwrap();
        let (_, report, _) =
            render_loop(&Adx::parse(&adx).unwrap(), Arithmetic::Fixed12, 2).unwrap();
        let measured =
            report["reentry_check"]["second_traversal"]["channels"][0]["max_abs_difference"]
                .as_f64()
                .unwrap();
        let predicted = whole[0].predicted_reentry;
        assert!(
            (predicted - measured).abs() < 0.05 * measured + 50.0,
            "{predicted} vs {measured}"
        );
        assert!(predicted_settle(&[pcm], 500, 22050, 0, 2560) > 10);
    }

    #[test]
    fn float_reference_keeps_unclamped_history() {
        // Large scales overflow the 16-bit range; the readings then diverge.
        let mut b = encode(&[vec![0; 64]], params(None)).unwrap();
        let start = 36;
        b[start..start + 2].copy_from_slice(&0x1FFFu16.to_be_bytes());
        for i in 0..16 {
            b[start + 2 + i] = 0x77;
        }
        let adx = Adx::parse(&b).unwrap();
        let (fixed, clamped) = decode_stream(&adx, Arithmetic::Fixed12).unwrap();
        let (float, _) = decode_stream(&adx, Arithmetic::FloatReference).unwrap();
        assert!(clamped > 0);
        assert_eq!(fixed[0], float[0]);
        assert_ne!(fixed, float);
    }
}
