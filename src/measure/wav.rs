//! WAV reading for measurement on the shared [`crate::wav`] structure: the
//! measurement limits on channels and rate, and normalized per-channel samples.

use crate::wav::{self, Format, WavError};
use serde::Serialize;

/// Most channels a measured file may have.
pub const MAX_CHANNELS: u16 = 8;
/// Accepted sample rates, inclusive.
pub const SAMPLE_RATES: (u32, u32) = (8_000, 384_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleEncoding {
    /// Unsigned 8-bit PCM, 128 is zero.
    PcmU8,
    PcmS16,
    PcmS24,
    PcmS32,
    Float32,
    Float64,
}

impl SampleEncoding {
    fn bytes(self) -> usize {
        match self {
            Self::PcmU8 => 1,
            Self::PcmS16 => 2,
            Self::PcmS24 => 3,
            Self::PcmS32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }
    fn format(self) -> Format {
        match self {
            Self::Float32 | Self::Float64 => Format::Float,
            _ => Format::Int,
        }
    }
    /// Normalized magnitude of the largest positive code; reaching it counts as clipping.
    pub fn clip_level(self) -> f32 {
        match self {
            Self::PcmU8 => 127.0 / 128.0,
            Self::PcmS16 => 32_767.0 / 32_768.0,
            Self::PcmS24 => 8_388_607.0 / 8_388_608.0,
            Self::PcmS32 => (2_147_483_647.0_f64 / 2_147_483_648.0) as f32,
            Self::Float32 | Self::Float64 => 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WavFormat {
    pub encoding: SampleEncoding,
    pub channels: u16,
    pub sample_rate: u32,
    /// Significant bits; less than the container for some extensible files.
    pub valid_bits: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_mask: Option<u32>,
    pub frames: u64,
    #[serde(skip)]
    pub data_offset: usize,
}

impl WavFormat {
    pub fn duration_seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate)
    }
}

/// Reads the RIFF structure and format without decoding samples, then applies
/// the measurement limits on channels and sample rate.
pub fn read_header(bytes: &[u8]) -> Result<WavFormat, WavError> {
    let file = wav::parse(bytes)?;
    let encoding = match (file.format, file.bits) {
        (Format::Int, 8) => SampleEncoding::PcmU8,
        (Format::Int, 16) => SampleEncoding::PcmS16,
        (Format::Int, 24) => SampleEncoding::PcmS24,
        (Format::Int, 32) => SampleEncoding::PcmS32,
        (Format::Float, 32) => SampleEncoding::Float32,
        (Format::Float, _) => SampleEncoding::Float64,
        (Format::Int, _) => unreachable!("the parser accepts only 8/16/24/32-bit integers"),
    };
    let fmt = file.fmt_offset;
    if file.channels > MAX_CHANNELS {
        return Err(wav::error(
            fmt + 8 + 2,
            "fmt channels",
            format!(
                "fmt chunk at byte {fmt} declares {} channels; 1–{MAX_CHANNELS} are measured",
                file.channels
            ),
        ));
    }
    if !(SAMPLE_RATES.0..=SAMPLE_RATES.1).contains(&file.sample_rate) {
        return Err(wav::error(
            fmt + 8 + 4,
            "fmt sample rate",
            format!(
                "fmt chunk at byte {fmt} declares {} Hz; {}–{} Hz are measured",
                file.sample_rate, SAMPLE_RATES.0, SAMPLE_RATES.1
            ),
        ));
    }
    Ok(WavFormat {
        encoding,
        channels: file.channels,
        sample_rate: file.sample_rate,
        valid_bits: file.valid_bits.unwrap_or(file.bits),
        channel_mask: file.channel_mask,
        frames: file.frames as u64,
        data_offset: file.data_offset,
    })
}

/// Decodes `frames` frames from `start` into one normalized vector per channel.
/// Integer codes are divided by 2^(bits-1); float values are kept and must be finite.
pub fn decode(
    bytes: &[u8],
    format: &WavFormat,
    start: u64,
    frames: u64,
) -> Result<Vec<Vec<f32>>, WavError> {
    let channels = usize::from(format.channels);
    let width = format.encoding.bytes();
    let kind = format.encoding.format();
    let frame_bytes = channels * width;
    let first = format.data_offset + start as usize * frame_bytes;
    let mut out = vec![Vec::with_capacity(frames as usize); channels];
    for frame in 0..frames as usize {
        let base = first + frame * frame_bytes;
        for (channel, samples) in out.iter_mut().enumerate() {
            let at = base + channel * width;
            let value = wav::normalized(kind, &bytes[at..at + width]) as f32;
            if !value.is_finite() {
                return Err(wav::non_finite_sample(at, start as usize + frame, channel));
            }
            samples.push(value);
        }
    }
    Ok(out)
}

/// Writes normalized samples as a plain WAV, for tests and experiments.
pub fn encode(channels: &[Vec<f32>], sample_rate: u32, encoding: SampleEncoding) -> Vec<u8> {
    let count = channels.first().map_or(0, Vec::len);
    let width = encoding.bytes();
    let tag: u16 = match encoding.format() {
        Format::Float => 3,
        Format::Int => 1,
    };
    let int = |x: f32, bits: u32| {
        let scale = f64::from(1u32 << (bits - 1));
        (f64::from(x) * scale).round().clamp(-scale, scale - 1.0) as i32
    };
    let mut data = Vec::with_capacity(count * channels.len() * width);
    for i in 0..count {
        for channel in channels {
            let x = channel[i];
            match encoding {
                SampleEncoding::PcmU8 => data.push((int(x, 8) + 128) as u8),
                SampleEncoding::PcmS16 => data.extend((int(x, 16) as i16).to_le_bytes()),
                SampleEncoding::PcmS24 => data.extend(&int(x, 24).to_le_bytes()[..3]),
                SampleEncoding::PcmS32 => {
                    let v = (f64::from(x) * 2_147_483_648.0)
                        .round()
                        .clamp(-2_147_483_648.0, 2_147_483_647.0)
                        as i32;
                    data.extend(v.to_le_bytes());
                }
                SampleEncoding::Float32 => data.extend(x.to_le_bytes()),
                SampleEncoding::Float64 => data.extend(f64::from(x).to_le_bytes()),
            }
        }
    }
    let bits = width as u16 * 8;
    wav::build(
        tag,
        channels.len() as u16,
        sample_rate,
        bits,
        None,
        &data,
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone() -> Vec<Vec<f32>> {
        let left: Vec<f32> = (0..64).map(|i| ((i as f32) * 0.3).sin() * 0.5).collect();
        let right: Vec<f32> = left.iter().map(|x| -x).collect();
        vec![left, right]
    }

    #[test]
    fn every_encoding_round_trips_within_its_resolution() {
        let source = tone();
        for (encoding, tolerance) in [
            (SampleEncoding::PcmU8, 1.0 / 128.0),
            (SampleEncoding::PcmS16, 1.0 / 32_768.0),
            (SampleEncoding::PcmS24, 1.0e-6),
            (SampleEncoding::PcmS32, 1.0e-7),
            (SampleEncoding::Float32, 0.0),
            (SampleEncoding::Float64, 0.0),
        ] {
            let bytes = encode(&source, 48_000, encoding);
            let format = read_header(&bytes).unwrap();
            assert_eq!(
                (format.encoding, format.channels, format.frames),
                (encoding, 2, 64)
            );
            let decoded = decode(&bytes, &format, 0, 64).unwrap();
            for (a, b) in source.iter().flatten().zip(decoded.iter().flatten()) {
                assert!((a - b).abs() <= tolerance, "{encoding:?} {a} {b}");
            }
        }
    }

    #[test]
    fn extensible_format_and_odd_chunks_are_read() {
        let plain = encode(&tone(), 44_100, SampleEncoding::PcmS16);
        let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
        // An odd-sized unknown chunk with its pad byte.
        bytes.extend(b"junk\x03\0\0\0abc\0");
        bytes.extend(b"fmt \x28\0\0\0");
        bytes.extend(0xFFFEu16.to_le_bytes());
        bytes.extend(&plain[22..36]); // channels, rate, byte rate, align, bits
        bytes.extend(22u16.to_le_bytes());
        bytes.extend(16u16.to_le_bytes());
        bytes.extend(3u32.to_le_bytes());
        bytes.extend([1, 0, 0, 0]);
        bytes.extend(wav::SUBTYPE_TAIL);
        bytes.extend(&plain[36..]);
        let size = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        let format = read_header(&bytes).unwrap();
        assert_eq!(format.channel_mask, Some(3));
        assert_eq!(format.encoding, SampleEncoding::PcmS16);
        assert_eq!(
            decode(&bytes, &format, 10, 4).unwrap(),
            decode(&plain, &read_header(&plain).unwrap(), 10, 4).unwrap()
        );
    }

    #[test]
    fn malformed_files_are_refused_at_their_offset() {
        let good = encode(&tone(), 48_000, SampleEncoding::PcmS16);
        let cases: Vec<(Vec<u8>, usize, &str)> = vec![
            (
                b"RIFX".iter().chain(&good[4..]).copied().collect(),
                0,
                "RIFF id",
            ),
            (good[..good.len() - 10].to_vec(), 4, "RIFF size"),
            (
                {
                    let mut b = good.clone();
                    b[20] = 2;
                    b
                },
                20,
                "fmt audio format",
            ),
            (
                {
                    let mut b = good.clone();
                    b[22] = 0;
                    b
                },
                22,
                "fmt channels",
            ),
            (
                {
                    let mut b = good.clone();
                    b[24..28].copy_from_slice(&100u32.to_le_bytes());
                    b
                },
                24,
                "fmt sample rate",
            ),
            (
                {
                    let mut b = good.clone();
                    b[32] = 3;
                    b
                },
                32,
                "fmt block align",
            ),
            (
                {
                    let mut b = good.clone();
                    b[34] = 12;
                    b
                },
                34,
                "fmt bits per sample",
            ),
            (
                {
                    let mut b = good.clone();
                    b[40..44].copy_from_slice(&1000u32.to_le_bytes());
                    b
                },
                40,
                "chunk size",
            ),
            (
                {
                    let mut b = good.clone();
                    b[36..40].copy_from_slice(b"junk");
                    b
                },
                12,
                "data chunk",
            ),
        ];
        for (bytes, offset, field) in cases {
            let error = read_header(&bytes).unwrap_err();
            assert_eq!((error.offset, error.field), (offset, field), "{error}");
            // Structural refusals are the sample-conversion parser's own.
            if field != "fmt sample rate" {
                assert_eq!(wav::parse(&bytes).unwrap_err(), error);
            }
        }
        let mut odd = good.clone();
        let size = odd.len() as u32 - 45;
        odd[40..44].copy_from_slice(&size.to_le_bytes());
        let error = read_header(&odd).unwrap_err();
        assert_eq!((error.offset, error.field), (40, "data size"));
        let mut nan = encode(&tone(), 48_000, SampleEncoding::Float32);
        nan[44 + 8..44 + 12].copy_from_slice(&f32::NAN.to_le_bytes());
        let format = read_header(&nan).unwrap();
        assert_eq!(decode(&nan, &format, 0, 64).unwrap_err().offset, 52);
    }

    #[test]
    fn both_readers_decode_every_format_and_channel_count_identically() {
        // The formulas of the two readers before they shared one parser.
        let measured = |e: SampleEncoding, b: &[u8]| -> f32 {
            match e {
                SampleEncoding::PcmU8 => (f32::from(b[0]) - 128.0) / 128.0,
                SampleEncoding::PcmS16 => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
                SampleEncoding::PcmS24 => {
                    (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0
                }
                SampleEncoding::PcmS32 => {
                    (f64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])) / 2_147_483_648.0)
                        as f32
                }
                SampleEncoding::Float32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                SampleEncoding::Float64 => {
                    f64::from_le_bytes(b.try_into().expect("eight bytes")) as f32
                }
            }
        };
        let pcm16 = |e: SampleEncoding, s: &[u8]| -> f64 {
            match e {
                SampleEncoding::PcmU8 => (s[0] as f64 - 128.0) * 256.0,
                SampleEncoding::PcmS16 => i16::from_le_bytes([s[0], s[1]]) as f64,
                SampleEncoding::PcmS24 => {
                    (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f64 / 256.0
                }
                SampleEncoding::PcmS32 => {
                    i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f64 / 65536.0
                }
                SampleEncoding::Float32 => {
                    f32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f64 * 32768.0
                }
                SampleEncoding::Float64 => {
                    f64::from_le_bytes(s.try_into().expect("eight bytes")) * 32768.0
                }
            }
        };
        let mut seed = 0x2545_f491u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed
        };
        let floats = [-2.0, -1.0, -0.5, 0.0, 1e-30, 0.1, 0.999, 1.0, 1.5];
        let frames = 40;
        for encoding in [
            SampleEncoding::PcmU8,
            SampleEncoding::PcmS16,
            SampleEncoding::PcmS24,
            SampleEncoding::PcmS32,
            SampleEncoding::Float32,
            SampleEncoding::Float64,
        ] {
            let width = encoding.bytes();
            let code = if encoding.format() == Format::Float {
                3
            } else {
                1
            };
            for extensible in [false, true] {
                for channels in [1u16, 2, 3, 8] {
                    let mut raw = Vec::new();
                    for i in 0..frames * usize::from(channels) {
                        let scale = 1.0 + f64::from(next() % 7) / 3.0;
                        let top =
                            |k: usize, last: u8, rest: u8| if k + 1 == width { last } else { rest };
                        match (encoding, i) {
                            (SampleEncoding::Float32, _) => {
                                raw.extend(((floats[i % 9] * scale) as f32).to_le_bytes())
                            }
                            (SampleEncoding::Float64, _) => {
                                raw.extend((floats[i % 9] * scale).to_le_bytes())
                            }
                            // The smallest and largest codes, signed and unsigned.
                            (_, 0) => raw.extend((0..width).map(|k| top(k, 0x80, 0))),
                            (_, 1) => raw.extend((0..width).map(|k| top(k, 0x7f, 0xff))),
                            (_, 2) => raw.extend(vec![0; width]),
                            (_, 3) => raw.extend(vec![0xff; width]),
                            _ => raw.extend(&next().to_le_bytes()[..width]),
                        }
                    }
                    let tag = if extensible { 0xFFFE } else { code };
                    let bits = width as u16 * 8;
                    let ext = extensible.then_some(code);
                    let bytes = wav::build(tag, channels, 48_000, bits, ext, &raw, &[]);
                    let sample = |f: usize, c: usize| {
                        let at = (f * usize::from(channels) + c) * width;
                        &raw[at..at + width]
                    };
                    let case = format!("{encoding:?} extensible={extensible} channels={channels}");
                    let file = wav::parse(&bytes).unwrap();
                    let expected: Vec<Vec<f64>> = (0..usize::from(channels))
                        .map(|c| (0..frames).map(|f| pcm16(encoding, sample(f, c))).collect())
                        .collect();
                    assert_eq!(
                        crate::sample_wav::channel_values(&file).unwrap(),
                        expected,
                        "{case}"
                    );
                    let format = read_header(&bytes).unwrap();
                    assert_eq!(format.encoding, encoding, "{case}");
                    let expected: Vec<Vec<f32>> = (0..usize::from(channels))
                        .map(|c| {
                            (0..frames)
                                .map(|f| measured(encoding, sample(f, c)))
                                .collect()
                        })
                        .collect();
                    assert_eq!(
                        decode(&bytes, &format, 0, frames as u64).unwrap(),
                        expected,
                        "{case}"
                    );
                    let part: Vec<Vec<f32>> = expected.iter().map(|c| c[7..12].to_vec()).collect();
                    assert_eq!(decode(&bytes, &format, 7, 5).unwrap(), part, "{case}");
                }
            }
        }
    }
}
