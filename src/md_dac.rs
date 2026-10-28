//! Mega Drive YM2612 DAC samples stored as packed four-bit codes.
//!
//! The YM2612 DAC takes an 8-bit value written by software at its own pace;
//! the chip has no sample format, rate or loop of its own. This layout holds
//! two DAC values per byte: each nibble is the upper four bits of the 8-bit
//! DAC value and the lower four bits are zero. Which nibble plays first is a
//! driver choice and is always declared ([`NibbleOrder`]); the data has no
//! header, length, rate or loop field, so the caller's slice is the extent.
//! The midpoint 0x80 is the PCM zero, as for unsigned 8-bit PCM.
use serde_json::{Value, json};

pub const ENCODING: &str = "md_dac4";
/// PCM16 value of one nibble step (one step of the upper four DAC bits).
pub const STEP: i32 = 4096;
/// Nibble of the DAC midpoint 0x80, used for padding.
pub const MIDPOINT_NIBBLE: u8 = 8;
/// The `first_bytes` rows listed in a report.
const LISTED: usize = 8;
pub const QUANTIZATION_RULE: &str = "nibble = floor((pcm16 + 2048) / 4096) + 8, clamped to 0..15 (nearest 4096-unit level, ties up); the decoded value is (nibble - 8) * 4096";

/// Which nibble of a byte is the earlier DAC value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NibbleOrder {
    HighFirst,
    LowFirst,
}

impl NibbleOrder {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "high_first" => Some(Self::HighFirst),
            "low_first" => Some(Self::LowFirst),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::HighFirst => "high_first",
            Self::LowFirst => "low_first",
        }
    }
    fn split(self, byte: u8) -> [u8; 2] {
        match self {
            Self::HighFirst => [byte >> 4, byte & 15],
            Self::LowFirst => [byte & 15, byte >> 4],
        }
    }
    fn join(self, first: u8, second: u8) -> u8 {
        match self {
            Self::HighFirst => (first << 4) | second,
            Self::LowFirst => (second << 4) | first,
        }
    }
}

/// Nibbles in play order.
pub fn nibbles(data: &[u8], order: NibbleOrder) -> Vec<u8> {
    data.iter().flat_map(|&b| order.split(b)).collect()
}

/// The 8-bit DAC value a nibble stands for.
pub fn dac_code(nibble: u8) -> u8 {
    nibble << 4
}

/// PCM16 value of a nibble: (DAC value - 0x80) * 256.
pub fn pcm(nibble: u8) -> i32 {
    (i32::from(nibble) - i32::from(MIDPOINT_NIBBLE)) * STEP
}

/// Decoded PCM16 samples; refuses an empty slice or one over `max_samples`,
/// naming the slice byte where the limit is reached.
pub fn decode(data: &[u8], order: NibbleOrder, max_samples: usize) -> Result<Vec<i32>, String> {
    if data.is_empty() {
        return Err("md_dac4 needs at least one byte; the slice is empty".into());
    }
    if data.len() * 2 > max_samples {
        return Err(format!(
            "the md_dac4 slice holds {} bytes ({} samples); at most {max_samples} samples are supported, reached at slice byte {}. Select a shorter slice",
            data.len(),
            data.len() * 2,
            max_samples / 2
        ));
    }
    Ok(nibbles(data, order).into_iter().map(pcm).collect())
}

fn histogram(nibbles: &[u8]) -> [usize; 16] {
    let mut counts = [0usize; 16];
    for &n in nibbles {
        counts[usize::from(n)] += 1;
    }
    counts
}

fn layout(order: NibbleOrder) -> Value {
    json!({
        "nibble_order": order.name(),
        "dac_value": "nibble << 4 (upper four bits of the 8-bit DAC value; lower four bits zero)",
        "pcm16": "(dac_value - 0x80) * 256 = (nibble - 8) * 4096",
        "coordinates": match order {
            NibbleOrder::HighFirst => "sample k is slice byte k / 2: bits 4-7 when k is even, bits 0-3 when k is odd",
            NibbleOrder::LowFirst => "sample k is slice byte k / 2: bits 0-3 when k is even, bits 4-7 when k is odd",
        },
        "carries": "no header, length, rate, loop or end marker; the slice is the whole extent and sample_rate is declared",
    })
}

/// Interpretation of a source slice: declared layout, counts, the nibble
/// histogram and the first bytes with their offsets and DAC values.
pub fn report(data: &[u8], order: NibbleOrder) -> Value {
    let all = nibbles(data, order);
    let counts = histogram(&all);
    let first: Vec<Value> = data
        .iter()
        .take(LISTED)
        .enumerate()
        .map(|(offset, &byte)| {
            let pair = order.split(byte);
            json!({"slice_byte": offset, "byte": format!("0x{byte:02X}"),
                "samples": [2 * offset, 2 * offset + 1],
                "nibbles": pair, "dac_values": pair.map(dac_code)})
        })
        .collect();
    json!({
        "layout": layout(order),
        "bytes": data.len(),
        "samples": all.len(),
        "nibble_histogram": counts,
        "nibble_range": [all.iter().min(), all.iter().max()],
        "extreme_nibbles": {"0": counts[0], "15": counts[15]},
        "first_bytes": first,
    })
}

/// Encodes PCM16 samples by [`QUANTIZATION_RULE`]; an odd count gets one
/// midpoint nibble. Returns the bytes and the target report.
pub fn encode(pcm: &[i32], order: NibbleOrder) -> (Vec<u8>, Value) {
    let mut clamped = 0usize;
    let mut codes: Vec<u8> = pcm
        .iter()
        .map(|&x| {
            let n = (x + STEP / 2).div_euclid(STEP) + i32::from(MIDPOINT_NIBBLE);
            if !(0..=15).contains(&n) {
                clamped += 1;
            }
            n.clamp(0, 15) as u8
        })
        .collect();
    let padded = !codes.len().is_multiple_of(2);
    if padded {
        codes.push(MIDPOINT_NIBBLE);
    }
    let bytes: Vec<u8> = codes.chunks(2).map(|p| order.join(p[0], p[1])).collect();
    let report = json!({
        "layout": layout(order),
        "bytes": bytes.len(),
        "samples": codes.len(),
        "quantization": QUANTIZATION_RULE,
        "clamped_samples": clamped,
        "padding_nibble": padded.then_some(MIDPOINT_NIBBLE),
        "nibble_histogram": histogram(&codes),
        "readback": "the written bytes were decoded again; their samples are what this report compares",
    });
    (bytes, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nibble_order_is_declared_and_values_are_linear() {
        let data = [0x0F, 0x80];
        let high = decode(&data, NibbleOrder::HighFirst, 16).unwrap();
        assert_eq!(high, [-32768, 28672, 0, -32768]);
        let low = decode(&data, NibbleOrder::LowFirst, 16).unwrap();
        assert_eq!(low, [28672, -32768, -32768, 0]);
        assert_eq!(dac_code(15), 0xF0);
        // Equal steps: linear in the nibble.
        let steps: Vec<i32> = (0..15).map(|n| pcm(n + 1) - pcm(n)).collect();
        assert!(steps.iter().all(|&s| s == STEP));
    }

    #[test]
    fn decoding_refuses_empty_and_oversized_slices_with_their_position() {
        assert!(
            decode(&[], NibbleOrder::HighFirst, 16)
                .unwrap_err()
                .contains("empty")
        );
        let e = decode(&[0x88; 9], NibbleOrder::HighFirst, 16).unwrap_err();
        assert!(
            e.contains("18 samples") && e.contains("slice byte 8"),
            "{e}"
        );
    }

    #[test]
    fn encoding_rounds_to_the_nearest_level_pads_and_clamps() {
        let pcm16 = [0, 2047, 2048, -2049, 32767, -32768, 30719];
        let (bytes, report) = encode(&pcm16, NibbleOrder::HighFirst);
        assert_eq!(
            nibbles(&bytes, NibbleOrder::HighFirst),
            [8, 8, 9, 7, 15, 0, 15, 8]
        );
        assert_eq!(report["clamped_samples"], 1);
        assert_eq!(report["padding_nibble"], 8);
        assert_eq!(report["samples"], 8);
        // Decoded levels re-encode unchanged in either order.
        for order in [NibbleOrder::HighFirst, NibbleOrder::LowFirst] {
            let source: Vec<u8> = (0..=255).collect();
            let decoded = decode(&source, order, 512).unwrap();
            assert_eq!(encode(&decoded, order).0, source);
        }
    }

    #[test]
    fn the_report_locates_bytes_and_counts_nibbles() {
        let r = report(&[0xE0, 0x8F], NibbleOrder::HighFirst);
        assert_eq!(r["samples"], 4);
        assert_eq!(r["first_bytes"][1]["byte"], "0x8F");
        assert_eq!(r["first_bytes"][1]["samples"], json!([2, 3]));
        assert_eq!(r["first_bytes"][0]["dac_values"], json!([0xE0, 0]));
        assert_eq!(r["extreme_nibbles"], json!({"0": 1, "15": 1}));
        assert_eq!(r["nibble_range"], json!([0, 15]));
    }
}
