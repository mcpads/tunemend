//! PlayStation VAB sample banks: the VH header (bank header, program table,
//! tone attributes and VAG size table) and the VB body of headerless SPU
//! ADPCM regions, as separate files or one combined VAB.
//!
//! Every byte is retained. Interpretation reports each field with its
//! absolute offset, including reserved and unused entries; layouts whose
//! counts and sizes disagree are refused with the offending offset rather
//! than guessed. Sample regions decode through [`crate::psx_adpcm`].
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::psx_adpcm::{self, BLOCK_BYTES, BLOCK_SAMPLES, LOOP_END, Stream};

pub const MAGIC: &[u8; 4] = b"pBAV";
pub const BANK_HEADER_BYTES: usize = 0x20;
pub const PROGRAMS: usize = 128;
pub const PROGRAM_BYTES: usize = 0x10;
pub const PROGRAM_TABLE: usize = 0x20;
pub const TONE_TABLE: usize = PROGRAM_TABLE + PROGRAMS * PROGRAM_BYTES;
pub const TONES_PER_PROGRAM: usize = 16;
pub const TONE_BYTES: usize = 0x20;
pub const TONE_BLOCK_BYTES: usize = TONES_PER_PROGRAM * TONE_BYTES;
pub const VAG_SLOTS: usize = 256;
pub const VAG_TABLE_BYTES: usize = VAG_SLOTS * 2;
/// The VAG size table counts 8-byte units.
pub const VAG_SIZE_UNIT: usize = 8;
/// Versions named by the cited description.
pub const DOCUMENTED_VERSIONS: [u32; 4] = [5, 6, 7, 0x20];
/// "Sony wants the total size of the ADPCM data to be max 7E000h bytes".
pub const DOCUMENTED_BODY_BUDGET: usize = 0x7E000;
/// SPU RAM is 512 KiB and its first 4 KiB are capture buffers, so no body
/// larger than this can be resident.
pub const SPU_SAMPLE_RAM: usize = 0x80000 - 0x1000;
pub const SCOPE: &str = "PlayStation VAB (pBAV) bank: header, program and tone attributes, VAG size table and SPU ADPCM regions, interpreted from public descriptions; driver note/volume arithmetic and hardware playback are not established (sequence playback against a bank is render_psx_sequence)";

/// How the bank was supplied: a VH and a VB, or one VAB holding both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Packing {
    Separate,
    Combined,
}

/// A located refusal: which part, the byte offset within it, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VabError {
    pub part: &'static str,
    pub offset: usize,
    pub message: String,
}

impl VabError {
    fn header(offset: usize, message: impl Into<String>) -> Self {
        VabError {
            part: "header",
            offset,
            message: message.into(),
        }
    }
    fn body(offset: usize, message: impl Into<String>) -> Self {
        VabError {
            part: "body",
            offset,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for VabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} byte {:#x} ({}): {}",
            self.part, self.offset, self.offset, self.message
        )
    }
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}
fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn field(offset: usize, value: impl Serialize) -> Value {
    json!({"offset": offset, "value": value})
}

/// A sample region of the body: the bytes the VAG size table assigns to one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: usize,
    pub bytes: usize,
}

/// A validated VAB bank.
#[derive(Debug, Clone)]
pub struct Vab {
    pub header: Vec<u8>,
    pub body: Vec<u8>,
    pub packing: Packing,
    /// Program index of each tone block, in table order.
    pub tone_programs: Vec<usize>,
    /// Region of each of the 256 VAG slots; slot 0 precedes VAG 1.
    pub regions: Vec<Region>,
    pub vag_count: usize,
    /// Body bytes past the last region, retained and not interpreted.
    pub trailing: usize,
}

/// Header length a VAB declares through its program table, checked
/// against the program count; used to split a combined file.
pub fn header_length(bytes: &[u8]) -> Result<(usize, Vec<usize>), VabError> {
    if bytes.len() < BANK_HEADER_BYTES {
        return Err(VabError::header(
            0,
            format!(
                "a VAB header needs at least {BANK_HEADER_BYTES} bytes; the slice holds {}",
                bytes.len()
            ),
        ));
    }
    match &bytes[..4] {
        m if m == MAGIC => {}
        b"VABp" => {
            return Err(VabError::header(
                0,
                "magic VABp is the byte-reversed (big-endian) form; only little-endian pBAV banks are admitted",
            ));
        }
        other => {
            return Err(VabError::header(
                0,
                format!(
                    "bytes 0..4 are {:?} ({}), not the VAB magic pBAV",
                    String::from_utf8_lossy(other),
                    hex(other)
                ),
            ));
        }
    }
    if bytes.len() < TONE_TABLE {
        return Err(VabError::header(
            bytes.len(),
            format!(
                "the program table ends at {TONE_TABLE:#x}, but the slice holds {} bytes",
                bytes.len()
            ),
        ));
    }
    let mut with_tones = Vec::new();
    for program in 0..PROGRAMS {
        let at = PROGRAM_TABLE + program * PROGRAM_BYTES;
        let tones = bytes[at] as usize;
        if tones > TONES_PER_PROGRAM {
            return Err(VabError::header(
                at,
                format!(
                    "program {program} declares {tones} tones; a tone block holds {TONES_PER_PROGRAM}"
                ),
            ));
        }
        if tones > 0 {
            with_tones.push(program);
        }
    }
    let declared = le16(bytes, 0x12) as usize;
    if declared != with_tones.len() {
        return Err(VabError::header(
            0x12,
            format!(
                "the header declares {declared} programs, but {} program entries have tones; the tone table holds one {TONE_BLOCK_BYTES:#x}-byte block per program with tones, so its length would be ambiguous",
                with_tones.len()
            ),
        ));
    }
    Ok((
        TONE_TABLE + with_tones.len() * TONE_BLOCK_BYTES + VAG_TABLE_BYTES,
        with_tones,
    ))
}

/// What a VH header declares about its own length and its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderFacts {
    pub header_bytes: usize,
    pub programs_with_tones: usize,
    pub vag_count: usize,
    /// Sum of the VAG size table: the body bytes the regions occupy.
    pub region_bytes: usize,
    pub declared_total: u32,
}

/// Reads a header's declared layout without a body, for discovery.
pub fn header_facts(bytes: &[u8]) -> Result<HeaderFacts, VabError> {
    let (header_bytes, programs) = header_length(bytes)?;
    if bytes.len() < header_bytes {
        return Err(VabError::header(
            bytes.len(),
            format!(
                "the header with its tone blocks and VAG table ends at {header_bytes:#x}, but only {} bytes follow",
                bytes.len()
            ),
        ));
    }
    let table = header_bytes - VAG_TABLE_BYTES;
    let region_bytes = (0..VAG_SLOTS)
        .map(|slot| le16(bytes, table + slot * 2) as usize * VAG_SIZE_UNIT)
        .sum();
    Ok(HeaderFacts {
        header_bytes,
        programs_with_tones: programs.len(),
        vag_count: le16(bytes, 0x16) as usize,
        region_bytes,
        declared_total: le32(bytes, 0x0C),
    })
}

/// load_instrument_source discovery: a VH, a combined VAB, or VAB headers
/// inside a larger container. `None` when the bytes hold no pBAV header.
pub fn discovery_hint(bytes: &[u8]) -> Option<Value> {
    if bytes.starts_with(b"VABp") {
        return Some(json!({"format": "vab_variant", "magic": "VABp",
            "note": "byte-reversed (big-endian) VAB header; retained but not interpreted, load_vab_bank admits only pBAV"}));
    }
    if bytes.starts_with(MAGIC) {
        let mut hint = json!({"format": "vab", "load_with": "load_vab_bank"});
        match header_facts(bytes) {
            Ok(f) => {
                let facts = json!({"header_bytes": f.header_bytes, "programs_with_tones": f.programs_with_tones,
                    "vags": f.vag_count, "body_bytes_expected": f.region_bytes, "total_size": f.declared_total});
                hint["header"] = facts;
                let extra = bytes.len() - f.header_bytes;
                if extra == 0 {
                    hint["packing"] = json!("separate");
                    hint["note"] = json!(
                        "a VH alone: retain its VB too and pass {\"header\": {\"source\": <this>}, \"body\": {\"source\": <VB>}}; a VB has no header, so it is not recognized from its own bytes"
                    );
                } else {
                    hint["packing"] = json!("combined");
                    hint["body_bytes_present"] = json!(extra);
                    hint["note"] = json!(if f.declared_total as usize == bytes.len()
                        && extra == f.region_bytes
                    {
                        "a combined VAB: pass {\"header\": {\"source\": <this>}} alone"
                    } else {
                        "bytes follow the header but do not match its total size and region sizes; pass header (and body) offset and length ranges"
                    });
                }
            }
            Err(e) => hint["unusable"] = json!(e.to_string()),
        }
        return Some(hint);
    }
    // VAB headers inside a container; a match must parse as a header. The
    // first rows are detailed; every header (up to a bound) is listed as
    // [offset, header_bytes, body_bytes_expected] so none is unreachable.
    const DETAILED: usize = 64;
    const LISTED: usize = 4096;
    let mut found = Vec::new();
    let mut all = Vec::new();
    let mut count = 0usize;
    let mut at = 1;
    while let Some(i) = bytes
        .get(at..)
        .and_then(|rest| rest.windows(4).position(|w| w == MAGIC))
    {
        let offset = at + i;
        if let Ok(f) = header_facts(&bytes[offset..]) {
            count += 1;
            if found.len() < DETAILED {
                found.push(json!({"offset": offset, "header_bytes": f.header_bytes, "body_bytes_expected": f.region_bytes, "total_size": f.declared_total}));
            }
            if all.len() < LISTED {
                all.push(json!([offset, f.header_bytes, f.region_bytes]));
            }
        }
        at = offset + 4;
    }
    (count > 0).then(|| {
        json!({"format": "vab_headers_inside", "load_with": "load_vab_bank", "count": count, "first": found,
            "first_truncated": count > DETAILED,
            "headers": {"columns": ["offset", "header_bytes", "body_bytes_expected"], "rows": all,
                "listed": all.len(), "truncated": count > LISTED},
            "truncated": count > LISTED,
            "note": "VAB headers inside this container; pass each as header {source, offset, length: header_bytes} and its body as {source, offset, length: body_bytes_expected}. The body's start is not stored in the header (observed containers place it at the next 2048-byte boundary); check the loaded bank's summary"})
    })
}

impl Vab {
    /// Splits a combined VAB at the header length its program table declares.
    pub fn parse_combined(bytes: &[u8]) -> Result<Vab, VabError> {
        let (length, _) = header_length(bytes)?;
        if bytes.len() < length {
            return Err(VabError::header(
                bytes.len(),
                format!(
                    "the header with its tone blocks and VAG table ends at {length:#x}, but the slice holds {} bytes",
                    bytes.len()
                ),
            ));
        }
        Self::parse_parts(
            bytes[..length].to_vec(),
            bytes[length..].to_vec(),
            Packing::Combined,
        )
    }

    /// A separately supplied VH and VB.
    pub fn parse(header: Vec<u8>, body: Vec<u8>) -> Result<Vab, VabError> {
        Self::parse_parts(header, body, Packing::Separate)
    }

    fn parse_parts(header: Vec<u8>, body: Vec<u8>, packing: Packing) -> Result<Vab, VabError> {
        let (length, tone_programs) = header_length(&header)?;
        if header.len() != length {
            return Err(VabError::header(
                header.len().min(length),
                if header.len() > length {
                    format!(
                        "the VH holds {} bytes, {} more than the header, tone blocks and VAG table it declares ({length:#x}); if the body follows, load the slice as a combined VAB",
                        header.len(),
                        header.len() - length
                    )
                } else {
                    format!(
                        "the header with its tone blocks and VAG table ends at {length:#x}, but the VH holds {} bytes",
                        header.len()
                    )
                },
            ));
        }
        let tones: usize = (0..PROGRAMS)
            .map(|p| header[PROGRAM_TABLE + p * PROGRAM_BYTES] as usize)
            .sum();
        let declared_tones = le16(&header, 0x14) as usize;
        if declared_tones != tones {
            return Err(VabError::header(
                0x14,
                format!(
                    "the header declares {declared_tones} tones, but the program entries' tone counts sum to {tones}"
                ),
            ));
        }
        let vag_count = le16(&header, 0x16) as usize;
        if vag_count >= VAG_SLOTS {
            return Err(VabError::header(
                0x16,
                format!(
                    "the header declares {vag_count} VAGs; the size table has slots 1..{} after slot 0",
                    VAG_SLOTS - 1
                ),
            ));
        }
        let table = length - VAG_TABLE_BYTES;
        let mut regions = Vec::with_capacity(VAG_SLOTS);
        let mut start = 0usize;
        for slot in 0..VAG_SLOTS {
            let units = le16(&header, table + slot * 2) as usize;
            if slot > vag_count && units != 0 {
                return Err(VabError::header(
                    table + slot * 2,
                    format!(
                        "VAG size table slot {slot} holds {units} units, but the header declares {vag_count} VAGs; a region beyond the count would be unaddressed"
                    ),
                ));
            }
            let bytes = units * VAG_SIZE_UNIT;
            regions.push(Region { start, bytes });
            start += bytes;
        }
        if body.len() < start {
            let short = regions
                .iter()
                .position(|r| r.start + r.bytes > body.len())
                .expect("some region ends past the body");
            return Err(VabError::body(
                body.len(),
                format!(
                    "the VAG size table places {start} bytes of regions, but the VB holds {}; slot {short} at body byte {} (table entry at header byte {:#x}) runs past it",
                    body.len(),
                    regions[short].start,
                    table + short * 2
                ),
            ));
        }
        Ok(Vab {
            trailing: body.len() - start,
            header,
            body,
            packing,
            tone_programs,
            regions,
            vag_count,
        })
    }

    /// The bytes as supplied: VH and VB, or the combined file.
    pub fn combined_bytes(&self) -> Vec<u8> {
        let mut out = self.header.clone();
        out.extend(&self.body);
        out
    }

    pub fn vag_table_offset(&self) -> usize {
        self.header.len() - VAG_TABLE_BYTES
    }

    pub fn declared_total(&self) -> u32 {
        le32(&self.header, 0x0C)
    }

    pub fn total_matches(&self) -> bool {
        self.declared_total() as usize == self.header.len() + self.body.len()
    }

    /// Header fields with their offsets and the consistency checks.
    pub fn summary(&self) -> Value {
        let h = &self.header;
        let version = le32(h, 4);
        let loops = (1..=self.vag_count)
            .filter_map(|v| self.sample_stream(v).ok())
            .fold(
                json!({"loop": 0, "end_mute": 0, "silent_loop_block": 0}),
                |mut acc, (s, _)| {
                    let key = match s.ending {
                        psx_adpcm::Ending::Loop { .. } => "loop",
                        psx_adpcm::Ending::EndMute => "end_mute",
                        psx_adpcm::Ending::SilentLoopBlock => "silent_loop_block",
                    };
                    acc[key] = json!(acc[key].as_u64().unwrap_or(0) + 1);
                    acc
                },
            );
        let undecodable: Vec<usize> = (1..=self.vag_count)
            .filter(|&v| self.sample_stream(v).is_err())
            .collect();
        json!({
            "packing": self.packing,
            "header_bytes": h.len(),
            "body_bytes": self.body.len(),
            "fields": {
                "magic": field(0, "pBAV"),
                "version": field(4, version),
                "vab_id": field(8, le32(h, 8)),
                "total_size": field(0x0C, self.declared_total()),
                "reserved_0x10": field(0x10, format!("{:#06x}", le16(h, 0x10))),
                "programs": field(0x12, le16(h, 0x12)),
                "tones": field(0x14, le16(h, 0x14)),
                "vags": field(0x16, le16(h, 0x16)),
                "master_volume": field(0x18, h[0x18]),
                "master_pan": field(0x19, h[0x19]),
                "bank_attribute_1": field(0x1A, h[0x1A]),
                "bank_attribute_2": field(0x1B, h[0x1B]),
                "reserved_0x1c": field(0x1C, format!("{:#010x}", le32(h, 0x1C))),
            },
            "header_hex": hex(&h[..BANK_HEADER_BYTES]),
            "version_documented": DOCUMENTED_VERSIONS.contains(&version),
            "layout": {
                "program_table": {"offset": PROGRAM_TABLE, "bytes": PROGRAMS * PROGRAM_BYTES},
                "tone_table": {"offset": TONE_TABLE, "blocks": self.tone_programs.len(), "block_bytes": TONE_BLOCK_BYTES, "block_programs": self.tone_programs},
                "vag_size_table": {"offset": self.vag_table_offset(), "slots": VAG_SLOTS, "unit_bytes": VAG_SIZE_UNIT},
                "programs_field": "count of program entries with tones (one tone block each, in program order)",
            },
            "checks": {
                "total_size_matches": self.total_matches(),
                "total_size_expected": self.header.len() + self.body.len(),
                "slot_0_bytes": self.regions[0].bytes,
                "body_trailing_bytes": self.trailing,
                "body_within_documented_budget": self.body.len() <= DOCUMENTED_BODY_BUDGET,
                "dangling_tone_references": self.dangling_tones(),
                "parent_program_mismatches": self.parent_mismatches(),
            },
            "samples": {"count": self.vag_count, "endings": loops, "undecodable": undecodable},
        })
    }

    fn tone_slots(&self) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        self.tone_programs
            .iter()
            .enumerate()
            .flat_map(move |(block, &program)| {
                let count = self.header[PROGRAM_TABLE + program * PROGRAM_BYTES] as usize;
                (0..count).map(move |tone| (program, tone, block))
            })
    }

    fn dangling_tones(&self) -> Vec<Value> {
        self.tone_slots()
            .filter_map(|(program, tone, _)| {
                let at = self.tone_offset(program, tone)?;
                let vag = le16(&self.header, at + 0x16) as usize;
                (vag == 0 || vag > self.vag_count).then(
                    || json!({"program": program, "tone": tone, "vag": vag, "offset": at + 0x16}),
                )
            })
            .collect()
    }

    fn parent_mismatches(&self) -> Vec<Value> {
        self.tone_slots()
            .filter_map(|(program, tone, _)| {
                let at = self.tone_offset(program, tone)?;
                let parent = le16(&self.header, at + 0x14) as usize;
                (parent != program).then(|| json!({"program": program, "tone": tone, "parent_program": parent, "offset": at + 0x14}))
            })
            .collect()
    }

    /// Header offset of a used tone slot (program with tones, tone below its count).
    pub fn tone_offset(&self, program: usize, tone: usize) -> Option<usize> {
        let block = self.tone_programs.iter().position(|&p| p == program)?;
        let count = self.header[PROGRAM_TABLE + program * PROGRAM_BYTES] as usize;
        (tone < count).then_some(TONE_TABLE + block * TONE_BLOCK_BYTES + tone * TONE_BYTES)
    }

    /// One program entry, every field with its offset.
    pub fn program(&self, program: usize) -> Value {
        let at = PROGRAM_TABLE + program * PROGRAM_BYTES;
        let p = &self.header[at..at + PROGRAM_BYTES];
        let block = self.tone_programs.iter().position(|&q| q == program);
        json!({
            "program": program,
            "offset": at,
            "raw": hex(p),
            "used": p[0] > 0,
            "tone_block": block.map(|b| json!({"index": b, "offset": TONE_TABLE + b * TONE_BLOCK_BYTES})),
            "fields": {
                "tones": field(at, p[0]),
                "master_volume": field(at + 1, p[1]),
                "priority": field(at + 2, p[2]),
                "mode": field(at + 3, p[3]),
                "master_pan": field(at + 4, p[4]),
                "reserved0": field(at + 5, p[5]),
                "attributes": field(at + 6, format!("{:#06x}", le16(p, 6))),
                "reserved1": field(at + 8, format!("{:#010x}", le32(p, 8))),
                "reserved2": field(at + 12, format!("{:#010x}", le32(p, 12))),
            },
        })
    }

    /// Every tone record of a tone block, used or not.
    pub fn tone_block(&self, block: usize) -> Vec<Value> {
        let program = self.tone_programs[block];
        let count = self.header[PROGRAM_TABLE + program * PROGRAM_BYTES] as usize;
        (0..TONES_PER_PROGRAM)
            .map(|tone| {
                let at = TONE_TABLE + block * TONE_BLOCK_BYTES + tone * TONE_BYTES;
                self.tone_record(program, tone, at, tone < count)
            })
            .collect()
    }

    fn tone_record(&self, program: usize, tone: usize, at: usize, used: bool) -> Value {
        let t = &self.header[at..at + TONE_BYTES];
        let (adsr1, adsr2) = (le16(t, 0x10), le16(t, 0x12));
        let (parent, vag) = (le16(t, 0x14) as usize, le16(t, 0x16) as usize);
        let mut record = json!({
            "program": program,
            "tone": tone,
            "offset": at,
            "used": used,
            "raw": hex(t),
            "fields": {
                "priority": field(at, t[0]),
                "mode": field(at + 1, t[1]),
                "volume": field(at + 2, t[2]),
                "pan": field(at + 3, t[3]),
                "center_note": field(at + 4, t[4]),
                "fine_tune": field(at + 5, t[5]),
                "note_min": field(at + 6, t[6]),
                "note_max": field(at + 7, t[7]),
                "vibrato_width": field(at + 8, t[8]),
                "vibrato_time": field(at + 9, t[9]),
                "portamento_width": field(at + 10, t[10]),
                "portamento_time": field(at + 11, t[11]),
                "pitch_bend_min": field(at + 12, t[12]),
                "pitch_bend_max": field(at + 13, t[13]),
                "reserved_0x0e": field(at + 14, t[14]),
                "reserved_0x0f": field(at + 15, t[15]),
                "adsr1": field(at + 0x10, format!("{adsr1:#06x}")),
                "adsr2": field(at + 0x12, format!("{adsr2:#06x}")),
                "parent_program": field(at + 0x14, parent),
                "vag": field(at + 0x16, vag),
                "reserved_0x18": field(at + 0x18, hex(&t[0x18..0x20])),
            },
            "adsr": adsr_fields(adsr1, adsr2),
        });
        if used {
            record["parent_program_matches"] = json!(parent == program);
            record["vag_resolved"] = json!(vag >= 1 && vag <= self.vag_count);
            if vag >= 1 && vag <= self.vag_count {
                let r = self.regions[vag];
                record["sample_region"] = json!({"body_offset": r.start, "bytes": r.bytes});
            }
        }
        record
    }

    /// Used tones that reference a VAG.
    pub fn references(&self, vag: usize) -> Vec<Value> {
        self.tone_slots()
            .filter(|&(program, tone, _)| {
                self.tone_offset(program, tone)
                    .is_some_and(|at| le16(&self.header, at + 0x16) as usize == vag)
            })
            .map(|(program, tone, _)| json!({"program": program, "tone": tone}))
            .collect()
    }

    /// The region bytes of a VAG (1..=vag_count).
    pub fn region_bytes(&self, vag: usize) -> Result<&[u8], String> {
        if vag == 0 || vag > self.vag_count {
            return Err(format!(
                "VAG {vag} does not exist; this bank has VAGs 1..={}",
                self.vag_count
            ));
        }
        let r = self.regions[vag];
        Ok(&self.body[r.start..r.start + r.bytes])
    }

    /// The SPU stream of a VAG: from the region start through its first
    /// loop-end block, and the bytes after that block.
    pub fn sample_stream(&self, vag: usize) -> Result<(Stream, usize), String> {
        let region = self.region_bytes(vag)?;
        let start = self.regions[vag].start;
        stream_of(region, start)
    }

    /// Interpretation of one VAG region with its decoded first pass.
    pub fn sample(&self, vag: usize) -> Result<(Value, Option<Stream>), String> {
        let region = self.region_bytes(vag)?;
        let r = self.regions[vag];
        let mut report = json!({
            "vag": vag,
            "size_table_entry": {"offset": self.vag_table_offset() + vag * 2, "units": r.bytes / VAG_SIZE_UNIT},
            "body_offset": r.start,
            "bytes": r.bytes,
            "region_sha256": crate::store::sha256_hex(region),
            "leading_zero_block": region.len() >= BLOCK_BYTES && region[..BLOCK_BYTES].iter().all(|&b| b == 0),
            "referenced_by": self.references(vag),
        });
        match stream_of(region, r.start) {
            Ok((stream, after)) => {
                let stream_bytes = (stream.end_block + 1) * BLOCK_BYTES;
                report["decodable"] = json!(true);
                report["stream"] = stream.report().clone();
                let at = |block: usize| json!({"block": block, "region_offset": block * BLOCK_BYTES, "body_offset": r.start + block * BLOCK_BYTES});
                report["offsets"] = json!({
                    "base": "stream.end_offset, stream.loop_start_offset, body_offset and after_end.body_offset count bytes from the start of the VB (body); region_offset counts from this region's first byte; source_offset (when the bank was loaded from a source) is the byte in that retained source",
                    "end": at(stream.end_block),
                    "loop_start": stream.loop_start_block().map(at),
                });
                report["stream_bytes"] = json!(stream_bytes);
                report["after_end"] = json!({
                    "bytes": after,
                    "body_offset": r.start + stream_bytes,
                    "first_block_hex": (after >= BLOCK_BYTES).then(|| hex(&region[stream_bytes..stream_bytes + BLOCK_BYTES])),
                    "played": false,
                    "meaning": "bytes after the first loop-end block; the voice repeats or is released there, so they are retained but not decoded",
                });
                Ok((report, Some(stream)))
            }
            Err(reason) => {
                report["decodable"] = json!(false);
                report["reason"] = json!(reason);
                Ok((report, None))
            }
        }
    }

    /// A copy whose VAG region is replaced. `fixed` refuses a size change;
    /// otherwise later regions shift and the size table and (when it
    /// matched) the total size are rewritten. Returns the bank and every
    /// changed header offset with old and new bytes.
    pub fn with_region(
        &self,
        vag: usize,
        region: &[u8],
        fixed: bool,
    ) -> Result<(Vab, Value), String> {
        self.region_bytes(vag)?;
        if region.is_empty() || !region.len().is_multiple_of(BLOCK_BYTES) {
            return Err(format!(
                "a replacement region needs whole 16-byte SPU ADPCM blocks; it holds {} bytes",
                region.len()
            ));
        }
        stream_of(region, 0).map_err(|e| format!("the replacement region does not decode: {e}"))?;
        let old = self.regions[vag];
        if fixed && region.len() != old.bytes {
            return Err(format!(
                "VAG {vag} occupies {} bytes at body byte {}; the replacement holds {} ({:+}). layout fixed keeps every region offset, so the size must match; choose layout resize to shift later regions",
                old.bytes,
                old.start,
                region.len(),
                region.len() as i64 - old.bytes as i64
            ));
        }
        let units = region.len() / VAG_SIZE_UNIT;
        if units > u16::MAX as usize {
            return Err(format!(
                "the replacement holds {} bytes; a size-table entry counts at most {} bytes",
                region.len(),
                u16::MAX as usize * VAG_SIZE_UNIT
            ));
        }
        let mut body = Vec::with_capacity(self.body.len() + region.len());
        body.extend(&self.body[..old.start]);
        body.extend(region);
        body.extend(&self.body[old.start + old.bytes..]);
        if body.len() > SPU_SAMPLE_RAM && body.len() > self.body.len() {
            return Err(format!(
                "the body would hold {} bytes, more than the {SPU_SAMPLE_RAM} bytes of SPU RAM outside the capture buffers",
                body.len()
            ));
        }
        let mut header = self.header.clone();
        let table = self.vag_table_offset() + vag * 2;
        header[table..table + 2].copy_from_slice(&(units as u16).to_le_bytes());
        let total_tracks = self.total_matches();
        if total_tracks {
            let total = u32::try_from(header.len() + body.len())
                .map_err(|_| "total size exceeds 32 bits")?;
            header[0x0C..0x10].copy_from_slice(&total.to_le_bytes());
        }
        let changed: Vec<Value> = (0..header.len())
            .filter(|&i| header[i] != self.header[i])
            .map(|i| json!({"offset": i, "old": self.header[i], "new": header[i]}))
            .collect();
        let next = Self::parse_parts(header, body, self.packing).map_err(|e| e.to_string())?;
        let shifted: Vec<Value> = (vag + 1..=self.vag_count)
            .filter(|&v| next.regions[v].start != self.regions[v].start)
            .map(|v| json!({"vag": v, "old_body_offset": self.regions[v].start, "new_body_offset": next.regions[v].start}))
            .collect();
        let report = json!({
            "vag": vag,
            "old_region": {"body_offset": old.start, "bytes": old.bytes, "sha256": crate::store::sha256_hex(&self.body[old.start..old.start + old.bytes])},
            "new_region": {"body_offset": old.start, "bytes": region.len(), "sha256": crate::store::sha256_hex(region)},
            "size_change": region.len() as i64 - old.bytes as i64,
            "changed_header_bytes": changed,
            "total_size": if !total_tracks {
                json!({"updated": false, "value": next.declared_total(), "reason": "the source's total size did not equal VH plus VB, so it is kept as found"})
            } else if next.declared_total() != self.declared_total() {
                json!({"updated": true, "old": self.declared_total(), "new": next.declared_total()})
            } else {
                json!({"updated": false, "value": next.declared_total(), "reason": "VH plus VB keeps its size, so the matching total size is unchanged"})
            },
            "shifted_regions": shifted,
            "body_bytes": {"old": self.body.len(), "new": next.body.len()},
            "body_within_documented_budget": next.body.len() <= DOCUMENTED_BODY_BUDGET,
        });
        Ok((next, report))
    }

    /// A copy with tone bytes rewritten; `edits` are (header offset, value).
    pub fn with_header_bytes(&self, edits: &[(usize, u8)]) -> Result<Vab, String> {
        let mut header = self.header.clone();
        for &(at, value) in edits {
            header[at] = value;
        }
        Self::parse_parts(header, self.body.clone(), self.packing).map_err(|e| e.to_string())
    }
}

/// Flag bytes that differ between two regions, block by block, and how many
/// region bytes differ at all. `body_offset` is where both regions start.
pub fn region_differences(
    old: &[u8],
    new: &[u8],
    body_offset: usize,
) -> (usize, Vec<(usize, usize, u8, u8)>) {
    let common = old.len().min(new.len());
    let bytes =
        (0..common).filter(|&i| old[i] != new[i]).count() + old.len().max(new.len()) - common;
    let flags = (0..common / BLOCK_BYTES)
        .filter(|&b| old[b * BLOCK_BYTES + 1] != new[b * BLOCK_BYTES + 1])
        .map(|b| {
            (
                b,
                body_offset + b * BLOCK_BYTES + 1,
                old[b * BLOCK_BYTES + 1],
                new[b * BLOCK_BYTES + 1],
            )
        })
        .collect();
    (bytes, flags)
}

/// Names of the set flag bits of an SPU ADPCM flag byte.
pub fn flag_names(flags: u8) -> Vec<&'static str> {
    let mut names = Vec::new();
    if flags & LOOP_END != 0 {
        names.push("loop_end");
    }
    if flags & psx_adpcm::LOOP_REPEAT != 0 {
        names.push("repeat");
    }
    if flags & psx_adpcm::LOOP_START != 0 {
        names.push("loop_start");
    }
    if flags & 0xF8 != 0 {
        names.push("unknown_bits_3_7");
    }
    names
}

/// Parses a region through its first loop-end block; returns the stream
/// and the count of bytes after that block.
pub fn stream_of(region: &[u8], base: usize) -> Result<(Stream, usize), String> {
    if !region.len().is_multiple_of(BLOCK_BYTES) {
        return Err(format!(
            "the region holds {} bytes, not whole 16-byte blocks",
            region.len()
        ));
    }
    let end = region
        .chunks(BLOCK_BYTES)
        .position(|b| b[1] & LOOP_END != 0)
        .ok_or_else(|| {
            format!(
                "no block of the region at body byte {base} has the loop-end flag; the SPU would read past it"
            )
        })?;
    let stream_bytes = (end + 1) * BLOCK_BYTES;
    let stream = Stream::parse(
        &region[..stream_bytes],
        base,
        None,
        false,
        (end + 1) * BLOCK_SAMPLES,
    )?;
    Ok((stream, region.len() - stream_bytes))
}

/// How a tone's center note and fine tune set the pitch register. Field names
/// and 0..127 ranges do not establish the sign; this interpretation is inferred
/// from looped tones and remains distinct from hardware verification.
pub const PITCH_READING: &str = "Tunemend's declared reading: key = center note with fine tune 0 plays at SPU pitch 0x1000 (44100 Hz), and each fine-tune unit raises the played pitch by 1/128 semitone: pitch = 4096 * 2^((key - center + fine_tune/128) / 12). The sources give field names and ranges only; the sign is inferred from looped tones in real banks, which sound nearest whole semitones under it";

/// Exact pitch register value for a key under [`PITCH_READING`].
pub fn note_pitch(note: u8, center: u8, fine_tune: u8) -> f64 {
    let semitones = note as f64 - center as f64 + fine_tune as f64 / 128.0;
    4096.0 * (semitones / 12.0).exp2()
}

/// Center note and fine tune that play a sample recorded at `rate` with
/// `root` (fractional MIDI note) in tune under [`PITCH_READING`].
pub fn tone_tuning(rate: f64, root: f64) -> (f64, f64, f64) {
    // center - fine/128 = root + 12 * log2(44100 / rate)
    let exact = root + 12.0 * (psx_adpcm::SPU_RATE as f64 / rate).log2();
    let mut center = exact.ceil();
    let mut fine = ((center - exact) * 128.0).round();
    if fine >= 128.0 {
        center -= 1.0;
        fine = 0.0;
    }
    (center, fine, exact)
}

/// ADSR1 and ADSR2 as the SPU register fields they hold.
pub fn adsr_fields(adsr1: u16, adsr2: u16) -> Value {
    let mode = |bit: bool| if bit { "exponential" } else { "linear" };
    json!({
        "attack": {"mode": mode(adsr1 & 0x8000 != 0), "shift": (adsr1 >> 10) & 0x1F, "step": (adsr1 >> 8) & 3},
        "decay": {"mode": "exponential", "shift": (adsr1 >> 4) & 0xF},
        "sustain_level": {"value": adsr1 & 0xF, "level": ((adsr1 & 0xF) as u32 + 1) * 0x800},
        "sustain": {"mode": mode(adsr2 & 0x8000 != 0), "direction": if adsr2 & 0x4000 != 0 { "decrease" } else { "increase" },
            "bit_13": (adsr2 >> 13) & 1, "shift": (adsr2 >> 8) & 0x1F, "step": (adsr2 >> 6) & 3},
        "release": {"mode": mode(adsr2 & 0x20 != 0), "shift": adsr2 & 0x1F},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-shot region: zero block, one data block with loop end, and
    /// the after-end block the observed banks carry.
    pub fn one_shot_region() -> Vec<u8> {
        let mut r = vec![0u8; 16];
        r.extend([0x0C, 0x01]);
        r.extend([0x21u8; 14]);
        r.extend([0x00, 0x07]);
        r.extend([0x77u8; 14]);
        r
    }

    /// A looping region: zero block, loop start, loop end with repeat.
    pub fn loop_region() -> Vec<u8> {
        let mut r = vec![0u8; 16];
        r.extend([0x0A, 0x04]);
        r.extend([0x13u8; 14]);
        r.extend([0x1A, 0x03]);
        r.extend([0x31u8; 14]);
        r
    }

    /// Programs 0 and 3 have tones (non-contiguous), VAGs 1 and 2.
    pub fn bank() -> (Vec<u8>, Vec<u8>) {
        let regions = [one_shot_region(), loop_region()];
        let mut h = vec![0u8; TONE_TABLE + 2 * TONE_BLOCK_BYTES + VAG_TABLE_BYTES];
        h[..4].copy_from_slice(MAGIC);
        h[4..8].copy_from_slice(&7u32.to_le_bytes());
        h[0x10..0x12].copy_from_slice(&0xEEEEu16.to_le_bytes());
        h[0x12..0x14].copy_from_slice(&2u16.to_le_bytes());
        h[0x14..0x16].copy_from_slice(&3u16.to_le_bytes());
        h[0x16..0x18].copy_from_slice(&2u16.to_le_bytes());
        h[0x18] = 0x7F;
        h[0x19] = 0x40;
        h[0x1C..0x20].fill(0xFF);
        for p in 0..PROGRAMS {
            let at = PROGRAM_TABLE + p * PROGRAM_BYTES;
            h[at + 1] = 0xFF;
            h[at + 8..at + 16].fill(0xFF);
        }
        h[PROGRAM_TABLE] = 2;
        h[PROGRAM_TABLE + 3 * PROGRAM_BYTES] = 1;
        let tone = |h: &mut Vec<u8>, block: usize, slot: usize, program: u16, vag: u16| {
            let at = TONE_TABLE + block * TONE_BLOCK_BYTES + slot * TONE_BYTES;
            h[at + 2] = 100;
            h[at + 3] = 64;
            h[at + 4] = 60;
            h[at + 7] = 127;
            h[at + 14] = 0xB1;
            h[at + 15] = 0xB2;
            h[at + 0x10..at + 0x12].copy_from_slice(&0x00FFu16.to_le_bytes());
            h[at + 0x12..at + 0x14].copy_from_slice(&0xDFE0u16.to_le_bytes());
            h[at + 0x14..at + 0x16].copy_from_slice(&program.to_le_bytes());
            h[at + 0x16..at + 0x18].copy_from_slice(&vag.to_le_bytes());
        };
        tone(&mut h, 0, 0, 0, 1);
        tone(&mut h, 0, 1, 0, 2);
        tone(&mut h, 1, 0, 3, 2);
        let table = h.len() - VAG_TABLE_BYTES;
        for (i, r) in regions.iter().enumerate() {
            let units = (r.len() / 8) as u16;
            h[table + (i + 1) * 2..table + (i + 1) * 2 + 2].copy_from_slice(&units.to_le_bytes());
        }
        let body: Vec<u8> = regions.concat();
        let total = (h.len() + body.len()) as u32;
        h[0x0C..0x10].copy_from_slice(&total.to_le_bytes());
        (h, body)
    }

    #[test]
    fn interprets_non_contiguous_programs_and_regions() {
        let (h, b) = bank();
        let vab = Vab::parse(h, b).unwrap();
        assert_eq!(vab.tone_programs, vec![0, 3]);
        assert_eq!(
            vab.regions[1],
            Region {
                start: 0,
                bytes: 48
            }
        );
        assert_eq!(
            vab.regions[2],
            Region {
                start: 48,
                bytes: 48
            }
        );
        assert!(vab.total_matches());
        assert_eq!(vab.tone_offset(3, 0), Some(TONE_TABLE + TONE_BLOCK_BYTES));
        assert_eq!(vab.tone_offset(3, 1), None);
        assert_eq!(vab.references(2).len(), 2);
        let (one, stream) = vab.sample(1).unwrap();
        assert_eq!(one["stream"]["ending"], "end_mute");
        assert_eq!(one["after_end"]["bytes"], 16);
        assert_eq!(one["leading_zero_block"], true);
        assert_eq!(stream.unwrap().first_pass().len(), 56);
        let (looped, _) = vab.sample(2).unwrap();
        assert_eq!(looped["stream"]["ending"], "loop");
        assert_eq!(looped["stream"]["loop_start_block"], 1);
        let summary = vab.summary();
        assert_eq!(summary["samples"]["endings"]["loop"], 1);
        assert_eq!(summary["fields"]["programs"]["value"], 2);
    }

    #[test]
    fn tuning_reading_round_trips() {
        assert_eq!(note_pitch(72, 72, 0), 4096.0);
        assert_eq!(note_pitch(60, 72, 0), 2048.0);
        // 64 units raise the pitch by half a semitone.
        assert!((note_pitch(72, 72, 64) - 4096.0 * (0.5f64 / 12.0).exp2()).abs() < 1e-9);
        assert_eq!(tone_tuning(22050.0, 60.0), (72.0, 0.0, 72.0));
        // 32000 Hz at root 60: the center sits 5.553 semitones above it.
        let (center, fine, exact) = tone_tuning(32000.0, 60.0);
        assert_eq!(center, 66.0);
        assert!((note_pitch(60, 66, fine as u8) / 4096.0 - 32000.0 / 44100.0).abs() < 0.003);
        assert!(exact < center);
    }

    #[test]
    fn combined_splits_at_the_declared_header_length() {
        let (h, b) = bank();
        let mut all = h.clone();
        all.extend(&b);
        let vab = Vab::parse_combined(&all).unwrap();
        assert_eq!(vab.header, h);
        assert_eq!(vab.body, b);
        assert_eq!(vab.combined_bytes(), all);
    }

    #[test]
    fn located_refusals() {
        let (h, b) = bank();
        let refuse = |h: &[u8], b: &[u8]| Vab::parse(h.to_vec(), b.to_vec()).unwrap_err();
        let mut bad = h.clone();
        bad[..4].copy_from_slice(b"VABp");
        assert_eq!(refuse(&bad, &b).offset, 0);
        let mut bad = h.clone();
        bad[0x12] = 3;
        assert_eq!(refuse(&bad, &b).offset, 0x12);
        let mut bad = h.clone();
        bad[0x14] = 9;
        assert_eq!(refuse(&bad, &b).offset, 0x14);
        let mut bad = h.clone();
        bad[PROGRAM_TABLE] = 17;
        assert_eq!(refuse(&bad, &b).offset, PROGRAM_TABLE);
        let mut bad = h.clone();
        let table = bad.len() - VAG_TABLE_BYTES;
        bad[table + 6] = 1;
        assert_eq!(refuse(&bad, &b).offset, table + 6);
        let e = refuse(&h, &b[..80]);
        assert_eq!((e.part, e.offset), ("body", 80));
        let mut long = h.clone();
        long.push(0);
        assert!(refuse(&long, &b).message.contains("combined"));
        // Trailing body bytes are retained, not refused.
        let mut more = b.clone();
        more.extend([1, 2, 3]);
        assert_eq!(Vab::parse(h, more).unwrap().trailing, 3);
    }

    #[test]
    fn region_replacement_accounts_for_sizes_and_offsets() {
        let (h, b) = bank();
        let vab = Vab::parse(h, b).unwrap();
        let mut bigger = one_shot_region();
        bigger.splice(16..16, [0x0C, 0x00].into_iter().chain([0x55u8; 14]));
        assert!(
            vab.with_region(1, &bigger, true)
                .unwrap_err()
                .contains("fixed")
        );
        let (next, report) = vab.with_region(1, &bigger, false).unwrap();
        assert_eq!(next.regions[2].start, 64);
        assert_eq!(next.body[64..], vab.body[48..]);
        assert_eq!(report["shifted_regions"][0]["new_body_offset"], 64);
        assert!(next.total_matches());
        let changed: Vec<u64> = report["changed_header_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["offset"].as_u64().unwrap())
            .collect();
        let table = (vab.vag_table_offset() + 2) as u64;
        assert!(
            changed
                .iter()
                .all(|&o| o == table || (0x0C..0x10).contains(&o))
        );
        let mut no_end = one_shot_region();
        no_end[17] = 0;
        no_end[33] = 0;
        assert!(
            vab.with_region(1, &no_end, true)
                .unwrap_err()
                .contains("loop-end")
        );
    }
}
