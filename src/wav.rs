//! Bounded RIFF/WAVE structure shared by sample conversion and measurement.
//!
//! Integer PCM 8/16/24/32-bit and IEEE float 32/64, plain or carried by
//! WAVE_FORMAT_EXTENSIBLE, any channel count up to [`MAX_CHANNELS`]. Both
//! features accept and refuse the same files here; each applies its own limits
//! and value scaling on top ([`crate::sample_wav`], [`crate::measure::wav`]).
//! Unknown chunks are listed, never interpreted; the caller retains the bytes.
use serde_json::{Value, json};

pub const MAX_CHANNELS: u16 = 32;
pub const MAX_CHUNKS: usize = 1024;
pub const MAX_SMPL_LOOPS: u32 = 256;
/// Bytes 4..16 shared by the KSDATAFORMAT_SUBTYPE_* GUIDs.
pub const SUBTYPE_TAIL: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Int,
    Float,
}

/// A malformed or unsupported file. `offset` and `field` locate the defect;
/// the message is the human sentence, which names the chunk and byte too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavError {
    pub offset: usize,
    pub field: &'static str,
    pub message: String,
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn error(offset: usize, field: &'static str, message: impl Into<String>) -> WavError {
    WavError {
        offset,
        field,
        message: message.into(),
    }
}

/// The refusal of a sample whose value is not finite after scaling.
pub fn non_finite_sample(at: usize, frame: usize, channel: usize) -> WavError {
    error(
        at,
        "data sample",
        format!("non-finite or out-of-range float at frame {frame}, channel {channel} (byte {at})"),
    )
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub id: [u8; 4],
    /// Byte offset of the chunk header within the file.
    pub offset: usize,
    pub size: u32,
}

#[derive(Debug, Clone)]
pub struct SmplLoop {
    pub cue_id: u32,
    pub kind: u32,
    pub start: u32,
    /// Inclusive last frame of the loop.
    pub end: u32,
    pub fraction: u32,
    pub play_count: u32,
}

#[derive(Debug, Clone)]
pub struct Smpl {
    pub offset: usize,
    pub manufacturer: u32,
    pub product: u32,
    pub sample_period_ns: u32,
    pub unity_note: u32,
    pub pitch_fraction: u32,
    pub loops: Vec<SmplLoop>,
    pub sampler_data_bytes: u32,
}

#[derive(Debug)]
pub struct WavFile<'a> {
    pub format_tag: u16,
    pub format: Format,
    pub extensible: bool,
    pub channels: u16,
    pub sample_rate: u32,
    pub byte_rate: u32,
    pub block_align: u16,
    pub bits: u16,
    pub valid_bits: Option<u16>,
    pub channel_mask: Option<u32>,
    /// Byte offset of the fmt chunk header within the file.
    pub fmt_offset: usize,
    /// Byte offset of the first sample within the file.
    pub data_offset: usize,
    pub data: &'a [u8],
    pub frames: usize,
    pub chunks: Vec<Chunk>,
    /// Absent, parsed, or malformed (information only unless opted in).
    pub smpl: Option<Result<Smpl, WavError>>,
    /// Bytes after the RIFF chunk; retained, not interpreted.
    pub trailing_bytes: usize,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn chunk_name(id: &[u8; 4]) -> String {
    if id.iter().all(|b| (0x20..0x7f).contains(b)) {
        id.iter().map(|&b| b as char).collect()
    } else {
        format!(
            "0x{}",
            id.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    }
}

/// One little-endian sample normalized to full scale 1.0: integer codes are
/// divided by 2^(bits-1), unsigned 8-bit after subtracting 128, floats are
/// kept. The width is the slice length. Exact in f64 for every format.
pub fn normalized(format: Format, s: &[u8]) -> f64 {
    match (format, s.len()) {
        (Format::Int, 1) => (f64::from(s[0]) - 128.0) / 128.0,
        (Format::Int, 2) => f64::from(i16::from_le_bytes([s[0], s[1]])) / 32_768.0,
        (Format::Int, 3) => f64::from(i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) / 8_388_608.0,
        (Format::Int, _) => {
            f64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])) / 2_147_483_648.0
        }
        (Format::Float, 4) => f64::from(f32::from_le_bytes([s[0], s[1], s[2], s[3]])),
        (Format::Float, _) => f64::from_le_bytes(s.try_into().expect("eight bytes")),
    }
}

struct Fmt {
    tag: u16,
    format: Format,
    extensible: bool,
    channels: u16,
    rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits: u16,
    valid_bits: Option<u16>,
    channel_mask: Option<u32>,
}

/// `at` is the chunk header; fields are located from the body at `at + 8`.
fn parse_fmt(body: &[u8], at: usize) -> Result<Fmt, WavError> {
    let field = at + 8;
    if body.len() < 16 {
        return Err(error(
            at + 4,
            "fmt size",
            format!(
                "fmt chunk at byte {at} has {} bytes; 16 are required",
                body.len()
            ),
        ));
    }
    let tag = u16_at(body, 0);
    let (channels, rate, byte_rate, block_align, bits) = (
        u16_at(body, 2),
        u32_at(body, 4),
        u32_at(body, 8),
        u16_at(body, 12),
        u16_at(body, 14),
    );
    let (code, code_at, extensible, valid_bits, channel_mask) = if tag == 0xFFFE {
        let short = || {
            format!(
                "WAVE_FORMAT_EXTENSIBLE fmt chunk at byte {at} needs 40 bytes with a 22-byte extension"
            )
        };
        if body.len() < 40 {
            return Err(error(at + 4, "fmt size", short()));
        }
        if u16_at(body, 16) < 22 {
            return Err(error(field + 16, "fmt extension size", short()));
        }
        if body[28..40] != SUBTYPE_TAIL || u16_at(body, 26) != 0 {
            return Err(error(
                field + 24,
                "fmt subformat",
                format!(
                    "WAVE_FORMAT_EXTENSIBLE subformat at byte {} is not a standard KSDATAFORMAT subtype",
                    field + 24
                ),
            ));
        }
        let valid = u16_at(body, 18);
        (
            u16_at(body, 24),
            field + 24,
            true,
            (valid != 0).then_some(valid),
            Some(u32_at(body, 20)),
        )
    } else {
        (tag, field, false, None, None)
    };
    let format = match code {
        1 => Format::Int,
        3 => Format::Float,
        _ => {
            return Err(error(
                code_at,
                "fmt audio format",
                format!(
                    "unsupported WAVE format 0x{code:04x} in fmt chunk at byte {at}; only integer PCM and IEEE float are decoded (no ADPCM or other compression)"
                ),
            ));
        }
    };
    let supported = match format {
        Format::Int => [8, 16, 24, 32].contains(&bits),
        Format::Float => [32, 64].contains(&bits),
    };
    if !supported {
        return Err(error(
            field + 14,
            "fmt bits per sample",
            format!(
                "unsupported {bits}-bit {} samples in fmt chunk at byte {at}; integer 8/16/24/32 or float 32/64 bits",
                if format == Format::Int {
                    "integer"
                } else {
                    "float"
                }
            ),
        ));
    }
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(error(
            field + 2,
            "fmt channels",
            if channels == 0 {
                format!("fmt chunk at byte {at} declares 0 channels")
            } else {
                format!(
                    "fmt chunk at byte {at} declares {channels} channels; at most {MAX_CHANNELS} are supported"
                )
            },
        ));
    }
    if rate == 0 {
        return Err(error(
            field + 4,
            "fmt sample rate",
            format!("fmt chunk at byte {at} declares a zero sample rate"),
        ));
    }
    if block_align as u32 != channels as u32 * bits as u32 / 8 {
        return Err(error(
            field + 12,
            "fmt block align",
            format!(
                "fmt chunk at byte {at}: block align {block_align} is not {channels} channels x {} bytes",
                bits / 8
            ),
        ));
    }
    if valid_bits.is_some_and(|v| v > bits) {
        return Err(error(
            field + 18,
            "fmt valid bits",
            format!("fmt chunk at byte {at}: valid bits exceed the {bits}-bit container"),
        ));
    }
    Ok(Fmt {
        tag,
        format,
        extensible,
        channels,
        rate,
        byte_rate,
        block_align,
        bits,
        valid_bits,
        channel_mask,
    })
}

fn parse_smpl(body: &[u8], at: usize) -> Result<Smpl, WavError> {
    if body.len() < 36 {
        return Err(error(
            at + 4,
            "smpl size",
            format!(
                "smpl chunk at byte {at} has {} bytes; 36 are required",
                body.len()
            ),
        ));
    }
    let count = u32_at(body, 28);
    if count > MAX_SMPL_LOOPS || 36 + 24 * count as usize > body.len() {
        return Err(error(
            at + 8 + 28,
            "smpl loop count",
            format!(
                "smpl chunk at byte {at} declares {count} loops but holds {} bytes",
                body.len()
            ),
        ));
    }
    let loops = (0..count as usize)
        .map(|i| {
            let b = &body[36 + 24 * i..];
            SmplLoop {
                cue_id: u32_at(b, 0),
                kind: u32_at(b, 4),
                start: u32_at(b, 8),
                end: u32_at(b, 12),
                fraction: u32_at(b, 16),
                play_count: u32_at(b, 20),
            }
        })
        .collect();
    Ok(Smpl {
        offset: at,
        manufacturer: u32_at(body, 0),
        product: u32_at(body, 4),
        sample_period_ns: u32_at(body, 8),
        unity_note: u32_at(body, 12),
        pitch_fraction: u32_at(body, 16),
        loops,
        sampler_data_bytes: u32_at(body, 32),
    })
}

/// Parses the RIFF structure. Refusals locate the byte offset and field involved.
pub fn parse(bytes: &[u8]) -> Result<WavFile<'_>, WavError> {
    if bytes.len() < 12 {
        return Err(error(
            0,
            "RIFF header",
            format!(
                "WAV header truncated: 12 bytes are required, {} are available",
                bytes.len()
            ),
        ));
    }
    match &bytes[0..4] {
        b"RIFF" => {}
        b"RIFX" => {
            return Err(error(
                0,
                "RIFF id",
                "big-endian RIFX at byte 0 is not supported",
            ));
        }
        b"RF64" => return Err(error(0, "RIFF id", "RF64 at byte 0 is not supported")),
        _ => {
            return Err(error(0, "RIFF id", "not a RIFF file: byte 0 is not 'RIFF'"));
        }
    }
    if &bytes[8..12] != b"WAVE" {
        return Err(error(
            8,
            "RIFF form type",
            "RIFF form type at byte 8 is not 'WAVE'",
        ));
    }
    let declared = u32_at(bytes, 4) as usize;
    let end = declared
        .checked_add(8)
        .filter(|&end| end <= bytes.len())
        .ok_or_else(|| {
            error(
                4,
                "RIFF size",
                format!(
                    "RIFF size at byte 4 declares {declared} bytes but {} follow; the file is truncated",
                    bytes.len() - 8
                ),
            )
        })?;
    if declared < 4 {
        return Err(error(
            4,
            "RIFF size",
            "RIFF size at byte 4 is smaller than the WAVE form type",
        ));
    }
    let (mut fmt, mut data, mut smpl, mut chunks) = (None, None, None, Vec::new());
    let mut pos = 12;
    while pos < end {
        if end - pos < 8 {
            return Err(error(
                pos,
                "chunk header",
                format!("incomplete chunk header at byte {pos}"),
            ));
        }
        if chunks.len() == MAX_CHUNKS {
            return Err(error(
                pos,
                "chunk count",
                format!("more than {MAX_CHUNKS} chunks"),
            ));
        }
        let id: [u8; 4] = bytes[pos..pos + 4].try_into().expect("four bytes");
        let size = u32_at(bytes, pos + 4);
        let body_start = pos + 8;
        let body_end = body_start
            .checked_add(size as usize)
            .filter(|&e| e <= end)
            .ok_or_else(|| {
                error(
                    pos + 4,
                    "chunk size",
                    format!(
                        "chunk '{}' at byte {pos} declares {size} bytes; {} remain in the RIFF chunk",
                        chunk_name(&id),
                        end - body_start
                    ),
                )
            })?;
        let body = &bytes[body_start..body_end];
        match &id {
            b"fmt " if fmt.is_some() => {
                return Err(error(
                    pos,
                    "fmt chunk",
                    format!("second fmt chunk at byte {pos}"),
                ));
            }
            b"fmt " => fmt = Some((pos, parse_fmt(body, pos)?)),
            b"data" if data.is_some() => {
                return Err(error(
                    pos,
                    "data chunk",
                    format!("second data chunk at byte {pos}"),
                ));
            }
            b"data" => data = Some((body_start, body)),
            b"smpl" if smpl.is_none() => smpl = Some(parse_smpl(body, pos)),
            b"smpl" => {
                smpl = Some(Err(error(
                    pos,
                    "smpl chunk",
                    format!("second smpl chunk at byte {pos}"),
                )));
            }
            _ => {}
        }
        chunks.push(Chunk {
            id,
            offset: pos,
            size,
        });
        // Odd chunks carry a pad byte; tolerate its absence at the very end.
        pos = (body_end + (size as usize & 1)).min(end);
    }
    let (fmt_offset, fmt) = fmt.ok_or_else(|| error(12, "fmt chunk", "no fmt chunk"))?;
    let (data_offset, data) = data.ok_or_else(|| error(12, "data chunk", "no data chunk"))?;
    if !data.len().is_multiple_of(fmt.block_align as usize) {
        return Err(error(
            data_offset - 4,
            "data size",
            format!(
                "data chunk at byte {} holds {} bytes, not whole {}-byte frames; the file is truncated or misdeclared",
                data_offset - 8,
                data.len(),
                fmt.block_align
            ),
        ));
    }
    let frames = data.len() / fmt.block_align as usize;
    if frames == 0 {
        return Err(error(
            data_offset - 4,
            "data size",
            format!(
                "data chunk at byte {} has no sample frames",
                data_offset - 8
            ),
        ));
    }
    Ok(WavFile {
        format_tag: fmt.tag,
        format: fmt.format,
        extensible: fmt.extensible,
        channels: fmt.channels,
        sample_rate: fmt.rate,
        byte_rate: fmt.byte_rate,
        block_align: fmt.block_align,
        bits: fmt.bits,
        valid_bits: fmt.valid_bits,
        channel_mask: fmt.channel_mask,
        fmt_offset,
        data_offset,
        data,
        frames,
        chunks,
        smpl,
        trailing_bytes: bytes.len() - end,
    })
}

impl WavFile<'_> {
    pub fn smpl_report(&self) -> Value {
        match &self.smpl {
            None => Value::Null,
            Some(Err(error)) => json!({"error": error.to_string()}),
            Some(Ok(s)) => json!({
                "offset": s.offset, "manufacturer": s.manufacturer, "product": s.product,
                "sample_period_ns": s.sample_period_ns, "unity_note": s.unity_note,
                "pitch_fraction": s.pitch_fraction,
                "pitch_fraction_cents": s.pitch_fraction as f64 * 100.0 / 4294967296.0,
                "sampler_data_bytes": s.sampler_data_bytes,
                "loops": s.loops.iter().enumerate().map(|(index, l)| json!({
                    "index": index, "cue_id": l.cue_id, "type": l.kind,
                    "start_frame": l.start, "end_frame_inclusive": l.end,
                    "fraction": l.fraction, "play_count": l.play_count})).collect::<Vec<_>>(),
            }),
        }
    }

    /// Header facts and chunk inventory, independent of the chosen mapping.
    pub fn report(&self) -> Value {
        let unknown: Vec<String> = self
            .chunks
            .iter()
            .filter(|c| !matches!(&c.id, b"fmt " | b"data" | b"smpl"))
            .map(|c| chunk_name(&c.id))
            .collect();
        json!({
            "format": if self.format == Format::Int { "pcm_integer" } else { "ieee_float" },
            "format_tag": self.format_tag, "extensible": self.extensible,
            "channels": self.channels, "channel_mask": self.channel_mask,
            "bits_per_sample": self.bits, "valid_bits": self.valid_bits,
            "header_sample_rate": self.sample_rate, "byte_rate": self.byte_rate,
            "byte_rate_consistent": self.byte_rate == self.sample_rate.saturating_mul(self.block_align as u32),
            "block_align": self.block_align, "frames": self.frames,
            "data_offset": self.data_offset, "data_bytes": self.data.len(),
            "chunks": self.chunks.iter().map(|c| json!({"id": chunk_name(&c.id), "offset": c.offset, "size": c.size})).collect::<Vec<_>>(),
            "uninterpreted_chunks": unknown,
            "trailing_bytes": self.trailing_bytes,
            "smpl": self.smpl_report(),
        })
    }
}

/// Builds a WAV for tests and fixtures: `samples` are interleaved raw sample
/// bytes, `extra` chunks (id, body) follow the data chunk.
pub fn build(
    format_tag: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    extensible_code: Option<u16>,
    samples: &[u8],
    extra: &[(&[u8; 4], Vec<u8>)],
) -> Vec<u8> {
    let block = channels * bits / 8;
    let mut fmt = Vec::new();
    fmt.extend(format_tag.to_le_bytes());
    fmt.extend(channels.to_le_bytes());
    fmt.extend(rate.to_le_bytes());
    fmt.extend((rate * block as u32).to_le_bytes());
    fmt.extend(block.to_le_bytes());
    fmt.extend(bits.to_le_bytes());
    if let Some(code) = extensible_code {
        fmt.extend(22u16.to_le_bytes());
        fmt.extend(bits.to_le_bytes());
        fmt.extend(0u32.to_le_bytes());
        fmt.extend(code.to_le_bytes());
        fmt.extend([0, 0]);
        fmt.extend(SUBTYPE_TAIL);
    }
    let mut body = b"WAVE".to_vec();
    let mut push = |id: &[u8; 4], bytes: &[u8]| {
        body.extend(id);
        body.extend((bytes.len() as u32).to_le_bytes());
        body.extend(bytes);
        if bytes.len() % 2 == 1 {
            body.push(0);
        }
    };
    push(b"fmt ", &fmt);
    push(b"data", samples);
    for (id, bytes) in extra {
        push(id, bytes);
    }
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

/// A `smpl` chunk body with one root note and forward loops (start, end).
pub fn smpl_body(unity_note: u32, pitch_fraction: u32, loops: &[(u32, u32, u32, u32)]) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [
        0,
        0,
        22676,
        unity_note,
        pitch_fraction,
        0,
        0,
        loops.len() as u32,
        0,
    ] {
        b.extend(u32::to_le_bytes(v));
    }
    for (i, &(kind, start, end, play_count)) in loops.iter().enumerate() {
        for v in [i as u32, kind, start, end, 0, play_count] {
            b.extend(v.to_le_bytes());
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s16(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn smpl_chunks_and_unknown_chunks_are_reported() {
        let wav = build(
            1,
            1,
            32000,
            16,
            None,
            &s16(&[0; 32]),
            &[
                (b"LIST", vec![1, 2, 3]),
                (b"smpl", smpl_body(60, 1 << 31, &[(0, 16, 31, 0)])),
            ],
        );
        let file = parse(&wav).unwrap();
        let report = file.report();
        assert_eq!(report["uninterpreted_chunks"], json!(["LIST"]));
        assert_eq!(report["smpl"]["unity_note"], 60);
        assert_eq!(report["smpl"]["pitch_fraction_cents"], 50.0);
        assert_eq!(report["smpl"]["loops"][0]["end_frame_inclusive"], 31);
        // A malformed smpl chunk is information, not a refusal of the file.
        let mut body = smpl_body(60, 0, &[(0, 0, 1, 0)]);
        body.truncate(40);
        let wav = build(1, 1, 32000, 16, None, &s16(&[0; 4]), &[(b"smpl", body)]);
        assert!(
            parse(&wav).unwrap().report()["smpl"]["error"]
                .as_str()
                .unwrap()
                .contains("declares 1 loops")
        );
    }

    #[test]
    fn malformed_files_are_refused_with_locations() {
        let good = build(1, 1, 8000, 16, None, &s16(&[1, 2, 3]), &[]);
        let refusal = |bytes: &[u8]| parse(bytes).unwrap_err().to_string();
        let mut truncated = good.clone();
        truncated.truncate(good.len() - 1);
        assert!(refusal(&truncated).contains("truncated"));
        let mut chunk = good.clone();
        let data_size = chunk.len() - 6 - 4;
        chunk[data_size..data_size + 4].copy_from_slice(&100u32.to_le_bytes());
        assert!(refusal(&chunk).contains("chunk 'data' at byte 36 declares 100"));
        let partial = build(1, 2, 8000, 16, None, &s16(&[1, 2, 3]), &[]);
        assert!(refusal(&partial).contains("not whole 4-byte frames"));
        let adpcm = build(2, 1, 8000, 16, None, &s16(&[1]), &[]);
        assert!(refusal(&adpcm).contains("0x0002"));
        let mut misaligned = good.clone();
        misaligned[32] = 4;
        assert!(refusal(&misaligned).contains("block align 4"));
        assert!(refusal(b"RIFX\0\0\0\0WAVE").contains("RIFX"));
        assert!(refusal(b"RIFF").contains("12 bytes"));
        let empty = build(1, 1, 8000, 16, None, &[], &[]);
        assert_eq!(
            parse(&empty).unwrap_err(),
            error(
                40,
                "data size",
                "data chunk at byte 36 has no sample frames"
            )
        );
        let mut short_extension = build(0xFFFE, 1, 8000, 16, Some(1), &s16(&[1]), &[]);
        short_extension[36] = 21;
        assert_eq!(
            parse(&short_extension).unwrap_err().field,
            "fmt extension size"
        );
        // A remainder too short for a chunk header is refused even after the data.
        let mut remainder = good.clone();
        remainder.extend([0; 4]);
        let size = (remainder.len() - 8) as u32;
        remainder[4..8].copy_from_slice(&size.to_le_bytes());
        assert_eq!(
            parse(&remainder).unwrap_err(),
            error(
                good.len(),
                "chunk header",
                format!("incomplete chunk header at byte {}", good.len())
            )
        );
        let no_data = {
            let mut w = good.clone();
            w[36..40].copy_from_slice(b"junk");
            w
        };
        assert_eq!(refusal(&no_data), "no data chunk");
    }
}
