//! WAV interpretation for sample conversion, on the shared [`crate::wav`]
//! structure. Decoded values are exact PCM16-unit numbers; choosing a channel
//! or a mean downmix and reducing to 16 bits happen in [`interpret`], which
//! measures both.
use crate::wav::{Format, WavError, WavFile, non_finite_sample, normalized};
use serde_json::{Map, Value, json};

/// How several channels become one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Channel(u16),
    Mean,
}

#[derive(Debug)]
pub struct Interpreted {
    /// Mono PCM16 values.
    pub pcm: Vec<i32>,
    /// `channel_downmix` and `bit_depth_reduction` when they apply.
    pub changes: Map<String, Value>,
}

/// Channel-major sample values in PCM16 units, exact for every format:
/// integers scale by powers of two and float 1.0 is 32768.
pub fn channel_values(file: &WavFile) -> Result<Vec<Vec<f64>>, WavError> {
    let width = file.bits as usize / 8;
    let mut out = vec![Vec::with_capacity(file.frames); file.channels as usize];
    for (frame, bytes) in file
        .data
        .chunks_exact(file.block_align as usize)
        .enumerate()
    {
        for (channel, s) in bytes.chunks_exact(width).enumerate() {
            let value = normalized(file.format, s) * 32768.0;
            if !value.is_finite() {
                return Err(non_finite_sample(
                    file.data_offset + frame * file.block_align as usize + channel * width,
                    frame,
                    channel,
                ));
            }
            out[channel].push(value);
        }
    }
    Ok(out)
}

fn stats(errors: impl Iterator<Item = f64>) -> (usize, f64, f64, usize) {
    let (mut changed, mut max, mut squares, mut n) = (0, 0f64, 0f64, 0);
    for e in errors {
        n += 1;
        if e != 0.0 {
            changed += 1;
            max = max.max(e.abs());
            squares += e * e;
        }
    }
    (changed, max, (squares / n.max(1) as f64).sqrt(), n)
}

/// Mono PCM16 from one channel or the per-frame mean of all channels,
/// rounded half away from zero and clamped. Measures what changed.
pub fn interpret(file: &WavFile, selection: Selection) -> Result<Interpreted, String> {
    let values = channel_values(file).map_err(|e| e.to_string())?;
    let precise: Vec<f64> = match selection {
        Selection::Channel(c) => values
            .get(c as usize)
            .ok_or_else(|| {
                format!(
                    "wav.channel {c} is outside the file's {} channels (0-based)",
                    file.channels
                )
            })?
            .clone(),
        Selection::Mean if file.channels < 2 => {
            return Err("wav.downmix needs more than one channel".into());
        }
        Selection::Mean => (0..file.frames)
            .map(|f| values.iter().map(|c| c[f]).sum::<f64>() / file.channels as f64)
            .collect(),
    };
    let pcm: Vec<i32> = precise
        .iter()
        .map(|&x| x.round().clamp(-32768.0, 32767.0) as i32)
        .collect();
    let mut changes = Map::new();
    if selection == Selection::Mean {
        let (_, max, rms, _) = stats(
            values
                .iter()
                .flat_map(|c| c.iter().zip(&precise).map(|(&v, &m)| v - m)),
        );
        changes.insert(
            "channel_downmix".into(),
            json!({"method": "mean", "channels": file.channels,
                "max_channel_deviation": max, "rms_channel_deviation": rms}),
        );
    }
    let (changed, max, rms, _) = stats(pcm.iter().zip(&precise).map(|(&q, &x)| q as f64 - x));
    if changed > 0 {
        let clipped = precise
            .iter()
            .filter(|&&x| !(-32768.5..32767.5).contains(&x))
            .count();
        changes.insert(
            "bit_depth_reduction".into(),
            json!({"source_bits": file.bits,
                "source_format": if file.format == Format::Int { "pcm_integer" } else { "ieee_float" },
                "to": "pcm16", "rounding": "half away from zero",
                "changed_samples": changed, "clipped_samples": clipped,
                "max_absolute_error": max, "rms_error": rms}),
        );
    }
    Ok(Interpreted { pcm, changes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{build, parse};

    fn s16(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn every_supported_format_decodes_to_exact_pcm16_units() {
        type Case = (u16, u16, Option<u16>, Vec<u8>, Vec<f64>);
        let cases: Vec<Case> = vec![
            (1, 8, None, vec![0, 128, 255], vec![-32768.0, 0.0, 32512.0]),
            (
                1,
                16,
                None,
                s16(&[-32768, 1, 32767]),
                vec![-32768.0, 1.0, 32767.0],
            ),
            (
                1,
                24,
                None,
                vec![0x80, 0, 0x80, 0xff, 0xff, 0x7f, 0x01, 0x01, 0],
                vec![-32768.0 + 0.5, 32767.0 + 255.0 / 256.0, 1.0 + 1.0 / 256.0],
            ),
            (
                1,
                32,
                None,
                [i32::MIN, 65536 * 3 + 1]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
                vec![-32768.0, 3.0 + 1.0 / 65536.0],
            ),
            (
                3,
                32,
                None,
                [-1.0f32, 0.5, 1.0]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
                vec![-32768.0, 16384.0, 32768.0],
            ),
            (
                3,
                64,
                None,
                [0.25f64].iter().flat_map(|v| v.to_le_bytes()).collect(),
                vec![8192.0],
            ),
            (0xFFFE, 16, Some(1), s16(&[5, -5]), vec![5.0, -5.0]),
            (
                0xFFFE,
                32,
                Some(3),
                [0.125f32].iter().flat_map(|v| v.to_le_bytes()).collect(),
                vec![4096.0],
            ),
        ];
        for (tag, bits, ext, raw, expected) in cases {
            let wav = build(tag, 1, 44100, bits, ext, &raw, &[]);
            let file = parse(&wav).unwrap_or_else(|e| panic!("{tag} {bits}: {e}"));
            assert_eq!(
                channel_values(&file).unwrap(),
                vec![expected],
                "{tag} {bits}"
            );
            assert_eq!(file.extensible, ext.is_some());
        }
        let nan = build(3, 1, 8000, 32, None, &f32::NAN.to_le_bytes(), &[]);
        assert!(
            channel_values(&parse(&nan).unwrap())
                .unwrap_err()
                .to_string()
                .contains("frame 0, channel 0 (byte 44)")
        );
    }

    #[test]
    fn reductions_and_downmixes_are_measured() {
        // 24-bit: 1.5 PCM16 units rounds away from zero; 16-bit exact values stay.
        let wav = build(1, 1, 8000, 24, None, &[0x80, 0x01, 0, 0, 0x02, 0], &[]);
        let out = interpret(&parse(&wav).unwrap(), Selection::Channel(0)).unwrap();
        assert_eq!(out.pcm, [2, 2]);
        let r = &out.changes["bit_depth_reduction"];
        assert_eq!(
            (
                r["changed_samples"].as_u64(),
                r["max_absolute_error"].as_f64()
            ),
            (Some(1), Some(0.5))
        );
        // Float above full scale clips and is counted.
        let wav = build(3, 1, 8000, 32, None, &1.5f32.to_le_bytes(), &[]);
        let out = interpret(&parse(&wav).unwrap(), Selection::Channel(0)).unwrap();
        assert_eq!(
            (
                out.pcm[0],
                &out.changes["bit_depth_reduction"]["clipped_samples"]
            ),
            (32767, &json!(1))
        );
        // Stereo: selecting a channel is exact; the mean is a measured downmix.
        let wav = build(1, 2, 8000, 16, None, &s16(&[100, 0, 101, 0]), &[]);
        let file = parse(&wav).unwrap();
        let left = interpret(&file, Selection::Channel(0)).unwrap();
        assert_eq!((left.pcm, left.changes.len()), (vec![100, 101], 0));
        let mean = interpret(&file, Selection::Mean).unwrap();
        assert_eq!(mean.pcm, [50, 51]);
        assert_eq!(
            mean.changes["channel_downmix"]["max_channel_deviation"],
            50.5
        );
        assert_eq!(mean.changes["bit_depth_reduction"]["changed_samples"], 1);
        assert!(
            interpret(&file, Selection::Channel(2))
                .unwrap_err()
                .contains("outside")
        );
    }
}
