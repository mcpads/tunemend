//! Nintendo DS sound data containers: the common block header, SDAT
//! archives (SYMB/INFO/FAT/FILE), SWAR wave archives, standalone SWAV,
//! SBNK instrument banks and STRM streams, interpreted from GBATEK's "DS
//! Files - Sound" chapter and kiwi.ds's SDAT specification. Every parse is
//! bounded by the bytes present and refuses with the offending byte offset;
//! nothing is repaired. Sample decoding is in [`crate::nds_adpcm`]. SSEQ and
//! SSAR are recognized by their header only.
use crate::nds_adpcm::{self, Header as AdpcmHeader, State};
use serde_json::{Value, json};

/// ARM7 clock / 2, the documented SWAV `Time` numerator (33.513982 MHz / 2).
pub const ARM7_CLOCK_HALF_HZ: f64 = 33_513_982.0 / 2.0;
/// Byte order mark as a little-endian u16 (stored FF FE).
pub const BOM: u16 = 0xFEFF;
pub const DOCUMENTED_VERSION: u16 = 0x0100;
pub const HEADER_BYTES: usize = 16;
/// SWAV sample-block info bytes before the data.
pub const WAVE_INFO_BYTES: usize = 12;
/// Decoded samples per stream channel that inspection and export accept.
pub const MAX_STREAM_SAMPLES: usize = 1 << 25;
const MAX_NAME: usize = 256;

/// A byte range `[start, end)`.
pub type Range = (usize, usize);
/// A bank's wave-archive slot: the INFO wave-archive index and its FAT file.
pub type WaveArchiveSlot = (usize, Option<u16>);

/// A refusal at a byte offset of the interpreted file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdsError {
    pub offset: usize,
    pub field: &'static str,
    pub message: String,
}

impl std::fmt::Display for NdsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "byte {} (0x{:x}), {}: {}",
            self.offset, self.offset, self.field, self.message
        )
    }
}

fn err(offset: usize, field: &'static str, message: impl Into<String>) -> NdsError {
    NdsError {
        offset,
        field,
        message: message.into(),
    }
}

fn u8_at(b: &[u8], at: usize, end: usize, field: &'static str) -> Result<u8, NdsError> {
    if at < end.min(b.len()) {
        Ok(b[at])
    } else {
        Err(err(at, field, "past the end of the data"))
    }
}
fn u16_at(b: &[u8], at: usize, end: usize, field: &'static str) -> Result<u16, NdsError> {
    if at + 2 <= end.min(b.len()) {
        Ok(u16::from_le_bytes([b[at], b[at + 1]]))
    } else {
        Err(err(at, field, "past the end of the data"))
    }
}
fn u32_at(b: &[u8], at: usize, end: usize, field: &'static str) -> Result<u32, NdsError> {
    if at + 4 <= end.min(b.len()) {
        Ok(u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]))
    } else {
        Err(err(at, field, "past the end of the data"))
    }
}
fn tag_at(
    b: &[u8],
    at: usize,
    end: usize,
    tag: &[u8; 4],
    field: &'static str,
) -> Result<(), NdsError> {
    if at + 4 > end.min(b.len()) {
        return Err(err(at, field, "past the end of the data"));
    }
    if &b[at..at + 4] != tag {
        return Err(err(
            at,
            field,
            format!(
                "expected block tag {:?}, found {:?}",
                String::from_utf8_lossy(tag),
                String::from_utf8_lossy(&b[at..at + 4])
            ),
        ));
    }
    Ok(())
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// File kinds by their four-byte ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sdat,
    Swar,
    Swav,
    Sbnk,
    Strm,
    Sseq,
    Ssar,
}

impl Kind {
    pub fn from_magic(bytes: &[u8]) -> Option<Kind> {
        Some(match bytes.get(..4)? {
            b"SDAT" => Kind::Sdat,
            b"SWAR" => Kind::Swar,
            b"SWAV" => Kind::Swav,
            b"SBNK" => Kind::Sbnk,
            b"STRM" => Kind::Strm,
            b"SSEQ" => Kind::Sseq,
            b"SSAR" => Kind::Ssar,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Sdat => "sdat",
            Kind::Swar => "swar",
            Kind::Swav => "swav",
            Kind::Sbnk => "sbnk",
            Kind::Strm => "strm",
            Kind::Sseq => "sseq",
            Kind::Ssar => "ssar",
        }
    }
}

/// The common 16-byte header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub kind: Kind,
    pub bom: u16,
    pub version: u16,
    pub file_size: u32,
    pub header_size: u16,
    pub blocks: u16,
    /// Bytes after `file_size`; retained, not interpreted.
    pub trailing_bytes: usize,
}

impl FileHeader {
    pub fn parse(b: &[u8]) -> Result<Self, NdsError> {
        if b.len() < HEADER_BYTES {
            return Err(err(0, "header", "shorter than the 16-byte NDS file header"));
        }
        let kind = Kind::from_magic(b).ok_or_else(|| {
            err(
                0,
                "id",
                format!(
                    "{:?} is not an NDS sound file ID (SDAT, SWAR, SWAV, SBNK, STRM, SSEQ, SSAR)",
                    String::from_utf8_lossy(&b[..4])
                ),
            )
        })?;
        let bom = u16_at(b, 4, b.len(), "byte_order")?;
        if bom != BOM {
            return Err(err(
                4,
                "byte_order",
                format!(
                    "byte order mark {bom:04X}h is not the documented FEFFh (stored FF FE); other byte orders are not interpreted"
                ),
            ));
        }
        let version = u16_at(b, 6, b.len(), "version")?;
        let file_size = u32_at(b, 8, b.len(), "file_size")?;
        let header_size = u16_at(b, 12, b.len(), "header_size")?;
        let blocks = u16_at(b, 14, b.len(), "blocks")?;
        if file_size as usize > b.len() {
            return Err(err(
                8,
                "file_size",
                format!(
                    "declares {file_size} bytes but {} are present (truncated file)",
                    b.len()
                ),
            ));
        }
        if (header_size as usize) < HEADER_BYTES || header_size as u32 > file_size {
            return Err(err(
                12,
                "header_size",
                format!("header size {header_size} is below 16 or past the declared file size"),
            ));
        }
        Ok(FileHeader {
            kind,
            bom,
            version,
            file_size,
            header_size,
            blocks,
            trailing_bytes: b.len() - file_size as usize,
        })
    }
    pub fn end(&self) -> usize {
        self.file_size as usize
    }
    pub fn report(&self) -> Value {
        json!({"id":self.kind.name().to_uppercase(),"byte_order":format!("{:04X}h", self.bom),
            "version":format!("{:04X}h", self.version),"version_documented":self.version == DOCUMENTED_VERSION,
            "file_size":self.file_size,"header_size":self.header_size,"blocks":self.blocks,
            "trailing_bytes":self.trailing_bytes,
            "offsets":{"id":0,"byte_order":4,"version":6,"file_size":8,"header_size":12,"blocks":14}})
    }
}

/// SWAV sample encodings (`WaveType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Pcm8,
    Pcm16,
    ImaAdpcm,
}

impl Encoding {
    pub fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Encoding::Pcm8),
            1 => Some(Encoding::Pcm16),
            2 => Some(Encoding::ImaAdpcm),
            _ => None,
        }
    }
    pub fn raw(self) -> u8 {
        match self {
            Encoding::Pcm8 => 0,
            Encoding::Pcm16 => 1,
            Encoding::ImaAdpcm => 2,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Pcm8 => "pcm8",
            Encoding::Pcm16 => "pcm16",
            Encoding::ImaAdpcm => "ima_adpcm",
        }
    }
    /// Samples per 32-bit word.
    pub fn samples_per_word(self) -> usize {
        match self {
            Encoding::Pcm8 => 4,
            Encoding::Pcm16 => 2,
            Encoding::ImaAdpcm => 8,
        }
    }
}

/// A SWAV sample block: the 12-byte info and its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveInfo {
    /// Offset of the 12-byte info within the file.
    pub info_offset: usize,
    pub encoding: Encoding,
    pub loop_flag: u8,
    pub sample_rate: u16,
    pub timer: u16,
    pub loop_words: u16,
    pub length_words: u32,
}

/// Decoded wave samples in PCM16 units.
#[derive(Debug, Clone)]
pub struct WaveDecoded {
    pub pcm: Vec<i32>,
    /// IMA-ADPCM: the header and the decoder state at the loop start, which
    /// the hardware reloads at every loop (GBATEK).
    pub adpcm: Option<(AdpcmHeader, Option<State>, usize)>,
}

impl WaveInfo {
    /// Parses the info at `at`; the data must end at or before `end`.
    pub fn parse(b: &[u8], at: usize, end: usize) -> Result<Self, NdsError> {
        if at + WAVE_INFO_BYTES > end.min(b.len()) {
            return Err(err(at, "wave_info", "the 12-byte sample info does not fit"));
        }
        let raw = b[at];
        let encoding = Encoding::from_raw(raw).ok_or_else(|| {
            err(
                at,
                "wave_type",
                format!("wave type {raw} is not documented (0 PCM8, 1 PCM16, 2 IMA-ADPCM)"),
            )
        })?;
        let loop_flag = b[at + 1];
        if loop_flag > 1 {
            return Err(err(
                at + 1,
                "loop_flag",
                format!("loop flag {loop_flag} is not 0 (one-shot) or 1 (loop)"),
            ));
        }
        let sample_rate = u16_at(b, at + 2, end, "sample_rate")?;
        let timer = u16_at(b, at + 4, end, "timer")?;
        let loop_words = u16_at(b, at + 6, end, "loop_offset")?;
        let length_words = u32_at(b, at + 8, end, "length")?;
        if sample_rate == 0 {
            return Err(err(at + 2, "sample_rate", "sample rate 0"));
        }
        if timer == 0 {
            return Err(err(at + 4, "timer", "timer 0"));
        }
        let info = WaveInfo {
            info_offset: at,
            encoding,
            loop_flag,
            sample_rate,
            timer,
            loop_words,
            length_words,
        };
        let data_end = (at + WAVE_INFO_BYTES) as u64 + info.data_bytes() as u64;
        if data_end > end.min(b.len()) as u64 {
            return Err(err(
                at + 8,
                "length",
                format!(
                    "loop offset {} + length {} words ({} bytes) run past byte {}",
                    loop_words,
                    length_words,
                    info.data_bytes(),
                    end.min(b.len())
                ),
            ));
        }
        if encoding == Encoding::ImaAdpcm && info.data_bytes() < 4 {
            return Err(err(
                at + 8,
                "length",
                "IMA-ADPCM data needs its 4-byte header word",
            ));
        }
        if info.looped() {
            if encoding == Encoding::ImaAdpcm && loop_words == 0 {
                return Err(err(
                    at + 6,
                    "loop_offset",
                    "an IMA-ADPCM loop starting at word 0 would restart at the header word; the documented loop start is in the data part",
                ));
            }
            if length_words == 0 {
                return Err(err(
                    at + 8,
                    "length",
                    "a looped wave with an empty loop part",
                ));
            }
        }
        Ok(info)
    }
    pub fn looped(&self) -> bool {
        self.loop_flag == 1
    }
    pub fn data_offset(&self) -> usize {
        self.info_offset + WAVE_INFO_BYTES
    }
    pub fn data_bytes(&self) -> usize {
        (self.loop_words as usize + self.length_words as usize) * 4
    }
    /// Whole block: info plus data.
    pub fn block_end(&self) -> usize {
        self.data_offset() + self.data_bytes()
    }
    pub fn samples(&self) -> usize {
        let words = self.loop_words as usize + self.length_words as usize;
        match self.encoding {
            Encoding::ImaAdpcm => 8 * (words - 1),
            e => e.samples_per_word() * words,
        }
    }
    /// The first sample of the repeated part (also where a one-shot's
    /// `loop_offset` points).
    pub fn loop_sample(&self) -> usize {
        match self.encoding {
            Encoding::ImaAdpcm => 8 * (self.loop_words as usize).saturating_sub(1),
            e => e.samples_per_word() * self.loop_words as usize,
        }
    }
    pub fn timer_rate_hz(&self) -> f64 {
        ARM7_CLOCK_HALF_HZ / self.timer as f64
    }
    pub fn decode(&self, b: &[u8]) -> Result<WaveDecoded, NdsError> {
        let data = &b[self.data_offset()..self.block_end()];
        Ok(match self.encoding {
            Encoding::Pcm8 => WaveDecoded {
                pcm: nds_adpcm::decode_pcm8(data),
                adpcm: None,
            },
            Encoding::Pcm16 => WaveDecoded {
                pcm: nds_adpcm::decode_pcm16(data),
                adpcm: None,
            },
            Encoding::ImaAdpcm => {
                let d = nds_adpcm::decode(data, self.samples())
                    .map_err(|m| err(self.data_offset(), "adpcm_header", m))?;
                let loop_state = self.looped().then(|| {
                    let at = self.loop_sample();
                    if at == 0 {
                        d.header.state
                    } else {
                        // The state after sample at-1 is the state the
                        // first pass had on reaching the loop start.
                        let mut s = d.header.state;
                        for i in 0..at {
                            let byte = data[4 + i / 2];
                            let code = if i % 2 == 0 { byte & 15 } else { byte >> 4 };
                            nds_adpcm::step(&mut s, code);
                        }
                        s
                    }
                });
                WaveDecoded {
                    pcm: d.pcm,
                    adpcm: Some((d.header, loop_state, d.clamped)),
                }
            }
        })
    }
    /// Raw fields with their offsets in the file.
    pub fn report(&self) -> Value {
        let at = self.info_offset;
        let rate = self.sample_rate as f64;
        let timer_rate = self.timer_rate_hz();
        json!({
            "info_offset": at,
            "wave_type": {"offset": at, "raw": self.encoding.raw(), "encoding": self.encoding.name()},
            "loop_flag": {"offset": at + 1, "raw": self.loop_flag, "looped": self.looped()},
            "sample_rate": {"offset": at + 2, "raw": self.sample_rate},
            "timer": {"offset": at + 4, "raw": self.timer, "rate_hz": timer_rate,
                "from_rate_floor": (ARM7_CLOCK_HALF_HZ / rate).floor() as u64,
                "cents_from_sample_rate": 1200.0 * (timer_rate / rate).log2()},
            "loop_offset": {"offset": at + 6, "words": self.loop_words, "bytes": self.loop_words as usize * 4},
            "length": {"offset": at + 8, "words": self.length_words, "bytes": self.length_words as usize * 4},
            "data": {"offset": self.data_offset(), "bytes": self.data_bytes(), "end": self.block_end()},
            "samples": self.samples(),
            "loop_start_sample": self.looped().then(|| self.loop_sample()),
            "loop_samples": self.looped().then(|| self.samples() - self.loop_sample()),
            "duration_seconds": self.samples() as f64 / rate,
            "hardware_minimum_words_met": self.loop_words as u64 + self.length_words as u64 >= 4,
        })
    }
}

/// Storage-order facts for a set of sample blocks.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// (first index, other index) pairs with the same info offset.
    pub shared: Vec<(usize, usize)>,
    /// (index, next index in storage order) where a block runs into the next.
    pub overlaps: Vec<(usize, usize)>,
    /// Byte ranges no wave block covers, between the offset table and the end.
    pub gaps: Vec<(usize, usize)>,
    pub increasing: bool,
}

/// A SWAR wave archive.
#[derive(Debug, Clone)]
pub struct Swar {
    pub header: FileHeader,
    pub data_block: usize,
    pub data_size: u32,
    pub reserved: [u8; 32],
    pub count: u32,
    pub table: usize,
    pub offsets: Vec<u32>,
    pub waves: Vec<Result<WaveInfo, NdsError>>,
}

impl Swar {
    pub fn parse(b: &[u8]) -> Result<Self, NdsError> {
        let header = FileHeader::parse(b)?;
        if header.kind != Kind::Swar {
            return Err(err(0, "id", "not a SWAR file"));
        }
        let end = header.end();
        let at = header.header_size as usize;
        tag_at(b, at, end, b"DATA", "data_block")?;
        let data_size = u32_at(b, at + 4, end, "data_size")?;
        let mut reserved = [0u8; 32];
        if at + 40 > end {
            return Err(err(at + 8, "reserved", "the 32 reserved bytes do not fit"));
        }
        reserved.copy_from_slice(&b[at + 8..at + 40]);
        let count = u32_at(b, at + 40, end, "count")?;
        let table = at + 44;
        if table as u64 + count as u64 * 4 > end as u64 {
            return Err(err(
                at + 40,
                "count",
                format!("{count} offsets do not fit before byte {end}"),
            ));
        }
        let table_end = table + count as usize * 4;
        let offsets: Vec<u32> = (0..count as usize)
            .map(|i| u32_at(b, table + 4 * i, end, "offset").expect("checked above"))
            .collect();
        let waves = offsets
            .iter()
            .enumerate()
            .map(|(i, &o)| {
                let o = o as usize;
                if o < table_end {
                    Err(err(
                        table + 4 * i,
                        "offset",
                        format!("wave {i} offset {o} points into the header or offset table"),
                    ))
                } else {
                    WaveInfo::parse(b, o, end)
                }
            })
            .collect();
        Ok(Swar {
            header,
            data_block: at,
            data_size,
            reserved,
            count,
            table,
            offsets,
            waves,
        })
    }
    pub fn table_end(&self) -> usize {
        self.table + self.count as usize * 4
    }
    pub fn layout(&self) -> Layout {
        layout(
            self.waves
                .iter()
                .map(|w| w.as_ref().ok().map(|w| (w.info_offset, w.block_end()))),
            self.table_end(),
            self.header.end(),
        )
    }
    pub fn report(&self) -> Value {
        let at = self.data_block;
        json!({"data_block":{"offset":at,"tag":"DATA","size_offset":at+4,"size":self.data_size,
            "size_matches_file":self.data_size as usize + at == self.header.end()},
            "reserved":{"offset":at+8,"hex":hex(&self.reserved),"zero":self.reserved == [0;32]},
            "count":{"offset":at+40,"value":self.count},"offset_table":{"offset":self.table,"end":self.table_end()}})
    }
}

fn layout(
    blocks: impl Iterator<Item = Option<(usize, usize)>>,
    table_end: usize,
    end: usize,
) -> Layout {
    let blocks: Vec<(usize, (usize, usize))> = blocks
        .enumerate()
        .filter_map(|(i, b)| b.map(|b| (i, b)))
        .collect();
    let mut out = Layout {
        increasing: blocks.windows(2).all(|w| w[0].1.0 < w[1].1.0),
        ..Layout::default()
    };
    let mut order = blocks.clone();
    order.sort_by_key(|&(i, (start, _))| (start, i));
    for w in order.windows(2) {
        let ((a, (sa, ea)), (b, (sb, _))) = (w[0], w[1]);
        if sa == sb {
            let first = order.iter().find(|x| x.1.0 == sa).expect("present").0;
            out.shared.push((first, b));
        } else if ea > sb {
            out.overlaps.push((a, b));
        }
    }
    let mut cursor = table_end;
    for &(_, (start, stop)) in &order {
        if start > cursor {
            out.gaps.push((cursor, start));
        }
        cursor = cursor.max(stop);
    }
    if end > cursor {
        out.gaps.push((cursor, end));
    }
    out
}

/// A standalone SWAV file.
#[derive(Debug, Clone)]
pub struct Swav {
    pub header: FileHeader,
    pub data_block: usize,
    pub data_size: u32,
    pub wave: WaveInfo,
}

impl Swav {
    pub fn parse(b: &[u8]) -> Result<Self, NdsError> {
        let header = FileHeader::parse(b)?;
        if header.kind != Kind::Swav {
            return Err(err(0, "id", "not a SWAV file"));
        }
        let at = header.header_size as usize;
        tag_at(b, at, header.end(), b"DATA", "data_block")?;
        let data_size = u32_at(b, at + 4, header.end(), "data_size")?;
        let wave = WaveInfo::parse(b, at + 8, header.end())?;
        Ok(Swav {
            header,
            data_block: at,
            data_size,
            wave,
        })
    }
    pub fn report(&self) -> Value {
        json!({"data_block":{"offset":self.data_block,"tag":"DATA","size_offset":self.data_block+4,"size":self.data_size,
            "size_matches_file":self.data_size as usize + self.data_block == self.header.end()},
            "trailing_in_file": self.header.end() - self.wave.block_end()})
    }
}

/// A note definition: wave reference, base note, envelope and pan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDef {
    pub offset: usize,
    pub swav: u16,
    /// Index 0..3 into the bank's wave-archive list (SDAT INFO BANK entry).
    pub swar: u16,
    pub note: u8,
    pub attack: u8,
    pub decay: u8,
    pub sustain: u8,
    pub release: u8,
    pub pan: u8,
}

impl NoteDef {
    fn parse(b: &[u8], at: usize, end: usize) -> Result<Self, NdsError> {
        if at + 10 > end {
            return Err(err(
                at,
                "note_definition",
                "the 10-byte note definition does not fit",
            ));
        }
        Ok(NoteDef {
            offset: at,
            swav: u16::from_le_bytes([b[at], b[at + 1]]),
            swar: u16::from_le_bytes([b[at + 2], b[at + 3]]),
            note: b[at + 4],
            attack: b[at + 5],
            decay: b[at + 6],
            sustain: b[at + 7],
            release: b[at + 8],
            pan: b[at + 9],
        })
    }
    pub fn report(&self) -> Value {
        let out_of_range: Vec<&str> = [
            ("note", self.note),
            ("attack", self.attack),
            ("decay", self.decay),
            ("sustain", self.sustain),
            ("release", self.release),
            ("pan", self.pan),
        ]
        .iter()
        .filter(|(_, v)| *v > 127)
        .map(|(n, _)| *n)
        .collect();
        json!({"offset":self.offset,"swav":self.swav,"swar_slot":self.swar,"base_note":self.note,
            "attack":self.attack,"decay":self.decay,"sustain":self.sustain,"release":self.release,"pan":self.pan,
            "above_documented_0_127":out_of_range})
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstrumentBody {
    /// Type 0.
    Empty,
    /// Types 1..15: one definition for every note.
    Single(NoteDef),
    /// Type 16: one definition per note from `low` to `high`, each after a
    /// two-byte unknown value.
    DrumSet {
        low: u8,
        high: u8,
        entries: Vec<(u16, NoteDef)>,
    },
    /// Type 17: up to eight regions ending at the given notes.
    KeySplit {
        ends: [u8; 8],
        regions: Vec<(u8, u8, u16, NoteDef)>,
    },
    /// Types 18..255: not documented; retained, not interpreted.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Instrument {
    pub index: usize,
    pub record_offset: usize,
    pub kind: u8,
    pub data_offset: u16,
    pub reserved: u8,
    pub body: Result<InstrumentBody, NdsError>,
}

impl Instrument {
    pub fn definitions(&self) -> Vec<&NoteDef> {
        match &self.body {
            Ok(InstrumentBody::Single(d)) => vec![d],
            Ok(InstrumentBody::DrumSet { entries, .. }) => entries.iter().map(|e| &e.1).collect(),
            Ok(InstrumentBody::KeySplit { regions, .. }) => regions.iter().map(|r| &r.3).collect(),
            _ => vec![],
        }
    }
    /// Bytes of the instrument's data, for coverage.
    fn extent(&self) -> Option<(usize, usize)> {
        let at = self.data_offset as usize;
        match &self.body {
            Ok(InstrumentBody::Single(_)) => Some((at, at + 10)),
            Ok(InstrumentBody::DrumSet { entries, .. }) => Some((at, at + 2 + 12 * entries.len())),
            Ok(InstrumentBody::KeySplit { regions, .. }) => Some((at, at + 8 + 12 * regions.len())),
            _ => None,
        }
    }
    pub fn report(&self) -> Value {
        let (type_name, body) = match &self.body {
            Err(e) => (
                "refused",
                json!({"refusal":e.to_string(),"error_offset":e.offset}),
            ),
            Ok(InstrumentBody::Empty) => ("empty", json!({})),
            Ok(InstrumentBody::Unknown) => (
                "undocumented",
                json!({"note":"instrument types above 17 are not documented; the bytes are retained, not interpreted"}),
            ),
            Ok(InstrumentBody::Single(d)) => ("single", json!({"definition":d.report()})),
            Ok(InstrumentBody::DrumSet { low, high, entries }) => (
                "drum_set",
                json!({"low_note":low,"high_note":high,"entries":entries.iter().enumerate().map(|(k,(u,d))| json!({
                    "note": *low as usize + k, "unknown_offset": d.offset - 2, "unknown": u, "definition": d.report()})).collect::<Vec<_>>()}),
            ),
            Ok(InstrumentBody::KeySplit { ends, regions }) => (
                "key_split",
                json!({"region_ends":ends,"regions":regions.iter().map(|(lo,hi,u,d)| json!({
                    "low_note":lo,"high_note":hi,"unknown_offset":d.offset-2,"unknown":u,"definition":d.report()})).collect::<Vec<_>>()}),
            ),
        };
        let mut out = json!({"index":self.index,"record_offset":self.record_offset,"type":self.kind,"type_name":type_name,
            "data_offset":self.data_offset,"reserved":self.reserved});
        if let Value::Object(map) = body {
            for (k, v) in map {
                out[k] = v;
            }
        }
        out
    }
}

/// An SBNK instrument bank.
#[derive(Debug, Clone)]
pub struct Sbnk {
    pub header: FileHeader,
    pub data_block: usize,
    pub data_size: u32,
    pub reserved: [u8; 32],
    pub count: u32,
    pub instruments: Vec<Instrument>,
}

impl Sbnk {
    pub fn parse(b: &[u8]) -> Result<Self, NdsError> {
        let header = FileHeader::parse(b)?;
        if header.kind != Kind::Sbnk {
            return Err(err(0, "id", "not an SBNK file"));
        }
        let end = header.end();
        let at = header.header_size as usize;
        tag_at(b, at, end, b"DATA", "data_block")?;
        let data_size = u32_at(b, at + 4, end, "data_size")?;
        if at + 40 > end {
            return Err(err(at + 8, "reserved", "the 32 reserved bytes do not fit"));
        }
        let mut reserved = [0u8; 32];
        reserved.copy_from_slice(&b[at + 8..at + 40]);
        let count = u32_at(b, at + 40, end, "count")?;
        let table = at + 44;
        if table as u64 + count as u64 * 4 > end as u64 {
            return Err(err(
                at + 40,
                "count",
                format!("{count} instrument records do not fit before byte {end}"),
            ));
        }
        let instruments = (0..count as usize)
            .map(|i| {
                let r = table + 4 * i;
                let kind = b[r];
                let data_offset = u16::from_le_bytes([b[r + 1], b[r + 2]]);
                Instrument {
                    index: i,
                    record_offset: r,
                    kind,
                    data_offset,
                    reserved: b[r + 3],
                    body: instrument_body(b, kind, data_offset as usize, r, end),
                }
            })
            .collect();
        Ok(Sbnk {
            header,
            data_block: at,
            data_size,
            reserved,
            count,
            instruments,
        })
    }
    pub fn table_end(&self) -> usize {
        self.data_block + 44 + self.count as usize * 4
    }
    /// Byte ranges no instrument references, and instruments sharing data.
    pub fn coverage(&self) -> (Vec<Range>, Vec<(usize, usize)>) {
        let mut extents: Vec<(usize, usize, usize)> = self
            .instruments
            .iter()
            .filter_map(|i| i.extent().map(|(s, e)| (s, e, i.index)))
            .collect();
        extents.sort();
        let mut gaps = vec![];
        let mut shared = vec![];
        let mut cursor = self.table_end();
        let mut last: Option<(usize, usize, usize)> = None;
        for &(s, e, i) in &extents {
            if let Some((_, le, li)) = last
                && s < le
            {
                shared.push((li, i));
            }
            if s > cursor {
                gaps.push((cursor, s));
            }
            cursor = cursor.max(e);
            last = Some((s, e, i));
        }
        if self.header.end() > cursor {
            gaps.push((cursor, self.header.end()));
        }
        (gaps, shared)
    }
    pub fn report(&self) -> Value {
        let at = self.data_block;
        json!({"data_block":{"offset":at,"tag":"DATA","size_offset":at+4,"size":self.data_size,
            "size_matches_file":self.data_size as usize + at == self.header.end()},
            "reserved":{"offset":at+8,"hex":hex(&self.reserved),"zero":self.reserved == [0;32]},
            "count":{"offset":at+40,"value":self.count},"record_table":{"offset":at+44,"end":self.table_end(),"record_bytes":4}})
    }
}

fn instrument_body(
    b: &[u8],
    kind: u8,
    at: usize,
    record: usize,
    end: usize,
) -> Result<InstrumentBody, NdsError> {
    if kind == 0 {
        return Ok(InstrumentBody::Empty);
    }
    if kind > 17 {
        return Ok(InstrumentBody::Unknown);
    }
    if at < HEADER_BYTES {
        return Err(err(
            record + 1,
            "data_offset",
            format!("data offset {at} points into the file header"),
        ));
    }
    match kind {
        1..=15 => Ok(InstrumentBody::Single(NoteDef::parse(b, at, end)?)),
        16 => {
            let low = u8_at(b, at, end, "low_note")?;
            let high = u8_at(b, at + 1, end, "high_note")?;
            if low > high || high > 127 {
                return Err(err(
                    at,
                    "note_range",
                    format!("drum-set range {low}..{high} is not an ordered range within 0..127"),
                ));
            }
            let mut entries = vec![];
            for k in 0..=(high - low) as usize {
                let e = at + 2 + 12 * k;
                let unknown = u16_at(b, e, end, "entry_unknown")?;
                entries.push((unknown, NoteDef::parse(b, e + 2, end)?));
            }
            Ok(InstrumentBody::DrumSet { low, high, entries })
        }
        _ => {
            if at + 8 > end {
                return Err(err(at, "region_ends", "the eight region ends do not fit"));
            }
            let mut ends = [0u8; 8];
            ends.copy_from_slice(&b[at..at + 8]);
            let used = ends.iter().take_while(|&&e| e != 0).count();
            if used == 0 {
                return Err(err(at, "region_ends", "a key split without any region"));
            }
            if let Some(k) = ends[used..].iter().position(|&e| e != 0) {
                return Err(err(
                    at + used + k,
                    "region_ends",
                    "a nonzero region end after an unused (0) one",
                ));
            }
            let mut regions = vec![];
            let mut low = 0u8;
            for (k, &high) in ends[..used].iter().enumerate() {
                if high < low || high > 127 {
                    return Err(err(
                        at + k,
                        "region_ends",
                        format!("region end {high} is below the region start {low} or above 127"),
                    ));
                }
                let e = at + 8 + 12 * k;
                let unknown = u16_at(b, e, end, "region_unknown")?;
                regions.push((low, high, unknown, NoteDef::parse(b, e + 2, end)?));
                low = high.saturating_add(1);
            }
            Ok(InstrumentBody::KeySplit { ends, regions })
        }
    }
}

/// A STRM stream.
#[derive(Debug, Clone)]
pub struct Strm {
    pub header: FileHeader,
    pub head_block: usize,
    pub head_size: u32,
    pub encoding: Encoding,
    pub loop_flag: u8,
    pub channels: u8,
    pub unknown_1b: u8,
    pub sample_rate: u16,
    pub timer: u16,
    pub loop_offset: u32,
    pub samples: u32,
    pub data_offset: u32,
    pub blocks: u32,
    pub block_bytes: u32,
    pub block_samples: u32,
    pub last_block_bytes: u32,
    pub last_block_samples: u32,
    pub reserved: [u8; 32],
    pub data_block: usize,
    pub data_size: u32,
}

/// One block header of an IMA-ADPCM stream channel.
#[derive(Debug, Clone)]
pub struct BlockHeader {
    pub block: usize,
    pub offset: usize,
    pub header: AdpcmHeader,
    /// Whether the header equals the decoder state at the end of the
    /// previous block of this channel.
    pub continues: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct StreamDecoded {
    pub pcm: Vec<i32>,
    pub block_headers: Vec<BlockHeader>,
    pub clamped: usize,
}

impl Strm {
    pub fn parse(b: &[u8]) -> Result<Self, NdsError> {
        let header = FileHeader::parse(b)?;
        if header.kind != Kind::Strm {
            return Err(err(0, "id", "not a STRM file"));
        }
        let end = header.end();
        let h = header.header_size as usize;
        tag_at(b, h, end, b"HEAD", "head_block")?;
        let head_size = u32_at(b, h + 4, end, "head_size")?;
        if head_size < 0x50 || h as u64 + head_size as u64 > end as u64 {
            return Err(err(
                h + 4,
                "head_size",
                format!("HEAD size {head_size} is below the documented 50h or past the file"),
            ));
        }
        let raw = b[h + 8];
        let encoding = Encoding::from_raw(raw).ok_or_else(|| {
            err(
                h + 8,
                "wave_type",
                format!("wave type {raw} is not documented (0 PCM8, 1 PCM16, 2 IMA-ADPCM)"),
            )
        })?;
        let loop_flag = b[h + 9];
        if loop_flag > 1 {
            return Err(err(
                h + 9,
                "loop_flag",
                format!("loop flag {loop_flag} is not 0 or 1"),
            ));
        }
        let channels = b[h + 10];
        if !(1..=2).contains(&channels) {
            return Err(err(
                h + 10,
                "channels",
                format!("{channels} channels; the documented streams are mono or stereo"),
            ));
        }
        let field = |o: usize, name| u32_at(b, h + o, end, name);
        let mut reserved = [0u8; 32];
        reserved.copy_from_slice(&b[h + 0x30..h + 0x50]);
        let s = Strm {
            head_block: h,
            head_size,
            encoding,
            loop_flag,
            channels,
            unknown_1b: b[h + 11],
            sample_rate: u16_at(b, h + 12, end, "sample_rate")?,
            timer: u16_at(b, h + 14, end, "timer")?,
            loop_offset: field(0x10, "loop_offset")?,
            samples: field(0x14, "samples")?,
            data_offset: field(0x18, "data_offset")?,
            blocks: field(0x1C, "blocks")?,
            block_bytes: field(0x20, "block_bytes")?,
            block_samples: field(0x24, "block_samples")?,
            last_block_bytes: field(0x28, "last_block_bytes")?,
            last_block_samples: field(0x2C, "last_block_samples")?,
            reserved,
            data_block: h + head_size as usize,
            data_size: 0,
            header,
        };
        let mut s = s;
        tag_at(b, s.data_block, end, b"DATA", "data_block")?;
        s.data_size = u32_at(b, s.data_block + 4, end, "data_size")?;
        if s.sample_rate == 0 {
            return Err(err(h + 12, "sample_rate", "sample rate 0"));
        }
        if s.timer == 0 {
            return Err(err(h + 14, "timer", "timer 0"));
        }
        if s.blocks == 0 || s.block_bytes == 0 || s.last_block_bytes == 0 {
            return Err(err(h + 0x1C, "blocks", "no blocks, or a block of 0 bytes"));
        }
        if s.last_block_bytes > s.block_bytes {
            return Err(err(
                h + 0x28,
                "last_block_bytes",
                "the last block is longer than a block",
            ));
        }
        let needed = |samples: u32| -> u64 {
            match encoding {
                Encoding::Pcm8 => samples as u64,
                Encoding::Pcm16 => samples as u64 * 2,
                Encoding::ImaAdpcm => 4 + (samples as u64).div_ceil(2),
            }
        };
        if needed(s.block_samples) != s.block_bytes as u64 {
            return Err(err(
                h + 0x24,
                "block_samples",
                format!(
                    "{} samples per block need {} bytes in {}; the block length is {}",
                    s.block_samples,
                    needed(s.block_samples),
                    encoding.name(),
                    s.block_bytes
                ),
            ));
        }
        let last = needed(s.last_block_samples);
        if last > s.last_block_bytes as u64 || s.last_block_bytes as u64 - last >= 4 {
            return Err(err(
                h + 0x2C,
                "last_block_samples",
                format!(
                    "{} samples in the last block need {last} bytes; the last block length is {} (at most 3 padding bytes are interpreted)",
                    s.last_block_samples, s.last_block_bytes
                ),
            ));
        }
        let total = (s.blocks as u64 - 1) * s.block_samples as u64 + s.last_block_samples as u64;
        if total != s.samples as u64 {
            return Err(err(
                h + 0x14,
                "samples",
                format!(
                    "the block layout holds {total} samples per channel but the header declares {}",
                    s.samples
                ),
            ));
        }
        if s.samples as usize > MAX_STREAM_SAMPLES {
            return Err(err(
                h + 0x14,
                "samples",
                format!(
                    "{} samples per channel exceed the {MAX_STREAM_SAMPLES}-sample limit",
                    s.samples
                ),
            ));
        }
        if s.looped() && s.loop_offset >= s.samples {
            return Err(err(
                h + 0x10,
                "loop_offset",
                "the loop start is at or past the last sample",
            ));
        }
        if (s.data_offset as usize) < s.data_block + 8 {
            return Err(err(
                h + 0x18,
                "data_offset",
                "the wave data offset points before the DATA block's data",
            ));
        }
        if s.data_end() > end as u64 {
            return Err(err(
                h + 0x1C,
                "blocks",
                format!(
                    "{} blocks of {} channel(s) end at byte {}, past the declared file size {end}",
                    s.blocks,
                    s.channels,
                    s.data_end()
                ),
            ));
        }
        Ok(s)
    }
    pub fn looped(&self) -> bool {
        self.loop_flag == 1
    }
    fn data_end(&self) -> u64 {
        self.data_offset as u64
            + (self.blocks as u64 - 1) * self.block_bytes as u64 * self.channels as u64
            + self.last_block_bytes as u64 * self.channels as u64
    }
    /// Offset of `block` of `channel`: channels interleave per block, and the
    /// last blocks are `last_block_bytes` apart.
    pub fn block_offset(&self, block: usize, channel: usize) -> usize {
        let stride = if block + 1 == self.blocks as usize {
            self.last_block_bytes
        } else {
            self.block_bytes
        } as usize;
        self.data_offset as usize
            + block * self.block_bytes as usize * self.channels as usize
            + channel * stride
    }
    pub fn block_len(&self, block: usize) -> (usize, usize) {
        if block + 1 == self.blocks as usize {
            (
                self.last_block_bytes as usize,
                self.last_block_samples as usize,
            )
        } else {
            (self.block_bytes as usize, self.block_samples as usize)
        }
    }
    pub fn timer_rate_hz(&self) -> f64 {
        ARM7_CLOCK_HALF_HZ / 32.0 / self.timer as f64
    }
    pub fn decode_channel(&self, b: &[u8], channel: usize) -> Result<StreamDecoded, NdsError> {
        let mut pcm = Vec::with_capacity(self.samples as usize);
        let mut block_headers = vec![];
        let mut clamped = 0;
        let mut previous: Option<State> = None;
        for block in 0..self.blocks as usize {
            let at = self.block_offset(block, channel);
            let (bytes, samples) = self.block_len(block);
            let data = &b[at..at + bytes];
            match self.encoding {
                Encoding::Pcm8 => pcm.extend(nds_adpcm::decode_pcm8(&data[..samples])),
                Encoding::Pcm16 => pcm.extend(nds_adpcm::decode_pcm16(&data[..samples * 2])),
                Encoding::ImaAdpcm => {
                    let d = nds_adpcm::decode(data, samples)
                        .map_err(|m| err(at, "block_header", format!("block {block}: {m}")))?;
                    block_headers.push(BlockHeader {
                        block,
                        offset: at,
                        header: d.header,
                        continues: previous.map(|p| p == d.header.state),
                    });
                    previous = Some(d.end_state);
                    clamped += d.clamped;
                    pcm.extend(d.pcm);
                }
            }
        }
        Ok(StreamDecoded {
            pcm,
            block_headers,
            clamped,
        })
    }
    pub fn report(&self) -> Value {
        let h = self.head_block;
        let rate = self.sample_rate as f64;
        json!({
            "head_block": {"offset": h, "tag": "HEAD", "size_offset": h + 4, "size": self.head_size},
            "wave_type": {"offset": h + 8, "raw": self.encoding.raw(), "encoding": self.encoding.name()},
            "loop_flag": {"offset": h + 9, "raw": self.loop_flag, "looped": self.looped()},
            "channels": {"offset": h + 10, "value": self.channels},
            "unknown_1b": {"offset": h + 11, "raw": self.unknown_1b, "documented_zero": self.unknown_1b == 0},
            "sample_rate": {"offset": h + 12, "raw": self.sample_rate},
            "timer": {"offset": h + 14, "raw": self.timer, "rate_hz": self.timer_rate_hz(),
                "cents_from_sample_rate": 1200.0 * (self.timer_rate_hz() / rate).log2()},
            "loop_offset": {"offset": h + 0x10, "samples": self.loop_offset,
                "block": self.loop_offset / self.block_samples, "sample_in_block": self.loop_offset % self.block_samples},
            "samples": {"offset": h + 0x14, "value": self.samples},
            "data_offset": {"offset": h + 0x18, "value": self.data_offset, "documented_68h": self.data_offset == 0x68},
            "blocks": {"offset": h + 0x1C, "value": self.blocks},
            "block_bytes": {"offset": h + 0x20, "value": self.block_bytes},
            "block_samples": {"offset": h + 0x24, "value": self.block_samples},
            "last_block_bytes": {"offset": h + 0x28, "value": self.last_block_bytes},
            "last_block_samples": {"offset": h + 0x2C, "value": self.last_block_samples},
            "reserved": {"offset": h + 0x30, "hex": hex(&self.reserved), "zero": self.reserved == [0; 32]},
            "data_block": {"offset": self.data_block, "tag": "DATA", "size_offset": self.data_block + 4, "size": self.data_size,
                "size_matches_file": self.data_block + self.data_size as usize == self.header.end()},
            "layout": {"interleave": "per block: block 0 of every channel, then block 1 of every channel; the last blocks are last_block_bytes apart",
                "data_end": self.data_end(), "unused_bytes_after_data": self.header.end() as u64 - self.data_end()},
            "adpcm_block_headers": (self.encoding == Encoding::ImaAdpcm).then_some("every block of every channel starts with its own 4-byte header (block_samples = (block_bytes - 4) * 2)"),
            "duration_seconds": self.samples as f64 / rate,
        })
    }
}

/// A FAT entry of an SDAT.
#[derive(Debug, Clone)]
pub struct FatEntry {
    pub index: usize,
    pub entry_offset: usize,
    pub offset: u32,
    pub size: u32,
    pub reserved: [u8; 8],
    pub range: Result<(usize, usize), NdsError>,
}

/// One INFO list: its entry offsets (relative to INFO) and the parsed entries.
#[derive(Debug, Clone, Default)]
pub struct InfoList {
    pub offset: usize,
    pub entries: Vec<Option<InfoEntry>>,
}

#[derive(Debug, Clone)]
pub struct InfoEntry {
    pub offset: usize,
    pub file_id: Option<u16>,
    pub bytes: Vec<u8>,
}

/// The eight INFO/SYMB list kinds in their documented order.
pub const LISTS: [&str; 8] = [
    "sequence",
    "sequence_archive",
    "bank",
    "wave_archive",
    "player",
    "group",
    "player2",
    "stream",
];
/// Entry sizes per list; 0 means variable or undocumented size (retained raw).
const ENTRY_BYTES: [usize; 8] = [12, 4, 12, 4, 8, 0, 24, 12];

#[derive(Debug, Clone)]
pub struct Sdat {
    pub header: FileHeader,
    /// (offset, size) of SYMB, INFO, FAT and FILE.
    pub blocks: [(u32, u32); 4],
    pub padding: [u8; 16],
    pub fat: Vec<FatEntry>,
    pub file_count: Option<u32>,
    pub info: Result<Vec<InfoList>, NdsError>,
    /// Names per list (index aligned with INFO entries), when SYMB exists.
    pub symbols: Option<Result<Vec<Vec<Option<String>>>, NdsError>>,
}

impl Sdat {
    pub fn parse(b: &[u8]) -> Result<Self, NdsError> {
        let header = FileHeader::parse(b)?;
        if header.kind != Kind::Sdat {
            return Err(err(0, "id", "not an SDAT file"));
        }
        let end = header.end();
        if (header.header_size as usize) < 0x40 {
            return Err(err(
                12,
                "header_size",
                "an SDAT header needs 40h bytes for its block table",
            ));
        }
        let mut blocks = [(0u32, 0u32); 4];
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = 0x10 + 8 * i;
            *block = (
                u32_at(b, at, end, "block_offset")?,
                u32_at(b, at + 4, end, "block_size")?,
            );
            if block.0 as u64 + block.1 as u64 > end as u64 {
                return Err(err(
                    at,
                    "block_offset",
                    format!(
                        "{} block runs past the file",
                        ["SYMB", "INFO", "FAT", "FILE"][i]
                    ),
                ));
            }
        }
        let mut padding = [0u8; 16];
        padding.copy_from_slice(&b[0x30..0x40]);
        let (fat_at, fat_size) = (blocks[2].0 as usize, blocks[2].1 as usize);
        tag_at(b, fat_at, end, b"FAT ", "fat_block")?;
        let fat_end = fat_at + fat_size;
        let count = u32_at(b, fat_at + 8, fat_end, "fat_count")?;
        if 12 + count as u64 * 16 > fat_size as u64 {
            return Err(err(
                fat_at + 8,
                "fat_count",
                format!("{count} FAT entries do not fit in the {fat_size}-byte FAT block"),
            ));
        }
        let fat = (0..count as usize)
            .map(|i| {
                let at = fat_at + 12 + 16 * i;
                let offset = u32_at(b, at, fat_end, "fat_offset").expect("checked");
                let size = u32_at(b, at + 4, fat_end, "fat_size").expect("checked");
                let mut reserved = [0u8; 8];
                reserved.copy_from_slice(&b[at + 8..at + 16]);
                let range = if offset as u64 + size as u64 > end as u64 {
                    Err(err(
                        at,
                        "fat_offset",
                        format!("file {i} ({offset}+{size}) runs past the SDAT"),
                    ))
                } else {
                    Ok((offset as usize, offset as usize + size as usize))
                };
                FatEntry {
                    index: i,
                    entry_offset: at,
                    offset,
                    size,
                    reserved,
                    range,
                }
            })
            .collect();
        let file_count = if blocks[3].1 >= 12 {
            tag_at(b, blocks[3].0 as usize, end, b"FILE", "file_block")
                .ok()
                .and_then(|_| u32_at(b, blocks[3].0 as usize + 8, end, "file_count").ok())
        } else {
            None
        };
        let info = parse_info(b, blocks[1].0 as usize, blocks[1].1 as usize);
        let symbols = (header.blocks >= 4 && blocks[0].1 > 0)
            .then(|| parse_symbols(b, blocks[0].0 as usize, blocks[0].1 as usize));
        Ok(Sdat {
            header,
            blocks,
            padding,
            fat,
            file_count,
            info,
            symbols,
        })
    }
    pub fn member<'a>(&self, b: &'a [u8], index: usize) -> Result<&'a [u8], NdsError> {
        let entry = self.fat.get(index).ok_or_else(|| {
            err(
                self.blocks[2].0 as usize + 8,
                "member",
                format!("no FAT entry {index}; the SDAT has {}", self.fat.len()),
            )
        })?;
        let (s, e) = entry.range.clone()?;
        Ok(&b[s..e])
    }
    pub fn name(&self, list: usize, index: usize) -> Option<&str> {
        self.symbols
            .as_ref()?
            .as_ref()
            .ok()?
            .get(list)?
            .get(index)?
            .as_deref()
    }
    /// INFO entries (list, index) that name this FAT file.
    pub fn references(&self, file: usize) -> Vec<(usize, usize)> {
        let Ok(lists) = &self.info else { return vec![] };
        let mut out = vec![];
        for (l, list) in lists.iter().enumerate() {
            for (i, e) in list.entries.iter().enumerate() {
                if e.as_ref().and_then(|e| e.file_id) == Some(file as u16) {
                    out.push((l, i));
                }
            }
        }
        out
    }
    /// The wave-archive FAT files a BANK entry lists (0xFFFF unused).
    pub fn bank_wave_archives(&self, bank: usize) -> Option<[Option<WaveArchiveSlot>; 4]> {
        let lists = self.info.as_ref().ok()?;
        let e = lists.get(2)?.entries.get(bank)?.as_ref()?;
        let mut out = [None; 4];
        for (slot, item) in out.iter_mut().enumerate() {
            let wa = u16::from_le_bytes([e.bytes[4 + 2 * slot], e.bytes[5 + 2 * slot]]);
            if wa != 0xFFFF {
                let file = lists
                    .get(3)
                    .and_then(|l| l.entries.get(wa as usize))
                    .and_then(|e| e.as_ref())
                    .and_then(|e| e.file_id);
                *item = Some((wa as usize, file));
            }
        }
        Some(out)
    }
    pub fn info_entry_report(&self, list: usize, index: usize, e: &InfoEntry) -> Value {
        let b = &e.bytes;
        let u16v = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let mut out = json!({"list":LISTS[list],"index":index,"offset":e.offset,"name":self.name(list, index),"hex":hex(b)});
        if let Some(f) = e.file_id {
            out["file_id"] = json!(f);
            out["unknown_2"] = json!(u16v(2));
        }
        match list {
            0 => {
                out["bank"] = json!(u16v(4));
                out["volume"] = json!(b[6]);
                out["channel_priority"] = json!(b[7]);
                out["player_priority"] = json!(b[8]);
                out["player"] = json!(b[9]);
                out["unknown_10"] = json!(u16v(10));
            }
            2 => {
                out["wave_archives"] = json!(
                    (0..4)
                        .map(|s| {
                            let w = u16v(4 + 2 * s);
                            (w != 0xFFFF).then_some(w)
                        })
                        .collect::<Vec<_>>()
                );
            }
            7 => {
                out["volume"] = json!(b[4]);
                out["priority"] = json!(b[5]);
                out["player"] = json!(b[6]);
                out["reserved"] = json!(hex(&b[7..12]));
            }
            _ => {}
        }
        out
    }
    pub fn report(&self) -> Value {
        let names = ["SYMB", "INFO", "FAT", "FILE"];
        json!({"blocks": self.blocks.iter().enumerate().map(|(i, (o, s))| json!({"block":names[i],"table_offset":0x10+8*i,"offset":o,"size":s})).collect::<Vec<_>>(),
        "padding":{"offset":0x30,"hex":hex(&self.padding),"zero":self.padding == [0;16]},
        "fat":{"offset":self.blocks[2].0,"count":self.fat.len()},
        "file_block_count":self.file_count,
        "info": match &self.info {
            Ok(lists) => json!(lists.iter().enumerate().map(|(i, l)| json!({"list":LISTS[i],"offset":l.offset,"entries":l.entries.len(),
                "present":l.entries.iter().filter(|e| e.is_some()).count()})).collect::<Vec<_>>()),
            Err(e) => json!({"refusal":e.to_string()}),
        },
        "symbols": match &self.symbols {
            None => json!("absent (3-block SDAT or empty SYMB)"),
            Some(Ok(lists)) => json!(lists.iter().enumerate().map(|(i, l)| json!({"list":LISTS[i],"names":l.iter().filter(|n| n.is_some()).count()})).collect::<Vec<_>>()),
            Some(Err(e)) => json!({"refusal":e.to_string()}),
        }})
    }
}

fn parse_info(b: &[u8], at: usize, size: usize) -> Result<Vec<InfoList>, NdsError> {
    let end = at + size;
    tag_at(b, at, end, b"INFO", "info_block")?;
    let mut lists = vec![];
    for (l, &entry_bytes) in ENTRY_BYTES.iter().enumerate() {
        let rel = u32_at(b, at + 8 + 4 * l, end, "info_list")? as usize;
        if rel == 0 {
            lists.push(InfoList::default());
            continue;
        }
        let list = at + rel;
        let count = u32_at(b, list, end, "info_count")? as usize;
        if list as u64 + 4 + count as u64 * 4 > end as u64 {
            return Err(err(
                list,
                "info_count",
                format!("{} list of {count} entries does not fit", LISTS[l]),
            ));
        }
        let mut entries = vec![];
        for i in 0..count {
            let e = u32_at(b, list + 4 + 4 * i, end, "info_entry")? as usize;
            if e == 0 {
                entries.push(None);
                continue;
            }
            let o = at + e;
            let len = if entry_bytes == 0 {
                // Group entries: count plus (type, index) pairs.
                4 + 8 * u32_at(b, o, end, "group_count")?.min(4096) as usize
            } else {
                entry_bytes
            };
            if o + len > end {
                return Err(err(
                    list + 4 + 4 * i,
                    "info_entry",
                    format!("{} entry {i} runs past the INFO block", LISTS[l]),
                ));
            }
            let bytes = b[o..o + len].to_vec();
            let file_id =
                matches!(l, 0 | 1 | 2 | 3 | 7).then(|| u16::from_le_bytes([bytes[0], bytes[1]]));
            entries.push(Some(InfoEntry {
                offset: o,
                file_id,
                bytes,
            }));
        }
        lists.push(InfoList {
            offset: list,
            entries,
        });
    }
    Ok(lists)
}

fn name_at(b: &[u8], at: usize, end: usize) -> Option<String> {
    let s = b.get(at..end.min(at + MAX_NAME))?;
    let len = s.iter().position(|&c| c == 0)?;
    let s = &s[..len];
    s.iter()
        .all(|c| c.is_ascii_graphic() || *c == b' ')
        .then(|| String::from_utf8_lossy(s).into_owned())
}

fn parse_symbols(b: &[u8], at: usize, size: usize) -> Result<Vec<Vec<Option<String>>>, NdsError> {
    let end = at + size;
    tag_at(b, at, end, b"SYMB", "symb_block")?;
    let mut lists = vec![];
    for (l, list_name) in LISTS.iter().enumerate() {
        let rel = u32_at(b, at + 8 + 4 * l, end, "symb_list")? as usize;
        if rel == 0 {
            lists.push(vec![]);
            continue;
        }
        let list = at + rel;
        let count = u32_at(b, list, end, "symb_count")? as usize;
        let stride = if l == 1 { 8 } else { 4 };
        if list as u64 + 4 + (count * stride) as u64 > end as u64 {
            return Err(err(
                list,
                "symb_count",
                format!("{} name list of {count} entries does not fit", list_name),
            ));
        }
        let names = (0..count)
            .map(|i| {
                let rel = u32_at(b, list + 4 + stride * i, end, "symb_name").ok()? as usize;
                (rel != 0).then(|| name_at(b, at + rel, end)).flatten()
            })
            .collect();
        lists.push(names);
    }
    Ok(lists)
}

/// Any recognized NDS sound file: what discovery reports.
pub fn discovery_hint(b: &[u8]) -> Option<Value> {
    let kind = Kind::from_magic(b)?;
    let mut hint =
        json!({"format": format!("nds_{}", kind.name()), "inspect_with": "inspect_nds_sound"});
    match FileHeader::parse(b) {
        Err(e) => hint["unusable"] = json!(e.to_string()),
        Ok(h) => {
            hint["file_size"] = json!(h.file_size);
            hint["trailing_bytes"] = json!(h.trailing_bytes);
            match kind {
                Kind::Swar | Kind::Swav | Kind::Strm => {
                    hint["export_with"] = json!("export_nds_sample");
                }
                Kind::Sdat => {
                    hint["members"] =
                        json!("inspect_nds_sound view members; select one with member")
                }
                Kind::Sseq | Kind::Ssar => {
                    hint["note"] =
                        json!("sequence data is retained; SSEQ/SSAR playback is not implemented")
                }
                Kind::Sbnk => {}
            }
            if kind == Kind::Swar {
                hint["replace_with"] = json!("replace_nds_wave");
            }
        }
    }
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(id: &[u8; 4], size: usize, blocks: u16) -> Vec<u8> {
        let mut h = id.to_vec();
        h.extend([0xFF, 0xFE, 0x00, 0x01]);
        h.extend((size as u32).to_le_bytes());
        h.extend(16u16.to_le_bytes());
        h.extend(blocks.to_le_bytes());
        h
    }

    #[test]
    fn headers_refuse_located_problems() {
        assert_eq!(FileHeader::parse(b"SWAR").unwrap_err().field, "header");
        let mut h = header(b"SWAR", 16, 1);
        assert_eq!(FileHeader::parse(&h).unwrap().kind, Kind::Swar);
        h[4] = 0xFE;
        h[5] = 0xFF;
        assert_eq!(FileHeader::parse(&h).unwrap_err().offset, 4);
        let h = header(b"SWAR", 17, 1);
        assert_eq!(FileHeader::parse(&h).unwrap_err().field, "file_size");
        let h = header(b"XXXX", 16, 1);
        assert_eq!(FileHeader::parse(&h).unwrap_err().field, "id");
        let mut t = header(b"SSEQ", 16, 1);
        t.extend([1, 2]);
        assert_eq!(FileHeader::parse(&t).unwrap().trailing_bytes, 2);
    }

    #[test]
    fn wave_info_sample_counts_and_loops_follow_word_units() {
        let mut b = vec![2u8, 1, 0x22, 0x56, 0x4C, 0x02, 2, 0, 3, 0, 0, 0];
        b.extend([0u8; 20]);
        let w = WaveInfo::parse(&b, 0, b.len()).unwrap();
        assert_eq!((w.samples(), w.loop_sample(), w.data_bytes()), (32, 8, 20));
        b[6] = 0;
        assert_eq!(
            WaveInfo::parse(&b, 0, b.len()).unwrap_err().field,
            "loop_offset"
        );
        b[0] = 3;
        assert_eq!(
            WaveInfo::parse(&b, 0, b.len()).unwrap_err().field,
            "wave_type"
        );
        b[0] = 0;
        b[6] = 2;
        let w = WaveInfo::parse(&b, 0, b.len()).unwrap();
        assert_eq!((w.samples(), w.loop_sample()), (20, 8));
        b[8] = 9;
        assert_eq!(WaveInfo::parse(&b, 0, b.len()).unwrap_err().field, "length");
    }

    #[test]
    fn layout_reports_gaps_shared_and_overlapping_blocks() {
        let l = layout(
            [
                Some((10, 20)),
                Some((10, 20)),
                Some((24, 40)),
                Some((30, 44)),
            ]
            .into_iter(),
            8,
            50,
        );
        assert_eq!(l.shared, vec![(0, 1)]);
        assert_eq!(l.overlaps, vec![(2, 3)]);
        assert_eq!(l.gaps, vec![(8, 10), (20, 24), (44, 50)]);
        assert!(!l.increasing);
    }
}
