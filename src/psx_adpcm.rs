//! PlayStation SPU ADPCM sample blocks and the VAG container.
//!
//! A block is 16 bytes: a
//! shift/filter byte, a flag byte and 14 bytes holding 28 four-bit samples,
//! low nibble first. Decoding uses the documented equation
//! `s = (t << (12 - shift)) + ((old * pos + older * neg + 32) >> 6)`, clamped
//! to 16 bits and stored as the next history value. Flag bit 0 is loop end,
//! bit 1 repeat (only with bit 0) and bit 2 loop start; bits 3-7 are unknown.
//! Values the documentation does not define are refused, never guessed.
use serde_json::{Value, json};

pub const RAW: &str = "psx_adpcm";
pub const VAG: &str = "vag";
pub const ENCODINGS: [&str; 2] = [RAW, VAG];
pub const BLOCK_BYTES: usize = 16;
pub const BLOCK_SAMPLES: usize = 28;
pub const VAG_HEADER_BYTES: usize = 48;
/// Version written in a new VAG header (one of the two documented values).
pub const VAG_VERSION: u32 = 0x20;
/// The SPU plays a sample at 44100 Hz with pitch register 0x1000; 0x4000 is
/// the fastest documented rate.
pub const SPU_RATE: u32 = 44100;
pub const MAX_PITCH: u32 = 0x4000;
pub const POSITIVE: [i32; 5] = [0, 60, 115, 98, 122];
pub const NEGATIVE: [i32; 5] = [0, 0, -52, -55, -60];
pub const LOOP_END: u8 = 1;
pub const LOOP_REPEAT: u8 = 2;
pub const LOOP_START: u8 = 4;
const UNKNOWN_FLAG_BITS: u8 = 0xF8;
/// Shift 13..15 is documented (for CD-XA, which the SPU header reportedly
/// shares) as acting like shift 9.
pub const RESERVED_SHIFT_AS: u8 = 9;
/// Listed block indices are capped; counts are always complete.
const LISTED: usize = 64;
pub const SEARCH_RULE: &str = "per block, every filter 0-4 (only filter 0 for the first block and the loop start block) and shift 0-12; each sample takes the nibble whose decoded value is nearest the target (ties: lowest signed nibble), carrying that candidate's own history; the candidate with the lowest squared error wins, ties to the lower filter, then the lower shift";
pub const SOURCE_URL: &str = "psx-spx (Sound Processing Unit and CDROM XA-ADPCM sections)";

/// One decoded sample: documented filter equation, floor division by 64 after
/// the +32 rounding term, clamp to 16 bits.
pub fn step(t: i32, shift: u8, filter: u8, old: i32, older: i32) -> i32 {
    let f = filter as usize;
    let predicted = (old * POSITIVE[f] + older * NEGATIVE[f] + 32) >> 6;
    (((t << 12) >> shift) + predicted).clamp(-32768, 32767)
}

fn nibble(block: &[u8], index: usize) -> i32 {
    let byte = block[2 + index / 2];
    let n = if index.is_multiple_of(2) {
        byte & 15
    } else {
        byte >> 4
    } as i32;
    if n >= 8 { n - 16 } else { n }
}

/// How a parsed stream ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Loop end without repeat: the voice is released at zero envelope.
    EndMute,
    /// The documented one-shot terminator: a filter-0, all-zero block that
    /// loops to itself, so it repeats silence.
    SilentLoopBlock,
    /// Loop end with repeat to `start_block`.
    Loop { start_block: usize, explicit: bool },
}

/// A validated stream up to and including its loop-end block.
#[derive(Debug, Clone)]
pub struct Stream {
    data: Vec<u8>,
    params: Vec<(u8, u8)>,
    pub end_block: usize,
    pub ending: Ending,
    pub reserved_shift_blocks: Vec<usize>,
    pub unknown_flag_blocks: Vec<(usize, u8)>,
    report: Value,
}

fn listed<T: serde::Serialize>(items: &[T]) -> Value {
    json!({"count": items.len(), "first": &items[..items.len().min(LISTED)]})
}

impl Stream {
    /// Parses blocks of a raw stream. `base` is the stream's offset within
    /// the caller's slice, used in messages and reports; `explicit_loop` is a
    /// slice byte offset standing for the voice's repeat address register.
    pub fn parse(
        data: &[u8],
        base: usize,
        explicit_loop: Option<usize>,
        reserved_shift_as_9: bool,
        max_samples: usize,
    ) -> Result<Stream, String> {
        if data.is_empty() || !data.len().is_multiple_of(BLOCK_BYTES) {
            return Err(format!(
                "PSX ADPCM needs complete 16-byte blocks; the stream at byte {base} holds {} bytes",
                data.len()
            ));
        }
        let blocks = data.len() / BLOCK_BYTES;
        if blocks * BLOCK_SAMPLES > max_samples {
            return Err("sample limit exceeded".into());
        }
        let mut params = Vec::with_capacity(blocks);
        let (mut reserved, mut unknown, mut ignored, mut starts) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut filters, mut shifts) = ([0usize; 5], [0usize; 16]);
        let mut end = None;
        for (index, block) in data.chunks(BLOCK_BYTES).enumerate() {
            let at = base + index * BLOCK_BYTES;
            let (shift, filter, flags) = (block[0] & 15, block[0] >> 4, block[1]);
            if filter > 4 {
                return Err(format!(
                    "block {index} at byte {at}: filter {filter} is not documented (SPU ADPCM defines filters 0-4); refused rather than guessed"
                ));
            }
            let effective = if shift > 12 {
                if !reserved_shift_as_9 {
                    return Err(format!(
                        "block {index} at byte {at}: shift {shift} is reserved. The documentation says 13-15 act like 9 (stated for CD-XA, whose header the SPU reportedly shares); set psx_adpcm.reserved_shift_as_9 to decode it that way, reported as reserved_shift_interpreted"
                    ));
                }
                reserved.push(index);
                RESERVED_SHIFT_AS
            } else {
                shift
            };
            filters[filter as usize] += 1;
            shifts[shift as usize] += 1;
            params.push((effective, filter));
            if flags & UNKNOWN_FLAG_BITS != 0 {
                unknown.push((index, flags & UNKNOWN_FLAG_BITS));
            }
            if flags & LOOP_START != 0 {
                starts.push(index);
            }
            if flags & LOOP_END != 0 {
                end = Some(index);
                break;
            }
            if flags & LOOP_REPEAT != 0 {
                ignored.push(index);
            }
        }
        let end = end.ok_or_else(|| {
            format!("no block in the stream at byte {base} has the loop-end flag (bit 0); without it the SPU keeps reading the memory that follows")
        })?;
        if end + 1 != blocks {
            return Err(format!(
                "block {end} at byte {} ends the sample, but {} more blocks follow in the slice; select a slice that ends at the loop-end block",
                base + end * BLOCK_BYTES,
                blocks - end - 1
            ));
        }
        let data = data.to_vec();
        let repeat = data[end * BLOCK_BYTES + 1] & LOOP_REPEAT != 0;
        let explicit_block = match explicit_loop {
            None => None,
            Some(offset) => {
                let relative = offset
                    .checked_sub(base)
                    .filter(|r| r.is_multiple_of(BLOCK_BYTES) && r / BLOCK_BYTES <= end);
                Some(relative.map(|r| r / BLOCK_BYTES).ok_or_else(|| {
                    format!("loop_offset {offset} must be a block boundary inside this sample (bytes {base}..{}); a repeat address outside it would play another sample's storage", base + data.len())
                })?)
            }
        };
        let flagged = starts.last().copied();
        let ending = if !repeat {
            if explicit_block.is_some() {
                return Err(format!(
                    "block {end} ends with loop end and no repeat, which releases the voice; it does not loop, so omit loop_offset"
                ));
            }
            Ending::EndMute
        } else {
            let (start_block, explicit) = match (flagged, explicit_block) {
                (Some(f), Some(e)) if f != e => {
                    return Err(format!(
                        "loop_offset selects block {e}, but the loop-start flag sets the repeat address to block {f}; omit loop_offset"
                    ));
                }
                (Some(f), _) => (f, false),
                (None, Some(e)) => (e, true),
                (None, None) => {
                    return Err(format!(
                        "block {end} repeats, but no block up to it has the loop-start flag, so the repeat address is whatever the voice register held before (software or another sample's storage). Pass loop_offset (a block boundary in this slice) to declare it"
                    ));
                }
            };
            let block = &data[start_block * BLOCK_BYTES..];
            if start_block == end && block[0] >> 4 == 0 && block[2..16].iter().all(|&b| b == 0) {
                Ending::SilentLoopBlock
            } else {
                Ending::Loop {
                    start_block,
                    explicit,
                }
            }
        };
        let mut report = json!({
            "blocks": blocks,
            "first_pass_samples": blocks * BLOCK_SAMPLES,
            "end_block": end,
            "end_offset": base + end * BLOCK_BYTES,
            "end_flags": data[end * BLOCK_BYTES + 1],
            "ending": match ending {
                Ending::EndMute => "end_mute",
                Ending::SilentLoopBlock => "silent_loop_block",
                Ending::Loop { .. } => "loop",
            },
            "loop_start_flag_blocks": listed(&starts),
            "ignored_repeat_flag_blocks": listed(&ignored),
            "unknown_flag_bits": listed(&unknown.iter().map(|&(b, bits)| json!({"block": b, "bits": bits})).collect::<Vec<_>>()),
            "reserved_shift_blocks": listed(&reserved),
            "filters": filters,
            "shifts": shifts,
            "initial_history": [0, 0],
            "decoder": "documented equation (pos 0/60/115/98/122, neg 0/0/-52/-55/-60, +32 then floor division by 64, clamp to 16 bits, clamped value becomes history)",
        });
        if let Ending::Loop {
            start_block,
            explicit,
        } = ending
        {
            report["loop_start_block"] = json!(start_block);
            report["loop_start_offset"] = json!(base + start_block * BLOCK_BYTES);
            report["loop_address_source"] = json!(if explicit {
                "loop_offset"
            } else {
                "loop_start_flag"
            });
        }
        if ending == Ending::SilentLoopBlock {
            report["one_shot"] = json!(
                "the final block is the documented silent terminator: filter 0, zero data, loop start, end and repeat on itself"
            );
        }
        Ok(Stream {
            data,
            params,
            end_block: end,
            ending,
            reserved_shift_blocks: reserved,
            unknown_flag_blocks: unknown,
            report,
        })
    }

    pub fn report(&self) -> &Value {
        &self.report
    }

    pub fn loop_start_block(&self) -> Option<usize> {
        match self.ending {
            Ending::Loop { start_block, .. } => Some(start_block),
            _ => None,
        }
    }

    /// Decodes block ranges in order, carrying history across them.
    pub fn decode(&self, ranges: &[(usize, usize)]) -> Vec<i32> {
        let (mut pcm, mut old, mut older) = (Vec::new(), 0, 0);
        for &(start, end) in ranges {
            for index in start..end {
                let block = &self.data[index * BLOCK_BYTES..(index + 1) * BLOCK_BYTES];
                let (shift, filter) = self.params[index];
                for i in 0..BLOCK_SAMPLES {
                    let value = step(nibble(block, i), shift, filter, old, older);
                    pcm.push(value);
                    (older, old) = (old, value);
                }
            }
        }
        pcm
    }

    /// The first pass through the loop-end block.
    pub fn first_pass(&self) -> Vec<i32> {
        self.decode(&[(0, self.end_block + 1)])
    }

    /// First pass followed by `traversals` loop passes (a looping stream).
    pub fn with_traversals(&self, traversals: usize) -> Vec<i32> {
        let mut ranges = vec![(0, self.end_block + 1)];
        if let Some(start) = self.loop_start_block() {
            ranges.extend(std::iter::repeat_n((start, self.end_block + 1), traversals));
        }
        self.decode(&ranges)
    }
}

/// How an encoded target ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetEnd {
    /// Append the documented silent self-looping block.
    SilentLoopBlock,
    /// Loop end without repeat on the last data block.
    EndMute,
    /// Loop start on this block, loop end and repeat on the last block.
    Loop(usize),
}

/// Encodes `pcm` (padded with zeros to whole blocks) with the documented
/// flags for `end`. Returns the stream and a placement report.
pub fn encode(pcm: &[i32], end: TargetEnd) -> (Vec<u8>, Value) {
    let mut padded = pcm.to_vec();
    padded.resize(pcm.len().div_ceil(BLOCK_SAMPLES).max(1) * BLOCK_SAMPLES, 0);
    let blocks = padded.len() / BLOCK_SAMPLES;
    let restart = match end {
        TargetEnd::Loop(start) => Some(start),
        _ => None,
    };
    let (mut out, mut old, mut older) = (Vec::with_capacity((blocks + 1) * BLOCK_BYTES), 0, 0);
    let (mut filters, mut shifts) = ([0usize; 5], [0usize; 16]);
    for (index, targets) in padded.chunks(BLOCK_SAMPLES).enumerate() {
        let filter_count = if index == 0 || Some(index) == restart {
            1
        } else {
            5
        };
        let mut best: Option<(i64, u8, u8, [u8; BLOCK_SAMPLES], i32, i32)> = None;
        for filter in 0..filter_count {
            for shift in 0..=12u8 {
                let (mut a, mut b, mut error, mut codes) = (old, older, 0i64, [0u8; BLOCK_SAMPLES]);
                for (slot, &target) in targets.iter().enumerate() {
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
        (old, older) = (a, b);
        filters[filter as usize] += 1;
        shifts[shift as usize] += 1;
        let last = index + 1 == blocks;
        let mut flags = 0;
        if Some(index) == restart {
            flags |= LOOP_START;
        }
        if last {
            match end {
                TargetEnd::SilentLoopBlock => {}
                TargetEnd::EndMute => flags |= LOOP_END,
                TargetEnd::Loop(_) => flags |= LOOP_END | LOOP_REPEAT,
            }
        }
        out.push((filter << 4) | shift);
        out.push(flags);
        out.extend(codes.chunks(2).map(|c| c[0] | (c[1] << 4)));
    }
    let mut report = json!({
        "data_blocks": blocks,
        "filters": filters,
        "shifts": shifts,
        "first_block_filter_0": true,
        "encoder": SEARCH_RULE,
    });
    match end {
        TargetEnd::SilentLoopBlock => {
            out.push(0);
            out.push(LOOP_START | LOOP_END | LOOP_REPEAT);
            out.extend([0u8; BLOCK_BYTES - 2]);
            report["one_shot_end"] = json!("silent_loop_block");
            report["terminator"] = json!({"block": blocks, "flags": 7, "samples": BLOCK_SAMPLES, "meaning": "documented one-shot terminator: filter 0, zero data, loop start, end and repeat on itself"});
        }
        TargetEnd::EndMute => {
            report["one_shot_end"] = json!("end_mute");
            report["end_block"] = json!(blocks - 1);
            report["end_flags"] = json!(LOOP_END);
        }
        TargetEnd::Loop(start) => {
            report["loop_start_block"] = json!(start);
            report["loop_start_flags"] = json!(out[start * BLOCK_BYTES + 1]);
            report["end_block"] = json!(blocks - 1);
            report["end_flags"] = json!(out[(blocks - 1) * BLOCK_BYTES + 1]);
            report["loop_block_filter_0"] = json!(true);
        }
    }
    report["blocks"] = json!(out.len() / BLOCK_BYTES);
    (out, report)
}

/// A VAG file: the 48-byte header and the block data it declares.
#[derive(Debug, Clone)]
pub struct Vag {
    pub header: [u8; VAG_HEADER_BYTES],
    pub data_size: usize,
    pub sample_rate: u32,
    pub trailing: usize,
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Vag {
    /// Reads the documented big-endian header: magic `VAGp`, version at 4,
    /// reserved 8..12, data size at 12, sample rate at 16, reserved 20..32
    /// and a 16-byte name. The data size counts the block bytes after the
    /// header; bytes past it are retained, not interpreted.
    pub fn parse(bytes: &[u8]) -> Result<Vag, String> {
        if bytes.len() < VAG_HEADER_BYTES {
            return Err(format!(
                "a VAG header needs 48 bytes; the slice holds {}",
                bytes.len()
            ));
        }
        match &bytes[..4] {
            b"VAGp" => {}
            b"VAGi" => {
                return Err("magic VAGi (interleaved VAG) is not admitted; only mono VAGp files are interpreted".into());
            }
            b"pGAV" => {
                return Err("magic pGAV (byte-reversed VAG) is not admitted; only big-endian VAGp files are interpreted".into());
            }
            other => {
                return Err(format!(
                    "bytes 0..4 are {:?}, not the VAG magic VAGp; use encoding psx_adpcm for a headerless stream",
                    String::from_utf8_lossy(other)
                ));
            }
        }
        let header: [u8; VAG_HEADER_BYTES] =
            bytes[..VAG_HEADER_BYTES].try_into().expect("48 bytes");
        let data_size = be32(bytes, 12) as usize;
        let available = bytes.len() - VAG_HEADER_BYTES;
        if data_size == 0 || data_size > available || !data_size.is_multiple_of(BLOCK_BYTES) {
            return Err(format!(
                "the VAG data size at byte 12 is {data_size}, but {available} bytes follow the header; it must be a positive multiple of 16 that fits. Use encoding psx_adpcm with an explicit offset/length to read the blocks without the header"
            ));
        }
        let sample_rate = be32(bytes, 16);
        Ok(Vag {
            header,
            data_size,
            sample_rate,
            trailing: available - data_size,
        })
    }

    pub fn report(&self) -> Value {
        header_report(&self.header, self.trailing)
    }
}

fn header_report(header: &[u8; VAG_HEADER_BYTES], trailing: usize) -> Value {
    let version = be32(header, 4);
    let name = &header[32..48];
    let text_end = name.iter().position(|&b| b == 0).unwrap_or(16);
    json!({
        "magic": "VAGp",
        "version": version,
        "version_documented": version == 3 || version == 0x20,
        "data_size": be32(header, 12),
        "sample_rate": be32(header, 16),
        "name": String::from_utf8_lossy(&name[..text_end]),
        "name_hex": hex(name),
        "reserved": [
            {"offset": 8, "hex": hex(&header[8..12]), "zero": header[8..12].iter().all(|&b| b == 0)},
            {"offset": 20, "hex": hex(&header[20..32]), "zero": header[20..32].iter().all(|&b| b == 0)}
        ],
        "header_hex": hex(header),
        "data_offset": VAG_HEADER_BYTES,
        "trailing_bytes": trailing,
        "fields": "big-endian: version at 4, data size at 12 (block bytes after the header), sample rate at 16; name at 32..48; reserved bytes are retained, not interpreted",
    })
}

/// A VAG header for `data_size` block bytes at `rate`. With a `template` (a
/// source VAG header) every byte is kept except the data size, the sample
/// rate and, when given, the name; the report lists changed offsets.
pub fn build_header(
    template: Option<&[u8; VAG_HEADER_BYTES]>,
    data_size: usize,
    rate: u32,
    name: Option<&str>,
) -> Result<([u8; VAG_HEADER_BYTES], Value), String> {
    let size = u32::try_from(data_size).map_err(|_| "VAG data size exceeds 32 bits")?;
    let mut header = match template {
        Some(t) => *t,
        None => {
            let mut h = [0u8; VAG_HEADER_BYTES];
            h[..4].copy_from_slice(b"VAGp");
            h[4..8].copy_from_slice(&VAG_VERSION.to_be_bytes());
            h
        }
    };
    header[12..16].copy_from_slice(&size.to_be_bytes());
    header[16..20].copy_from_slice(&rate.to_be_bytes());
    if let Some(name) = name {
        header[32..48].fill(0);
        header[32..32 + name.len()].copy_from_slice(name.as_bytes());
    }
    let mut report = header_report(&header, 0);
    report["header_source"] = json!(if template.is_some() {
        "source VAG header; only data size, sample rate and an explicit name are rewritten"
    } else {
        "new header: VAGp, version 0x20, zero reserved bytes"
    });
    if let Some(t) = template {
        let changed: Vec<usize> = (0..VAG_HEADER_BYTES)
            .filter(|&i| t[i] != header[i])
            .collect();
        report["changed_header_offsets"] = json!(changed);
    }
    report
        .as_object_mut()
        .expect("object")
        .remove("trailing_bytes");
    Ok((header, report))
}

/// Checks a VAG name: printable ASCII, at most 16 bytes.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.len() > 16 || !name.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err("psx_adpcm.name must be printable ASCII of at most 16 bytes".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(header: u8, flags: u8, data: [u8; 14]) -> Vec<u8> {
        let mut b = vec![header, flags];
        b.extend(data);
        b
    }

    #[test]
    fn decoder_follows_the_documented_equation() {
        // Shift 0 keeps the full 12-bit scale: 7 << 12.
        assert_eq!(step(7, 0, 0, 0, 0), 28672);
        assert_eq!(step(-8, 0, 0, 0, 0), -32768);
        // Shift 12 leaves the nibble itself.
        assert_eq!(step(-3, 12, 0, 0, 0), -3);
        // Filter 1: old * 60 / 64 with +32 rounding: (1000 * 60 + 32) >> 6 = 938.
        assert_eq!(step(0, 12, 1, 1000, 0), 938);
        // Filter 2 uses both history values: (1000*115 - 400*52 + 32) >> 6.
        assert_eq!(step(1, 12, 2, 1000, 400), 1 + ((115000 - 20800 + 32) >> 6));
        // Floor division of a negative prediction: (-1000 * 60 + 32) >> 6 = -937.
        assert_eq!(step(0, 12, 1, -1000, 0), -937);
        // Filter 3 and 4 coefficients.
        assert_eq!(step(0, 12, 3, 640, 640), (640 * 98 - 640 * 55 + 32) >> 6);
        assert_eq!(step(0, 12, 4, 640, 640), (640 * 122 - 640 * 60 + 32) >> 6);
        // Clamped, and the clamped value is history.
        assert_eq!(step(7, 0, 4, 32767, 0), 32767);
        assert_eq!(step(-8, 0, 1, -32768, 0), -32768);
    }

    #[test]
    fn decoding_carries_history_across_blocks_and_reads_low_nibbles_first() {
        let mut data = block(0x0C, 0, [0x21; 14]);
        data.extend(block(0x1C, LOOP_END, [0; 14]));
        let stream = Stream::parse(&data, 0, None, false, 1 << 20).unwrap();
        let pcm = stream.first_pass();
        assert_eq!(&pcm[..4], [1, 2, 1, 2]);
        // Filter 1 predicts from the last value of block 0 (2): (2*60+32)>>6 = 2.
        assert_eq!(pcm[28], 2);
        assert_eq!(stream.ending, Ending::EndMute);
    }

    #[test]
    fn undocumented_headers_and_missing_ends_are_refused() {
        let filter5 = block(0x50, LOOP_END, [0; 14]);
        assert!(
            Stream::parse(&filter5, 0, None, false, 1 << 20)
                .unwrap_err()
                .contains("filter 5 is not documented")
        );
        let shift13 = block(0x0D, LOOP_END, [0x11; 14]);
        assert!(
            Stream::parse(&shift13, 0, None, false, 1 << 20)
                .unwrap_err()
                .contains("reserved_shift_as_9")
        );
        let as9 = Stream::parse(&shift13, 0, None, true, 1 << 20).unwrap();
        assert_eq!(as9.first_pass()[0], step(1, 9, 0, 0, 0));
        assert_eq!(as9.reserved_shift_blocks, [0]);
        assert!(
            Stream::parse(&block(0, 0, [0; 14]), 0, None, false, 1 << 20)
                .unwrap_err()
                .contains("loop-end flag")
        );
        let mut extra = block(0, LOOP_END, [0; 14]);
        extra.extend(block(0, 0, [0; 14]));
        assert!(
            Stream::parse(&extra, 0, None, false, 1 << 20)
                .unwrap_err()
                .contains("1 more blocks follow")
        );
        assert!(Stream::parse(&[0; 15], 0, None, false, 1 << 20).is_err());
    }

    #[test]
    fn loop_addresses_come_from_flags_or_an_explicit_in_slice_offset() {
        let mut data = block(0x0C, 0, [0x11; 14]);
        data.extend(block(0x0C, LOOP_START, [0x22; 14]));
        data.extend(block(0x0C, LOOP_END | LOOP_REPEAT, [0x33; 14]));
        let s = Stream::parse(&data, 0, None, false, 1 << 20).unwrap();
        assert_eq!(s.loop_start_block(), Some(1));
        assert_eq!(s.with_traversals(1).len(), 28 * 5);
        assert!(
            Stream::parse(&data, 0, Some(0), false, 1 << 20)
                .unwrap_err()
                .contains("loop-start flag")
        );
        // Without a loop-start flag the repeat address is foreign state.
        data[17] = 0;
        assert!(
            Stream::parse(&data, 0, None, false, 1 << 20)
                .unwrap_err()
                .contains("Pass loop_offset")
        );
        let s = Stream::parse(&data, 0, Some(16), false, 1 << 20).unwrap();
        assert_eq!(s.report()["loop_address_source"], "loop_offset");
        assert!(
            Stream::parse(&data, 0, Some(48), false, 1 << 20)
                .unwrap_err()
                .contains("another sample's storage")
        );
        assert!(Stream::parse(&data, 0, Some(8), false, 1 << 20).is_err());
    }

    #[test]
    fn encoder_places_documented_flags_for_each_ending() {
        let pcm: Vec<i32> = (0..60).map(|i| (i * 300) - 9000).collect();
        let (oneshot, report) = encode(&pcm, TargetEnd::SilentLoopBlock);
        assert_eq!(oneshot.len(), 4 * 16);
        assert_eq!(oneshot[1] | oneshot[17] | oneshot[33], 0);
        assert_eq!(
            &oneshot[48..],
            &[0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(report["terminator"]["block"], 3);
        let s = Stream::parse(&oneshot, 0, None, false, 1 << 20).unwrap();
        assert_eq!(s.ending, Ending::SilentLoopBlock);
        let (mute, _) = encode(&pcm, TargetEnd::EndMute);
        assert_eq!((mute.len(), mute[33]), (48, LOOP_END));
        let (looped, _) = encode(&pcm[..56], TargetEnd::Loop(1));
        assert_eq!(
            (looped[17], looped[16] >> 4),
            (LOOP_START | LOOP_END | LOOP_REPEAT, 0)
        );
        let (looped, _) = encode(&pcm[..56], TargetEnd::Loop(0));
        assert_eq!(
            (looped[1], looped[17]),
            (LOOP_START, LOOP_END | LOOP_REPEAT)
        );
        assert_eq!(oneshot[0] >> 4, 0, "first block uses filter 0");
    }

    #[test]
    fn encoder_reproduces_representable_streams_exactly_and_bounds_error() {
        // Decoded output of an arbitrary valid stream is re-encodable exactly
        // when every block is filter 0 (no history dependence).
        let mut data = Vec::new();
        for (i, shift) in [4u8, 7, 12].iter().enumerate() {
            let fill = [(i as u8 * 37) ^ 0x5A; 14];
            data.extend(block(*shift, if i == 2 { LOOP_END } else { 0 }, fill));
        }
        let pcm = Stream::parse(&data, 0, None, false, 1 << 20)
            .unwrap()
            .first_pass();
        let (encoded, _) = encode(&pcm, TargetEnd::EndMute);
        let again = Stream::parse(&encoded, 0, None, false, 1 << 20)
            .unwrap()
            .first_pass();
        assert_eq!(again, pcm);
        // A smooth tone: the filter-0 first block has a coarse step (an
        // amplitude of 12000 needs shift 1, step 2048); later blocks predict.
        let tone: Vec<i32> = (0..280)
            .map(|i| (12000.0 * (i as f64 * 0.07).sin()).round() as i32)
            .collect();
        let (encoded, _) = encode(&tone, TargetEnd::EndMute);
        let decoded = Stream::parse(&encoded, 0, None, false, 1 << 20)
            .unwrap()
            .first_pass();
        let error = |range: std::ops::Range<usize>| {
            range.map(|i| (tone[i] - decoded[i]).abs()).max().unwrap()
        };
        let (first, rest) = (error(0..28), error(28..280));
        assert!(first <= 1024 && rest <= 64, "max errors {first} {rest}");
    }

    #[test]
    fn vag_headers_are_read_big_endian_and_rebuilt_byte_for_byte() {
        let mut file = b"VAGp".to_vec();
        file.extend(3u32.to_be_bytes());
        file.extend([0xAA, 0, 0, 1]);
        file.extend(16u32.to_be_bytes());
        file.extend(22050u32.to_be_bytes());
        file.extend([0x5C; 12]);
        file.extend(b"bell\0\0\0\0\0\0\0\0\0\0\0\x01");
        file.extend(block(0, LOOP_END, [0; 14]));
        file.extend([9, 9]);
        let vag = Vag::parse(&file).unwrap();
        assert_eq!(
            (vag.data_size, vag.sample_rate, vag.trailing),
            (16, 22050, 2)
        );
        let report = vag.report();
        assert_eq!(report["name"], "bell");
        assert_eq!(report["reserved"][0]["zero"], false);
        assert_eq!(report["version_documented"], true);
        let (header, built) = build_header(Some(&vag.header), 32, 22050, None).unwrap();
        assert_eq!(built["changed_header_offsets"], json!([15]));
        assert_eq!(&header[20..48], &file[20..48]);
        let (_, renamed) = build_header(Some(&vag.header), 16, 11025, Some("x")).unwrap();
        assert_eq!(
            renamed["changed_header_offsets"],
            json!([18, 19, 32, 33, 34, 35, 47])
        );
        let mut bad = file.clone();
        bad[..4].copy_from_slice(b"VAGi");
        assert!(Vag::parse(&bad).unwrap_err().contains("interleaved"));
        bad[..4].copy_from_slice(b"pGAV");
        assert!(Vag::parse(&bad).unwrap_err().contains("byte-reversed"));
        let mut long = file.clone();
        long[15] = 64;
        assert!(Vag::parse(&long).unwrap_err().contains("data size"));
        assert!(check_name("seventeen bytes!!").is_err());
        assert!(check_name("ok name").is_ok());
    }
}
