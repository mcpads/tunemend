//! Nintendo DS sound-channel sample codecs: PCM8, PCM16 and the DS variant of
//! IMA-ADPCM, implemented from GBATEK's "DS Sound Notes" description.
//!
//! An IMA-ADPCM stream is a 32-bit header (initial PCM16 value in bits 0..15,
//! table index in bits 16..22, bits 23..31 unused) followed by 4-bit codes,
//! low nibble first. Decoding clamps to -0x7FFF..0x7FFF as documented; an
//! initial -0x8000 is kept until the first negative step clamps it. Container
//! layout (SWAV, STRM blocks) is in [`crate::nds_sound`].

/// The documented 89-entry step table.
pub const STEP: [i32; 89] = [
    0x0007, 0x0008, 0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x000E, 0x0010, 0x0011, 0x0013, 0x0015,
    0x0017, 0x0019, 0x001C, 0x001F, 0x0022, 0x0025, 0x0029, 0x002D, 0x0032, 0x0037, 0x003C, 0x0042,
    0x0049, 0x0050, 0x0058, 0x0061, 0x006B, 0x0076, 0x0082, 0x008F, 0x009D, 0x00AD, 0x00BE, 0x00D1,
    0x00E6, 0x00FD, 0x0117, 0x0133, 0x0151, 0x0173, 0x0198, 0x01C1, 0x01EE, 0x0220, 0x0256, 0x0292,
    0x02D4, 0x031C, 0x036C, 0x03C3, 0x0424, 0x048E, 0x0502, 0x0583, 0x0610, 0x06AB, 0x0756, 0x0812,
    0x08E0, 0x09C3, 0x0ABD, 0x0BD0, 0x0CFF, 0x0E4C, 0x0FBA, 0x114C, 0x1307, 0x14EE, 0x1706, 0x1954,
    0x1BDC, 0x1EA5, 0x21B6, 0x2515, 0x28CA, 0x2CDF, 0x315B, 0x364B, 0x3BB9, 0x41B2, 0x4844, 0x4F7E,
    0x5771, 0x602F, 0x69CE, 0x7462, 0x7FFF,
];
/// Index change per code magnitude (bits 0..2).
pub const INDEX_ADJUST: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];
pub const MAX_INDEX: u8 = 88;
/// Bytes of the IMA-ADPCM header word.
pub const HEADER_BYTES: usize = 4;

/// Decoder state: the last PCM16 value and the step table index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub pcm: i32,
    pub index: u8,
}

/// The 32-bit IMA-ADPCM header as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub word: u32,
    pub state: State,
    /// Bits 23..31, documented as unused (zero); reported, not interpreted.
    pub unused_bits: u16,
}

impl Header {
    /// Parses a header word; a table index above 88 is refused.
    pub fn parse(bytes: [u8; 4]) -> Result<Self, String> {
        let word = u32::from_le_bytes(bytes);
        let index = ((word >> 16) & 0x7F) as u8;
        if index > MAX_INDEX {
            return Err(format!(
                "IMA-ADPCM header table index {index} is outside 0..88"
            ));
        }
        Ok(Header {
            word,
            state: State {
                pcm: (word & 0xFFFF) as u16 as i16 as i32,
                index,
            },
            unused_bits: (word >> 23) as u16,
        })
    }

    pub fn word_for(state: State) -> u32 {
        (state.pcm as i16 as u16 as u32) | ((state.index as u32) << 16)
    }
}

/// The documented per-code arithmetic: the difference is the sum of shifted
/// step values ("in practice, with rounding error"), then the clamp and the
/// index change.
pub fn step(state: &mut State, code: u8) -> i32 {
    let s = STEP[state.index as usize];
    let mut diff = s >> 3;
    if code & 1 != 0 {
        diff += s >> 2;
    }
    if code & 2 != 0 {
        diff += s >> 1;
    }
    if code & 4 != 0 {
        diff += s;
    }
    state.pcm = if code & 8 == 0 {
        (state.pcm + diff).min(0x7FFF)
    } else {
        (state.pcm - diff).max(-0x7FFF)
    };
    state.index = (state.index as i32 + INDEX_ADJUST[(code & 7) as usize]).clamp(0, 88) as u8;
    state.pcm
}

/// Decoded IMA-ADPCM data.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub header: Header,
    pub pcm: Vec<i32>,
    /// Codes whose result met a clamp limit while the unclamped sum exceeded it.
    pub clamped: usize,
    /// Decoder state after the last sample.
    pub end_state: State,
}

/// Decodes `samples` codes after the header at the start of `data`.
pub fn decode(data: &[u8], samples: usize) -> Result<Decoded, String> {
    if data.len() < HEADER_BYTES {
        return Err("IMA-ADPCM data is shorter than its 4-byte header".into());
    }
    let header = Header::parse([data[0], data[1], data[2], data[3]])?;
    let codes = (data.len() - HEADER_BYTES) * 2;
    if samples > codes {
        return Err(format!(
            "{samples} samples need {} code bytes; {} are present",
            samples.div_ceil(2),
            data.len() - HEADER_BYTES
        ));
    }
    let mut state = header.state;
    let mut pcm = Vec::with_capacity(samples);
    let mut clamped = 0;
    for i in 0..samples {
        let byte = data[HEADER_BYTES + i / 2];
        let code = if i % 2 == 0 { byte & 0x0F } else { byte >> 4 };
        let before = state;
        let value = step(&mut state, code);
        if unclamped(before, code) != value {
            clamped += 1;
        }
        pcm.push(value);
    }
    Ok(Decoded {
        header,
        pcm,
        clamped,
        end_state: state,
    })
}

fn unclamped(state: State, code: u8) -> i32 {
    let s = STEP[state.index as usize];
    let diff = (s >> 3)
        + if code & 1 != 0 { s >> 2 } else { 0 }
        + if code & 2 != 0 { s >> 1 } else { 0 }
        + if code & 4 != 0 { s } else { 0 };
    if code & 8 == 0 {
        state.pcm + diff
    } else {
        state.pcm - diff
    }
}

/// Encoded IMA-ADPCM data: header word plus codes, and its decoded values.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub decoded: Vec<i32>,
    pub header: State,
}

fn greedy(target: &[i32], start: State) -> (Vec<u8>, Vec<i32>, f64) {
    let mut state = start;
    let mut codes = Vec::with_capacity(target.len());
    let mut out = Vec::with_capacity(target.len());
    let mut error = 0f64;
    for &t in target {
        let mut best: Option<(i64, u8, State)> = None;
        for code in 0..16u8 {
            let mut next = state;
            let v = step(&mut next, code);
            let e = (v as i64 - t as i64).abs();
            if best.is_none_or(|(b, _, _)| e < b) {
                best = Some((e, code, next));
            }
        }
        let (e, code, next) = best.expect("sixteen candidates");
        error += (e * e) as f64;
        state = next;
        codes.push(code);
        out.push(state.pcm);
    }
    (codes, out, error)
}

/// Deterministic encoder: the header holds the first target value (clamped to
/// ±0x7FFF) and the table index whose greedy coding of the first 256 samples
/// has the lowest squared error (ties to the lower index); each code is the
/// one whose decoded value is nearest its target (ties to the lower code).
/// Values are PCM16 units. An odd sample count pads the last high nibble
/// with code 0, which is never decoded.
pub fn encode(target: &[i32]) -> Encoded {
    let first = target.first().copied().unwrap_or(0).clamp(-0x7FFF, 0x7FFF);
    let probe = &target[..target.len().min(256)];
    let mut chosen = State {
        pcm: first,
        index: 0,
    };
    let mut best = f64::INFINITY;
    for index in 0..=MAX_INDEX {
        let start = State { pcm: first, index };
        let (_, _, error) = greedy(probe, start);
        if error < best {
            best = error;
            chosen = start;
        }
    }
    let (codes, decoded, _) = greedy(target, chosen);
    let mut bytes = Header::word_for(chosen).to_le_bytes().to_vec();
    for pair in codes.chunks(2) {
        bytes.push(pair[0] | (pair.get(1).copied().unwrap_or(0) << 4));
    }
    Encoded {
        bytes,
        decoded,
        header: chosen,
    }
}

/// Signed PCM8 as PCM16 units (`NN` plays as `NN00`).
pub fn decode_pcm8(data: &[u8]) -> Vec<i32> {
    data.iter().map(|&b| b as i8 as i32 * 256).collect()
}

/// Little-endian signed PCM16.
pub fn decode_pcm16(data: &[u8]) -> Vec<i32> {
    data.as_chunks::<2>()
        .0
        .iter()
        .map(|w| i16::from_le_bytes(*w) as i32)
        .collect()
}

/// PCM16 units to signed PCM8, rounded to nearest (halves up) and clamped,
/// the same rounding as the sample codec's `s8` target.
pub fn encode_pcm8(pcm: &[i32]) -> Vec<u8> {
    pcm.iter()
        .map(|&x| (x + 128).div_euclid(256).clamp(-128, 127) as i8 as u8)
        .collect()
}

pub fn encode_pcm16(pcm: &[i32]) -> Vec<u8> {
    pcm.iter()
        .flat_map(|&x| (x.clamp(-32768, 32767) as i16).to_le_bytes())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_table_matches_the_documented_generator() {
        // GBATEK: X=000776d2h, Table[I]=X SHR 16, X=X+(X/10), with entries
        // 3, 4 and 88 fixed.
        let mut x: u32 = 0x000776d2;
        for (i, &s) in STEP.iter().enumerate() {
            let generated = match i {
                3 => 0x0A,
                4 => 0x0B,
                88 => 0x7FFF,
                _ => (x >> 16) as i32,
            };
            assert_eq!(s, generated, "entry {i}");
            x += x / 10;
        }
    }

    #[test]
    fn codes_follow_the_documented_sum_clamp_and_index_rules() {
        let mut s = State { pcm: 0, index: 0 };
        // Step 7: diff = 0 + (7>>2=1) + (7>>1=3) + 7 = 11 for code 7.
        assert_eq!(step(&mut s, 7), 11);
        assert_eq!(s.index, 8);
        assert_eq!(step(&mut s, 0x8), 11 - (STEP[8] >> 3));
        assert_eq!(s.index, 7);
        let mut top = State {
            pcm: 0x7FF0,
            index: 88,
        };
        assert_eq!(step(&mut top, 7), 0x7FFF);
        let mut bottom = State {
            pcm: -0x7FF0,
            index: 88,
        };
        assert_eq!(step(&mut bottom, 15), -0x7FFF);
        // -0x8000 stays unclipped until a negative code clamps it.
        let mut low = State {
            pcm: -0x8000,
            index: 0,
        };
        assert_eq!(step(&mut low, 0), -0x8000);
        assert_eq!(step(&mut low, 8), -0x7FFF);
        assert_eq!(low.index, 0);
    }

    #[test]
    fn nibbles_are_low_first_and_headers_are_checked() {
        // Header pcm 100, index 10; codes 1 (low) then 9 (high).
        let data = [100, 0, 10, 0, 0x91];
        let d = decode(&data, 2).unwrap();
        let s = STEP[10];
        let first = 100 + (s >> 3) + (s >> 2);
        assert_eq!(d.pcm[0], first);
        let s2 = STEP[9];
        assert_eq!(d.pcm[1], first - ((s2 >> 3) + (s2 >> 2)));
        assert!(
            decode(&[0, 0, 89, 0], 0)
                .unwrap_err()
                .contains("outside 0..88")
        );
        assert!(decode(&data, 3).unwrap_err().contains("need"));
        let unused = decode(&[0, 0, 0x80, 0x01, 0], 1).unwrap();
        assert_eq!(unused.header.unused_bits, 3);
        assert_eq!(unused.header.state.index, 0);
    }

    #[test]
    fn encoder_round_trips_through_the_decoder() {
        let target: Vec<i32> = (0..999)
            .map(|i| ((i as f64 * 0.07).sin() * 12000.0) as i32)
            .collect();
        let e = encode(&target);
        let d = decode(&e.bytes, target.len()).unwrap();
        assert_eq!(d.pcm, e.decoded);
        assert_eq!(e.bytes.len(), 4 + target.len().div_ceil(2));
        let max = target
            .iter()
            .zip(&d.pcm)
            .map(|(a, b)| (a - b).abs())
            .max()
            .unwrap();
        assert!(max < 1500, "{max}");
        assert_eq!(encode(&target).bytes, e.bytes);
    }

    #[test]
    fn pcm_codecs_match_the_documented_scaling() {
        assert_eq!(
            decode_pcm8(&[0x7F, 0x80, 0xFF]),
            vec![0x7F00, -0x8000, -256]
        );
        assert_eq!(
            encode_pcm8(&[0x7F00, -0x8000, -256, 127, 128]),
            vec![0x7F, 0x80, 0xFF, 0, 1]
        );
        assert_eq!(
            decode_pcm16(&encode_pcm16(&[-32768, 5, 32767])),
            vec![-32768, 5, 32767]
        );
    }
}
