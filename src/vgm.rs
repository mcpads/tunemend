//! VGM container interpretation for versions 1.00 through 1.71, written from
//! the VGM 1.71 specification. Interpretation never changes the retained
//! original: every byte belongs to a reported range, every command keeps its
//! exact offset and bytes, and malformed input refuses with a byte offset.
use crate::ay::{AyConfig, AyVariant, Pan};
use crate::gb_apu::GbConfig;
use crate::huc6280::PceConfig;
use crate::nes_apu::NesConfig;
use crate::psg::{PsgConfig, PsgVariant};
use crate::wonderswan::WsConfig;
use serde_json::{Value, json};

/// VGM wait commands count samples at this rate.
pub const VGM_RATE: u32 = 44_100;
pub const MAX_VGM_BYTES: usize = 64 << 20;
pub const MAX_COMMANDS: usize = 8_000_000;
/// Newest version whose layout is interpreted.
pub const NEWEST_VERSION: u32 = 0x171;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VgmError {
    pub offset: usize,
    pub message: String,
}

fn fail<T>(offset: usize, message: impl Into<String>) -> Result<T, VgmError> {
    Err(VgmError {
        offset,
        message: message.into(),
    })
}

/// Header fields: offset, size, name and the first version that defines them.
pub(crate) const FIELDS: &[(usize, usize, &str, u32)] = &[
    (0x00, 4, "ident", 0x100),
    (0x04, 4, "eof_offset", 0x100),
    (0x08, 4, "version", 0x100),
    (0x0c, 4, "sn76489_clock", 0x100),
    (0x10, 4, "ym2413_clock", 0x100),
    (0x14, 4, "gd3_offset", 0x100),
    (0x18, 4, "total_samples", 0x100),
    (0x1c, 4, "loop_offset", 0x100),
    (0x20, 4, "loop_samples", 0x100),
    (0x24, 4, "rate", 0x101),
    (0x28, 2, "sn76489_feedback", 0x110),
    (0x2a, 1, "sn76489_shift_register_width", 0x110),
    (0x2b, 1, "sn76489_flags", 0x151),
    (0x2c, 4, "ym2612_clock", 0x110),
    (0x30, 4, "ym2151_clock", 0x110),
    (0x34, 4, "vgm_data_offset", 0x150),
    (0x38, 4, "sega_pcm_clock", 0x151),
    (0x3c, 4, "sega_pcm_interface_register", 0x151),
    (0x40, 4, "rf5c68_clock", 0x151),
    (0x44, 4, "ym2203_clock", 0x151),
    (0x48, 4, "ym2608_clock", 0x151),
    (0x4c, 4, "ym2610_clock", 0x151),
    (0x50, 4, "ym3812_clock", 0x151),
    (0x54, 4, "ym3526_clock", 0x151),
    (0x58, 4, "y8950_clock", 0x151),
    (0x5c, 4, "ymf262_clock", 0x151),
    (0x60, 4, "ymf278b_clock", 0x151),
    (0x64, 4, "ymf271_clock", 0x151),
    (0x68, 4, "ymz280b_clock", 0x151),
    (0x6c, 4, "rf5c164_clock", 0x151),
    (0x70, 4, "pwm_clock", 0x151),
    (0x74, 4, "ay8910_clock", 0x151),
    (0x78, 1, "ay8910_chip_type", 0x151),
    (0x79, 1, "ay8910_flags", 0x151),
    (0x7a, 1, "ym2203_ay8910_flags", 0x151),
    (0x7b, 1, "ym2608_ay8910_flags", 0x151),
    (0x7c, 1, "volume_modifier", 0x160),
    (0x7d, 1, "reserved_7d", 0x160),
    (0x7e, 1, "loop_base", 0x160),
    (0x7f, 1, "loop_modifier", 0x151),
    (0x80, 4, "gameboy_dmg_clock", 0x161),
    (0x84, 4, "nes_apu_clock", 0x161),
    (0x88, 4, "multipcm_clock", 0x161),
    (0x8c, 4, "upd7759_clock", 0x161),
    (0x90, 4, "okim6258_clock", 0x161),
    (0x94, 1, "okim6258_flags", 0x161),
    (0x95, 1, "k054539_flags", 0x161),
    (0x96, 1, "c140_chip_type", 0x161),
    (0x97, 1, "reserved_97", 0x161),
    (0x98, 4, "okim6295_clock", 0x161),
    (0x9c, 4, "k051649_clock", 0x161),
    (0xa0, 4, "k054539_clock", 0x161),
    (0xa4, 4, "huc6280_clock", 0x161),
    (0xa8, 4, "c140_clock", 0x161),
    (0xac, 4, "k053260_clock", 0x161),
    (0xb0, 4, "pokey_clock", 0x161),
    (0xb4, 4, "qsound_clock", 0x161),
    (0xb8, 4, "scsp_clock", 0x171),
    (0xbc, 4, "extra_header_offset", 0x170),
    (0xc0, 4, "wonderswan_clock", 0x171),
    (0xc4, 4, "vsu_clock", 0x171),
    (0xc8, 4, "saa1099_clock", 0x171),
    (0xcc, 4, "es5503_clock", 0x171),
    (0xd0, 4, "es5506_clock", 0x171),
    (0xd4, 1, "es5503_channels", 0x171),
    (0xd5, 1, "es5506_channels", 0x171),
    (0xd6, 1, "c352_clock_divider", 0x171),
    (0xd7, 1, "reserved_d7", 0x171),
    (0xd8, 4, "x1_010_clock", 0x171),
    (0xdc, 4, "c352_clock", 0x171),
    (0xe0, 4, "ga20_clock", 0x171),
    (0xe4, 28, "reserved_e4", 0x171),
];

fn le(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .rev()
        .fold(0u32, |v, &b| (v << 8) | u32::from(b))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn version_text(version: u32) -> String {
    format!("{:x}.{:02x}", version >> 8, version & 0xff)
}

/// Command classes; `rendered` says what `render_psg` does with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    PsgWrite,
    PsgStereo,
    Wait,
    End,
    SecondPsgWrite,
    SecondPsgStereo,
    Ym2612DacWait,
    ChipWrite,
    AyWrite,
    GbWrite,
    NesWrite,
    PceWrite,
    WsWrite,
    WsMemoryWrite,
    Ay8910StereoMask,
    DataBlock,
    PcmRamWrite,
    DacStream,
    PcmSeek,
    Reserved,
    NoOp,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::PsgWrite => "psg_write",
            Kind::PsgStereo => "psg_stereo",
            Kind::Wait => "wait",
            Kind::End => "end",
            Kind::SecondPsgWrite => "second_psg_write",
            Kind::SecondPsgStereo => "second_psg_stereo",
            Kind::Ym2612DacWait => "ym2612_dac_write_and_wait",
            Kind::ChipWrite => "other_chip_write",
            Kind::AyWrite => "ay8910_write",
            Kind::GbWrite => "gameboy_dmg_write",
            Kind::NesWrite => "nes_apu_write",
            Kind::PceWrite => "huc6280_write",
            Kind::WsWrite => "wonderswan_write",
            Kind::WsMemoryWrite => "wonderswan_memory_write",
            Kind::Ay8910StereoMask => "ay8910_stereo_mask",
            Kind::DataBlock => "data_block",
            Kind::PcmRamWrite => "pcm_ram_write",
            Kind::DacStream => "dac_stream_control",
            Kind::PcmSeek => "pcm_data_bank_seek",
            Kind::Reserved => "reserved",
            Kind::NoOp => "no_op",
        }
    }
    pub fn rendered(self) -> &'static str {
        match self {
            Kind::PsgWrite | Kind::PsgStereo => "applied to the PSG model",
            Kind::Wait => "advances time",
            Kind::End => "ends a pass",
            Kind::Ym2612DacWait => "advances time; the YM2612 DAC write is not rendered",
            Kind::SecondPsgWrite | Kind::SecondPsgStereo => {
                "not rendered: the second chip of a dual-chip file is not modelled"
            }
            Kind::DacStream => {
                "not rendered: DAC streams are not played, including streams aimed at the PSG"
            }
            Kind::AyWrite => {
                "applied to the AY-3-8910/YM2149 model when the header declares an AY8910 clock (bit 7 of the register byte selects the second chip)"
            }
            Kind::GbWrite => {
                "applied to the Game Boy DMG APU model when the header declares a GameBoy DMG clock (bit 7 of the register byte selects the second chip)"
            }
            Kind::NesWrite => {
                "applied to the NES APU model when the header declares an NES APU clock (registers 0x00..0x1F = $4000..$401F; bit 7 selects the second chip); FDS registers are not rendered"
            }
            Kind::PceWrite => {
                "applied to the HuC6280 PSG model when the header declares a HuC6280 clock (registers 0x00..0x09 = R0..R9 at $0800..$0809; bit 7 selects the second chip)"
            }
            Kind::WsWrite => {
                "applied to the WonderSwan model when the header declares a WonderSwan clock (registers 0x00..0x1F = ports $80..$9F; bit 7 selects the second chip)"
            }
            Kind::WsMemoryWrite => {
                "applied to the WonderSwan model's internal RAM (wavetables) when the header declares a WonderSwan clock"
            }
            Kind::Ay8910StereoMask => {
                "applied to the AY model's channel routing when the header declares an AY8910; YM2203 SSG masks are not rendered"
            }
            Kind::NoOp => "skipped",
            _ => "not rendered: another chip or its data",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub offset: usize,
    pub length: usize,
    pub kind: Kind,
    /// Cumulative VGM samples before this command.
    pub at: u64,
    /// Samples this command waits.
    pub wait: u32,
}

impl Command {
    pub fn opcode(&self, bytes: &[u8]) -> u8 {
        bytes[self.offset]
    }
}

pub fn chip_name(opcode: u8) -> &'static str {
    const FM: [&str; 15] = [
        "YM2413",
        "YM2612 port 0",
        "YM2612 port 1",
        "YM2151",
        "YM2203",
        "YM2608 port 0",
        "YM2608 port 1",
        "YM2610 port 0",
        "YM2610 port 1",
        "YM3812",
        "YM3526",
        "Y8950",
        "YMZ280B",
        "YMF262 port 0",
        "YMF262 port 1",
    ];
    const B: [&str; 16] = [
        "RF5C68",
        "RF5C164",
        "PWM",
        "GameBoy DMG",
        "NES APU",
        "MultiPCM",
        "uPD7759",
        "OKIM6258",
        "OKIM6295",
        "HuC6280",
        "K053260",
        "Pokey",
        "WonderSwan",
        "SAA1099",
        "ES5506",
        "GA20",
    ];
    const C: [&str; 9] = [
        "Sega PCM memory",
        "RF5C68 memory",
        "RF5C164 memory",
        "MultiPCM bank offset",
        "QSound",
        "SCSP",
        "WonderSwan memory",
        "VSU",
        "X1-010",
    ];
    const D: [&str; 7] = [
        "YMF278B", "YMF271", "SCC1", "K054539", "C140", "ES5503", "ES5506",
    ];
    match opcode {
        0x51..=0x5f => FM[usize::from(opcode - 0x51)],
        0xa0 => "AY8910",
        0xa1..=0xaf => FM[usize::from(opcode - 0xa1)],
        0xb0..=0xbf => B[usize::from(opcode - 0xb0)],
        0xc0..=0xc8 => C[usize::from(opcode - 0xc0)],
        0xd0..=0xd6 => D[usize::from(opcode - 0xd0)],
        0xe1 => "C352",
        _ => "unknown",
    }
}

/// Length and class of the command at `offset`, or `None` for an opcode the
/// specification does not define (its length is unknown).
fn classify(bytes: &[u8], offset: usize, version: u32) -> Option<(usize, Kind, u32)> {
    let opcode = bytes[offset];
    Some(match opcode {
        0x00 => (1, Kind::NoOp, 0),
        0x30 => (2, Kind::SecondPsgWrite, 0),
        0x31 => (2, Kind::Ay8910StereoMask, 0),
        0x3f => (2, Kind::SecondPsgStereo, 0),
        0x32..=0x3e => (2, Kind::Reserved, 0),
        0x40..=0x4e => (if version < 0x160 { 2 } else { 3 }, Kind::Reserved, 0),
        0x4f => (2, Kind::PsgStereo, 0),
        0x50 => (2, Kind::PsgWrite, 0),
        0xa0 => (3, Kind::AyWrite, 0),
        0xb3 => (3, Kind::GbWrite, 0),
        0xb4 => (3, Kind::NesWrite, 0),
        0xb9 => (3, Kind::PceWrite, 0),
        0xbc => (3, Kind::WsWrite, 0),
        0xc6 => (4, Kind::WsMemoryWrite, 0),
        0x51..=0x5f | 0xa1..=0xbf => (3, Kind::ChipWrite, 0),
        0x61 => {
            let wait = bytes.get(offset + 1..offset + 3).map(le).unwrap_or(0);
            (3, Kind::Wait, wait)
        }
        0x62 => (1, Kind::Wait, 735),
        0x63 => (1, Kind::Wait, 882),
        0x66 => (1, Kind::End, 0),
        0x67 => (7, Kind::DataBlock, 0),
        0x68 => (12, Kind::PcmRamWrite, 0),
        0x70..=0x7f => (1, Kind::Wait, u32::from(opcode & 0x0f) + 1),
        0x80..=0x8f => (1, Kind::Ym2612DacWait, u32::from(opcode & 0x0f)),
        0x90 | 0x91 | 0x95 => (5, Kind::DacStream, 0),
        0x92 => (6, Kind::DacStream, 0),
        0x93 => (11, Kind::DacStream, 0),
        0x94 => (2, Kind::DacStream, 0),
        0xc0..=0xc8 | 0xd0..=0xd6 => (4, Kind::ChipWrite, 0),
        0xc9..=0xcf | 0xd7..=0xdf => (4, Kind::Reserved, 0),
        0xe0 => (5, Kind::PcmSeek, 0),
        0xe1 => (5, Kind::ChipWrite, 0),
        0xe2..=0xff => (5, Kind::Reserved, 0),
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub struct Gd3 {
    pub offset: usize,
    pub end: usize,
    pub version: u32,
    pub strings: Vec<String>,
}

const GD3_NAMES: [&str; 11] = [
    "track_name",
    "track_name_japanese",
    "game_name",
    "game_name_japanese",
    "system_name",
    "system_name_japanese",
    "author",
    "author_japanese",
    "release_date",
    "converted_by",
    "notes",
];

fn parse_gd3(bytes: &[u8], offset: usize) -> Result<Gd3, VgmError> {
    if offset + 12 > bytes.len() {
        return fail(offset, "the GD3 tag header runs past the end of the file");
    }
    if &bytes[offset..offset + 4] != b"Gd3 " {
        return fail(offset, "the GD3 offset does not point at a 'Gd3 ' tag");
    }
    let version = le(&bytes[offset + 4..offset + 8]);
    let length = le(&bytes[offset + 8..offset + 12]) as usize;
    let start = offset + 12;
    let end = start
        .checked_add(length)
        .filter(|&e| e <= bytes.len())
        .ok_or(VgmError {
            offset: offset + 8,
            message: format!("the GD3 length {length} runs past the end of the file"),
        })?;
    if !length.is_multiple_of(2) {
        return fail(
            offset + 8,
            "the GD3 length is not a whole number of UTF-16 units",
        );
    }
    let units: Vec<u16> = bytes[start..end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    let mut strings: Vec<String> = units
        .split(|&u| u == 0)
        .map(String::from_utf16_lossy)
        .collect();
    // The last string is terminated, so splitting leaves one empty tail.
    if units.last() == Some(&0) {
        strings.pop();
    }
    Ok(Gd3 {
        offset,
        end,
        version,
        strings,
    })
}

/// A parsed VGM file. Borrowed bytes stay the authority for raw values.
#[derive(Debug, Clone)]
pub struct Vgm<'a> {
    pub bytes: &'a [u8],
    pub version: u32,
    pub data_start: usize,
    /// Offset after the end command.
    pub stream_end: usize,
    pub commands: Vec<Command>,
    pub gd3: Option<Result<Gd3, VgmError>>,
    pub loop_start: Option<usize>,
    /// Index of the command at the loop offset, or why there is none.
    pub loop_index: Option<Result<usize, VgmError>>,
    pub warnings: Vec<String>,
}

impl<'a> Vgm<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, VgmError> {
        if bytes.starts_with(&[0x1f, 0x8b]) {
            return fail(
                0,
                "gzip-compressed VGZ data; decompress it to a .vgm file and load that (the retained original is unaffected)",
            );
        }
        if bytes.len() < 0x40 {
            return fail(
                bytes.len(),
                format!("a VGM header needs 64 bytes; the file has {}", bytes.len()),
            );
        }
        if &bytes[..4] != b"Vgm " {
            return fail(0, "missing the 'Vgm ' identifier");
        }
        if bytes.len() > MAX_VGM_BYTES {
            return fail(
                MAX_VGM_BYTES,
                format!("files above {MAX_VGM_BYTES} bytes are not interpreted"),
            );
        }
        let mut warnings = Vec::new();
        let version = le(&bytes[8..12]);
        let bcd = (0..8).all(|i| (version >> (4 * i)) & 0xf <= 9);
        if !bcd {
            return fail(8, format!("version 0x{version:08x} is not BCD"));
        }
        if version > NEWEST_VERSION {
            warnings.push(format!(
                "version {} is newer than 1.71; fields and commands added later are not interpreted",
                version_text(version)
            ));
        }
        let data_offset = le(&bytes[0x34..0x38]) as usize;
        let data_start = if version >= 0x150 && data_offset != 0 {
            0x34 + data_offset
        } else {
            0x40
        };
        if data_start < 0x40 {
            return fail(
                0x34,
                "the data offset points inside the first 64 header bytes",
            );
        }
        if data_start > bytes.len() {
            return fail(
                0x34,
                format!("the data offset points at byte {data_start}, past the end of the file"),
            );
        }
        let eof = le(&bytes[4..8]) as usize + 4;
        if eof != bytes.len() {
            warnings.push(format!(
                "the EoF offset declares {eof} bytes; the file has {}",
                bytes.len()
            ));
        }
        let gd3_field = le(&bytes[0x14..0x18]) as usize;
        let gd3 = (gd3_field != 0).then(|| parse_gd3(bytes, 0x14 + gd3_field));
        let mut bound = bytes.len();
        if let Some(Ok(tag)) = &gd3
            && tag.offset >= data_start
        {
            bound = bound.min(tag.offset);
        }
        let mut commands = Vec::new();
        let (mut offset, mut at) = (data_start, 0u64);
        let stream_end = loop {
            if offset >= bound {
                return fail(
                    offset,
                    format!(
                        "no end command (0x66) before byte {bound}; {} commands read",
                        commands.len()
                    ),
                );
            }
            if commands.len() >= MAX_COMMANDS {
                return fail(
                    offset,
                    format!("more than {MAX_COMMANDS} commands are not interpreted"),
                );
            }
            let opcode = bytes[offset];
            let Some((mut length, kind, wait)) = classify(bytes, offset, version) else {
                return fail(
                    offset,
                    format!(
                        "command 0x{opcode:02x} is not defined by VGM 1.71, so its length is unknown"
                    ),
                );
            };
            if offset + length > bound {
                return fail(
                    offset,
                    format!(
                        "command 0x{opcode:02x} needs {length} bytes; {} remain before byte {bound}",
                        bound - offset
                    ),
                );
            }
            if kind == Kind::DataBlock {
                if bytes[offset + 1] != 0x66 {
                    return fail(offset + 1, "a data block (0x67) must continue with 0x66");
                }
                let size = data_block_size(bytes[offset + 2], le(&bytes[offset + 3..offset + 7]));
                if size > bound - offset - 7 {
                    return fail(
                        offset + 3,
                        format!("data block of {size} bytes runs past byte {bound}"),
                    );
                }
                length += size;
            }
            if kind == Kind::PcmRamWrite && bytes[offset + 1] != 0x66 {
                return fail(offset + 1, "a PCM RAM write (0x68) must continue with 0x66");
            }
            commands.push(Command {
                offset,
                length,
                kind,
                at,
                wait,
            });
            at += u64::from(wait);
            offset += length;
            if kind == Kind::End {
                break offset;
            }
        };
        let loop_field = le(&bytes[0x1c..0x20]) as usize;
        let loop_start = (loop_field != 0).then_some(0x1c + loop_field);
        let loop_index = loop_start.map(|start| {
            commands
                .binary_search_by_key(&start, |c| c.offset)
                .ok()
                .filter(|&i| commands[i].kind != Kind::End)
                .ok_or(VgmError {
                    offset: 0x1c,
                    message: format!(
                        "the loop offset points at byte {start}, which is not the start of a command before the end command"
                    ),
                })
        });
        Ok(Self {
            bytes,
            version,
            data_start,
            stream_end,
            commands,
            gd3,
            loop_start,
            loop_index,
            warnings,
        })
    }

    /// The field's raw bytes when it lies inside the header and the version
    /// defines it; header bytes overlapping the data are treated as zero.
    fn field(&self, offset: usize, size: usize, since: u32) -> Option<&'a [u8]> {
        (offset + size <= self.data_start.min(0x100) && self.version >= since)
            .then(|| &self.bytes[offset..offset + size])
    }
    pub fn u32_field(&self, name: &str) -> u32 {
        FIELDS
            .iter()
            .find(|f| f.2 == name)
            .and_then(|&(o, s, _, v)| self.field(o, s, v))
            .map(le)
            .unwrap_or(0)
    }
    pub fn total_samples(&self) -> u64 {
        self.commands.last().map_or(0, |c| c.at + u64::from(c.wait))
    }
    pub fn end_index(&self) -> usize {
        self.commands.len() - 1
    }
    pub fn version_text(&self) -> String {
        version_text(self.version)
    }

    /// Header rows: every field with raw bytes, value and whether it applies.
    pub fn header_rows(&self) -> Vec<Value> {
        FIELDS
            .iter()
            .map(|&(offset, size, name, since)| {
                let raw = self.bytes.get(offset..offset + size);
                let applies = self.field(offset, size, since).is_some();
                let value = if applies { raw.map(le) } else { None };
                let mut row = json!({"offset":offset,"size":size,"name":name,"since":version_text(since),
                    "raw":raw.filter(|_| offset + size <= self.data_start).map(hex),"applies":applies});
                if size <= 4 {
                    row["value"] = json!(value);
                }
                if let Some(meaning) = self.meaning(name, value.unwrap_or(0), applies) {
                    row["meaning"] = meaning;
                }
                if !applies && offset + size <= self.data_start {
                    row["note"] = json!(if self.version < since {
                        "present in the bytes but not defined for this version; retained, not interpreted"
                    } else {
                        "inside the header but not defined"
                    });
                } else if !applies {
                    row["note"] = json!("beyond the data offset: treated as zero");
                }
                row
            })
            .collect()
    }

    fn meaning(&self, name: &str, value: u32, applies: bool) -> Option<Value> {
        Some(match name {
            "ident" => json!("Vgm "),
            "eof_offset" => json!({"absolute": value as usize + 4, "file_bytes": self.bytes.len()}),
            "version" => json!(self.version_text()),
            "sn76489_clock" => {
                json!({"clock_hz": value & 0x3fff_ffff, "dual_chip": value & 0x4000_0000 != 0, "t6w28": value & 0x8000_0000 != 0})
            }
            "gd3_offset" | "loop_offset" | "vgm_data_offset" if value != 0 => {
                let base = match name {
                    "gd3_offset" => 0x14,
                    "loop_offset" => 0x1c,
                    _ => 0x34,
                };
                json!({"absolute": base + value as usize})
            }
            "vgm_data_offset" => {
                json!({"absolute": self.data_start, "rule": "0 or before 1.50: data starts at 0x40"})
            }
            "total_samples" | "loop_samples" => {
                json!({"seconds": f64::from(value) / f64::from(VGM_RATE)})
            }
            "sn76489_feedback" if !applies || value == 0 => {
                json!({"assumed": "0x0009", "rule": "before 1.10 or zero: Sega 0x0009"})
            }
            "sn76489_feedback" => json!(format!("0x{value:04x}")),
            "sn76489_shift_register_width" if !applies || value == 0 => {
                json!({"assumed": 16, "rule": "before 1.10 or zero: 16"})
            }
            "sn76489_flags" if applies => {
                json!({"frequency_0_is_0x400": value & 1 != 0, "output_negate": value & 2 != 0,
                "game_gear_stereo": value & 4 == 0, "clock_divider_8": value & 8 == 0, "xnor_noise": value & 16 != 0, "reserved_bits": value & 0xe0})
            }
            "ym2612_clock" if self.version < 0x110 => {
                json!({"uses_ym2413_clock": self.u32_field("ym2413_clock")})
            }
            "ym2151_clock" if self.version < 0x110 => {
                json!({"uses_ym2413_clock": self.u32_field("ym2413_clock")})
            }
            "ay8910_clock" if value != 0 => {
                json!({"clock_hz": value & 0x3fff_ffff, "dual_chip": value & 0x4000_0000 != 0})
            }
            "gameboy_dmg_clock" if value != 0 => {
                json!({"clock_hz": value & 0x3fff_ffff, "dual_chip": value & 0x4000_0000 != 0})
            }
            "nes_apu_clock" if value != 0 => {
                json!({"clock_hz": value & 0x3fff_ffff, "dual_chip": value & 0x4000_0000 != 0, "fds": value & 0x8000_0000 != 0})
            }
            "huc6280_clock" if value != 0 => {
                json!({"clock_hz": value & 0x3fff_ffff, "dual_chip": value & 0x4000_0000 != 0, "bit_31": value & 0x8000_0000 != 0})
            }
            "wonderswan_clock" if value != 0 => {
                json!({"clock_hz": value & 0x3fff_ffff, "dual_chip": value & 0x4000_0000 != 0, "bit_31": value & 0x8000_0000 != 0})
            }
            "ay8910_chip_type" if applies => json!({"name": ay_type_name(value as u8)}),
            "ay8910_flags" if applies => {
                json!({"legacy_output": value & 1 != 0, "single_output": value & 2 != 0, "discrete_output": value & 4 != 0,
                    "raw_output": value & 8 != 0, "ym2149_pin_26_clock_halved": value & 16 != 0,
                    "zx_spectrum_pan_left_center_right": value & 0x80 != 0, "reserved_bits": value & 0x60})
            }
            "volume_modifier" if applies => {
                let vm = if value <= 192 {
                    value as i32
                } else {
                    value as i32 - 256
                };
                json!({"gain": (f64::from(if vm == -63 { -64 } else { vm }) / 32.0).exp2(), "applied_by_render_psg": false})
            }
            "loop_base" if applies => {
                json!({"signed": value as u8 as i8, "applied_by_render_psg": false})
            }
            "loop_modifier" if applies => {
                json!({"factor_sixteenths": if value == 0 { 16 } else { value }, "applied_by_render_psg": false})
            }
            _ => return None,
        })
    }

    /// PSG settings the header declares, with the authority for each.
    pub fn psg_header(&self) -> Value {
        let clock = self.u32_field("sn76489_clock");
        let feedback_field = self.u32_field("sn76489_feedback");
        let width_field = self.u32_field("sn76489_shift_register_width");
        let flags = self.field(0x2b, 1, 0x151).map(|b| b[0]);
        let mut value = json!({"clock_hz": clock & 0x3fff_ffff, "dual_chip": clock & 0x4000_0000 != 0, "t6w28": clock & 0x8000_0000 != 0,
            "feedback": format!("0x{:04x}", if feedback_field == 0 { 9 } else { feedback_field }),
            "feedback_authority": if feedback_field == 0 { "default 0x0009 (field absent, before 1.10 or zero)" } else { "header" },
            "shift_register_width": if width_field == 0 { 16 } else { width_field },
            "width_authority": if width_field == 0 { "default 16 (field absent, before 1.10 or zero)" } else { "header" },
            "flags": flags.map(|f| format!("0x{f:02x}"))});
        match self.psg_config() {
            Ok((config, authority)) => {
                value["config"] = config.to_json();
                value["flags_authority"] = json!(authority);
            }
            Err(problem) => value["render_refusal"] = json!(problem),
        }
        value
    }

    /// The model configuration for this file's first PSG, or why it cannot be rendered.
    pub fn psg_config(&self) -> Result<(PsgConfig, &'static str), String> {
        let clock = self.u32_field("sn76489_clock");
        if clock & 0x3fff_ffff == 0 {
            return Err("the header declares no SN76489 (clock 0)".into());
        }
        if clock & 0x8000_0000 != 0 {
            return Err(
                "the header declares a T6W28 (Neo Geo Pocket), whose split channels are not modelled"
                    .into(),
            );
        }
        let feedback = match self.u32_field("sn76489_feedback") {
            0 => 9,
            v => v as u16,
        };
        let width = match self.u32_field("sn76489_shift_register_width") {
            0 => 16,
            v => v.min(255) as u8,
        };
        let flags = self.field(0x2b, 1, 0x151).map(|b| b[0]);
        if let Some(f) = flags {
            if f & 8 != 0 {
                return Err("flag bit 3 turns the /8 clock divider off (SN94624-style /2 parts), which is not modelled".into());
            }
            if f & 16 != 0 {
                return Err(
                    "flag bit 4 declares XNOR noise feedback (NCR8496), which is not modelled"
                        .into(),
                );
            }
        }
        let discrete = PsgVariant::Sn76489.config();
        let preset = if (feedback, width) == (discrete.taps, discrete.lfsr_width) {
            (
                discrete,
                "no flags before 1.51: the discrete SN76489 preset matched by feedback and width",
            )
        } else {
            (
                PsgVariant::Sega.config(),
                "no flags before 1.51: the Sega preset",
            )
        };
        let (zero, stereo, authority) = match flags {
            Some(f) => (f & 1 != 0, f & 4 == 0, "header flags"),
            None => (preset.0.zero_period_is_1024, preset.0.stereo_port, preset.1),
        };
        let config = PsgConfig {
            lfsr_width: width,
            taps: feedback,
            zero_period_is_1024: zero,
            negate_output: flags.is_some_and(|f| f & 2 != 0),
            stereo_port: stereo,
        };
        config.check()?;
        Ok((config, authority))
    }

    /// The AY8910-family chip the header declares: its configuration, clock,
    /// dual-chip bit and header description, or why it cannot be rendered.
    pub fn ay_config(&self) -> Option<Result<(AyConfig, u32, bool, Value), String>> {
        let raw = self.u32_field("ay8910_clock");
        let clock = raw & 0x3fff_ffff;
        if clock == 0 {
            return None;
        }
        let chip_type = self.field(0x78, 1, 0x151).map_or(0, |b| b[0]);
        let flags = self.field(0x79, 1, 0x151).map_or(0, |b| b[0]);
        let variant = match chip_type {
            0x00..=0x02 => AyVariant::Ay8910,
            0x10..=0x13 => AyVariant::Ym2149,
            0x03 => {
                return Some(Err(
                    "chip type 0x03 declares an AY8930, whose expanded mode is not modelled".into(),
                ));
            }
            other => {
                return Some(Err(format!(
                    "AY8910 chip type 0x{other:02x} is not defined by VGM 1.71"
                )));
            }
        };
        let halved = flags & 16 != 0;
        if halved && variant != AyVariant::Ym2149 {
            return Some(Err(
                "flag bit 4 halves the clock through YM2149 pin 26, but the chip type is not a YM2149".into(),
            ));
        }
        let mut config = AyConfig::new(variant);
        if halved {
            config.clock_divider *= 2;
        }
        if flags & 0x80 != 0 {
            config.pan = [Pan::Left, Pan::Center, Pan::Right];
        }
        let header = json!({"clock_hz":clock,"dual_chip":raw & 0x4000_0000 != 0,"chip_type":format!("0x{chip_type:02x}"),
            "chip_type_name":ay_type_name(chip_type),"flags":format!("0x{flags:02x}"),"variant":variant,
            "clock_halved_by_pin_26":halved,"pan_authority":if flags & 0x80 != 0 {"flag bit 7: A left, B center, C right"} else {"default: every channel center"},
            "not_applied":"output-mode flag bits 0..3 (legacy, single, discrete, raw)"});
        Some(Ok((config, clock, raw & 0x4000_0000 != 0, header)))
    }

    /// The Game Boy DMG APU the header declares.
    pub fn gb_config(&self) -> Option<(GbConfig, u32, bool, Value)> {
        let raw = self.u32_field("gameboy_dmg_clock");
        let clock = raw & 0x3fff_ffff;
        (clock != 0).then(|| {
            (
                GbConfig::dmg(),
                clock,
                raw & 0x4000_0000 != 0,
                json!({"clock_hz":clock,"dual_chip":raw & 0x4000_0000 != 0,"variant":"dmg",
                    "note":"VGM declares no DMG/CGB distinction; the DMG model is used"}),
            )
        })
    }

    /// The NES APU the header declares: configuration, clock, dual-chip bit,
    /// FDS bit and header description. The 2A07 (PAL) tables apply when the
    /// clock is within 1% of 1662607 Hz.
    pub fn nes_config(&self) -> Option<(NesConfig, u32, bool, bool, Value)> {
        let raw = self.u32_field("nes_apu_clock");
        let clock = raw & 0x3fff_ffff;
        (clock != 0).then(|| {
            let config = NesConfig::for_clock(clock);
            let (dual, fds) = (raw & 0x4000_0000 != 0, raw & 0x8000_0000 != 0);
            let mut header = json!({"clock_hz":clock,"dual_chip":dual,"fds":fds,"region":config.region,"part":config.region.part(),
                "region_authority":"inferred from the clock: within 1% of 1662607 Hz uses the 2A07 (PAL) tables, otherwise the 2A03 (NTSC) tables"});
            if fds {
                header["fds_note"] = json!("bit 31 declares the FDS expansion; its registers ($4040..$409E, $4023) are retained and reported as not rendered");
            }
            (config, clock, dual, fds, header)
        })
    }

    /// The HuC6280 the header declares: configuration, clock, dual-chip bit
    /// and header description.
    pub fn pce_config(&self) -> Option<(PceConfig, u32, bool, Value)> {
        let raw = self.u32_field("huc6280_clock");
        let clock = raw & 0x3fff_ffff;
        (clock != 0).then(|| {
            let dual = raw & 0x4000_0000 != 0;
            let mut header = json!({"clock_hz":clock,"dual_chip":dual,
                "tick":"one cycle of the header clock (the PC Engine's 7.16 MHz master clock / 2 is 3579545 Hz)"});
            if raw & 0x8000_0000 != 0 {
                header["bit_31"] = json!("set; VGM 1.71 defines no meaning for it, so it is reported and not interpreted");
            }
            (PceConfig::new(clock), clock, dual, header)
        })
    }

    /// The WonderSwan the header declares: configuration, clock, dual-chip
    /// bit and header description.
    pub fn ws_config(&self) -> Option<(WsConfig, u32, bool, Value)> {
        let raw = self.u32_field("wonderswan_clock");
        let clock = raw & 0x3fff_ffff;
        (clock != 0).then(|| {
            let dual = raw & 0x4000_0000 != 0;
            let mut header = json!({"clock_hz":clock,"dual_chip":dual,
                "tick":"one CPU cycle (3072000 Hz on the console); output every 128 cycles",
                "register_mapping":"0xBC register aa is the I/O port $80 + (aa & 0x7F); 0xC6 writes internal RAM, where the wavetables live"});
            if raw & 0x8000_0000 != 0 {
                header["bit_31"] = json!("set; VGM 1.71 defines no meaning for it, so it is reported and not interpreted");
            }
            (WsConfig::new(clock), clock, dual, header)
        })
    }

    /// Type, second-chip bit and payload range of a data block command.
    pub fn data_block(&self, index: usize) -> Option<(u8, bool, std::ops::Range<usize>)> {
        let c = self.commands.get(index)?;
        if c.kind != Kind::DataBlock {
            return None;
        }
        let raw = &self.bytes[c.offset..c.offset + c.length];
        let kind = raw[2];
        let second = kind >= 0x80 && le(&raw[3..7]) & 0x8000_0000 != 0;
        Some((kind, second, c.offset + 7..c.offset + c.length))
    }

    /// Every byte range of the file with its role.
    pub fn layout(&self) -> Vec<Value> {
        let mut ranges = vec![json!({"start":0,"end":self.data_start.min(0x100),"role":"header"})];
        if self.data_start > 0x100 {
            ranges.push(json!({"start":0x100,"end":self.data_start,"role":"extra header bytes before the data (retained, not interpreted)"}));
        }
        ranges.push(json!({"start":self.data_start,"end":self.stream_end,"role":"command stream through the end command"}));
        let mut cursor = self.stream_end;
        if let Some(Ok(tag)) = &self.gd3
            && tag.offset >= cursor
        {
            if tag.offset > cursor {
                ranges.push(json!({"start":cursor,"end":tag.offset,"role":"bytes after the end command (retained, not interpreted)"}));
            }
            ranges.push(json!({"start":tag.offset,"end":tag.end,"role":"GD3 tag"}));
            cursor = tag.end;
        }
        if cursor < self.bytes.len() {
            ranges.push(json!({"start":cursor,"end":self.bytes.len(),"role":"trailing bytes (retained, not interpreted)"}));
        }
        ranges
    }

    pub fn gd3_json(&self) -> Value {
        match &self.gd3 {
            None => json!(null),
            Some(Err(e)) => json!({"error":{"offset":e.offset,"message":e.message}}),
            Some(Ok(tag)) => {
                let mut fields = serde_json::Map::new();
                for (i, s) in tag.strings.iter().enumerate() {
                    let key = GD3_NAMES
                        .get(i)
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| format!("extra_{i}"));
                    fields.insert(key, json!(s));
                }
                json!({"offset":tag.offset,"end":tag.end,"version":format!("0x{:08x}",tag.version),"strings":fields,
                    "string_count":tag.strings.len()})
            }
        }
    }

    /// Exact bytes and decoded meaning of one command; PSG writes need the
    /// effect computed by the caller's register tracker.
    pub fn command_json(&self, index: usize, effect: Option<Value>) -> Value {
        let c = self.commands[index];
        let raw = &self.bytes[c.offset..c.offset + c.length];
        let opcode = raw[0];
        let shown = if c.kind == Kind::DataBlock {
            &raw[..7]
        } else {
            raw
        };
        let mut value = json!({"index":index,"offset":c.offset,"length":c.length,"bytes":hex(shown),
            "opcode":format!("0x{opcode:02x}"),"kind":c.kind.name(),"at_sample":c.at,"render":c.kind.rendered()});
        let meaning = match c.kind {
            Kind::PsgWrite | Kind::SecondPsgWrite => effect.unwrap_or(json!({"data":raw[1]})),
            Kind::PsgStereo | Kind::SecondPsgStereo => {
                let d = raw[1];
                json!({"port":"0x06","data":format!("0x{d:02x}"),"left_channels":(0..4).filter(|c| d & (0x10 << c) != 0).collect::<Vec<_>>(),
                    "right_channels":(0..4).filter(|c| d & (1 << c) != 0).collect::<Vec<_>>()})
            }
            Kind::Wait => json!({"samples":c.wait}),
            Kind::Ym2612DacWait => {
                json!({"chip":"YM2612 port 0","address":"0x2a","data":"next byte of the PCM data bank","samples":c.wait})
            }
            Kind::ChipWrite => match raw.len() {
                3 => {
                    json!({"chip":chip_name(opcode),"second_chip":(0xa1..=0xaf).contains(&opcode),"address":raw[1],"data":raw[2]})
                }
                _ => json!({"chip":chip_name(opcode),"operands":hex(&raw[1..])}),
            },
            Kind::Ay8910StereoMask => {
                let d = raw[1];
                let pan =
                    |c: u8| ["off", "left", "right", "center"][usize::from((d >> (2 * c)) & 3)];
                json!({"chip":if d & 0x40 != 0 {"YM2203 SSG"} else {"AY8910"},"second_chip":d & 0x80 != 0,"mask":d,
                    "a":pan(0),"b":pan(1),"c":pan(2)})
            }
            Kind::WsMemoryWrite => {
                let mut meaning = effect.unwrap_or_else(|| json!({}));
                meaning["chip"] = json!("WonderSwan");
                meaning["offset"] = json!(u16::from_be_bytes([raw[1], raw[2]]));
                meaning["data"] = json!(raw[3]);
                meaning
            }
            Kind::AyWrite | Kind::GbWrite | Kind::NesWrite | Kind::PceWrite | Kind::WsWrite => {
                let mut meaning = effect.unwrap_or_else(|| json!({}));
                meaning["chip"] = json!(match c.kind {
                    Kind::AyWrite => "AY8910",
                    Kind::GbWrite => "GameBoy DMG",
                    Kind::PceWrite => "HuC6280",
                    Kind::WsWrite => "WonderSwan",
                    _ => "NES APU",
                });
                if c.kind == Kind::NesWrite && raw[1] & 0x7f >= 0x20 {
                    let r = raw[1] & 0x7f;
                    let address = match r {
                        0x3f => 0x4023,
                        0x20..=0x3e => 0x4060 + u16::from(r),
                        _ => 0x4000 + u16::from(r),
                    };
                    meaning["address"] = json!(format!("${address:04X}"));
                    meaning["expansion"] = json!("FDS");
                    meaning["note"] = json!("FDS expansion audio register: retained, not modelled");
                }
                meaning["second_chip"] = json!(raw[1] & 0x80 != 0);
                meaning["register_byte"] = json!(raw[1]);
                meaning["data"] = json!(raw[2]);
                meaning
            }
            Kind::DataBlock => {
                let kind = raw[2];
                let field = le(&raw[3..7]);
                let mut meaning = json!({"type":format!("0x{kind:02x}"),"class":data_block_class(kind),"size":c.length - 7,
                    "second_chip":kind >= 0x80 && field & 0x8000_0000 != 0,
                    "data_range":[c.offset + 7, c.offset + c.length]});
                if kind == 0xc2 {
                    meaning["chip"] = json!("NES APU");
                    if c.length >= 9 {
                        let start = le(&raw[7..9]);
                        let bytes = c.length - 9;
                        meaning["nes_apu_ram_write"] = json!({"start":format!("${start:04X}"),"bytes":bytes,
                            "end":format!("${:04X}", (start as usize + bytes).saturating_sub(1).min(0xffff)),
                            "role":"DMC sample memory: bytes the DMC fetches from CPU addresses $8000..$FFFF"});
                    } else {
                        meaning["error"] =
                            json!("an NES APU RAM write needs a 2-byte start address");
                    }
                    value["render"] = json!(
                        "applied to the NES APU model's DMC memory at its stream position when the header declares an NES APU clock"
                    );
                }
                if kind == 0x05 {
                    meaning["chip"] = json!("HuC6280");
                    meaning["note"] = json!(
                        "PCM data for DAC stream commands (DDA samples); DAC streams are not rendered"
                    );
                }
                if kind == 0x07 {
                    meaning["chip"] = json!("NES APU");
                    meaning["note"] =
                        json!("DPCM data for DAC stream commands; DAC streams are not rendered");
                }
                meaning
            }
            Kind::PcmRamWrite => {
                json!({"chip_type":raw[2],"read_offset":le(&raw[3..6]),"write_offset":le(&raw[6..9]),
                "size":match le(&raw[9..12]) { 0 => 0x0100_0000, s => s }})
            }
            Kind::DacStream => {
                json!({"stream":raw[1],"operation":match opcode {0x90=>"setup",0x91=>"set data",0x92=>"set frequency",0x93=>"start",0x94=>"stop",_=>"fast start"},
                "targets_sn76489":opcode == 0x90 && raw[2] & 0x7f == 0,"operands":hex(&raw[1..])})
            }
            Kind::PcmSeek => json!({"offset":le(&raw[1..5])}),
            Kind::Reserved => {
                json!({"operands":hex(&raw[1..]),"note":"reserved by VGM 1.71; skipped with its defined length"})
            }
            Kind::End | Kind::NoOp => json!({}),
        };
        value["meaning"] = meaning;
        value
    }

    /// Every command kind with its count, opcodes and first offset.
    pub fn kinds(&self) -> Vec<Value> {
        let mut rows: std::collections::BTreeMap<
            Kind,
            (usize, usize, std::collections::BTreeSet<u8>),
        > = Default::default();
        for c in &self.commands {
            let row = rows
                .entry(c.kind)
                .or_insert((0, c.offset, Default::default()));
            row.0 += 1;
            row.2.insert(self.bytes[c.offset]);
        }
        rows.into_iter()
            .map(|(kind, (count, first, opcodes))| {
                let mut row = json!({"kind":kind.name(),"count":count,"first_offset":first,
                    "opcodes":opcodes.iter().map(|o| format!("0x{o:02x}")).collect::<Vec<_>>(),"render":kind.rendered()});
                if kind == Kind::ChipWrite {
                    let chips: std::collections::BTreeSet<&str> =
                        opcodes.iter().map(|&o| chip_name(o)).collect();
                    row["chips"] = json!(chips);
                }
                if kind == Kind::DataBlock
                    && (0..self.commands.len())
                        .any(|i| self.data_block(i).is_some_and(|(t, ..)| t == 0xc2))
                {
                    row["render"] = json!(
                        "NES APU RAM writes (type 0xC2) fill the NES APU model's DMC memory; other data blocks are not rendered"
                    );
                }
                row
            })
            .collect()
    }

    pub fn loop_json(&self) -> Value {
        let header_loop = self.u32_field("loop_samples");
        match (&self.loop_start, &self.loop_index) {
            (Some(start), Some(Ok(index))) => {
                let counted = self.total_samples() - self.commands[*index].at;
                json!({"offset":start,"command_index":index,"at_sample":self.commands[*index].at,
                    "header_loop_samples":header_loop,"counted_loop_samples":counted,"consistent":u64::from(header_loop) == counted})
            }
            (Some(start), Some(Err(e))) => {
                json!({"offset":start,"error":{"offset":e.offset,"message":e.message},"header_loop_samples":header_loop})
            }
            _ => json!(null),
        }
    }

    pub fn summary(&self) -> Value {
        let header_total = self.u32_field("total_samples");
        let clocks: Vec<Value> = FIELDS
            .iter()
            .filter(|f| f.2.ends_with("_clock"))
            .filter_map(|&(o, s, name, v)| {
                let value = self.field(o, s, v).map(le).unwrap_or(0);
                (value != 0).then(|| json!({"chip":name.trim_end_matches("_clock"),"clock_hz":value & 0x3fff_ffff,"raw":value}))
            })
            .collect();
        let mut summary = json!({"version":self.version_text(),"bytes":self.bytes.len(),"data_start":self.data_start,"stream_end":self.stream_end,
            "commands":self.commands.len(),"chips":clocks,"psg":self.psg_header(),
            "ay8910":self.ay_config().map(|r| match r { Ok((_, _, _, header)) => header, Err(problem) => json!({"render_refusal":problem}) }),
            "gameboy_dmg":self.gb_config().map(|(_, _, _, header)| header),
            "nes_apu":self.nes_config().map(|(.., header)| header),
            "header_total_samples":header_total,"counted_total_samples":self.total_samples(),
            "total_consistent":u64::from(header_total) == self.total_samples(),
            "duration_seconds":self.total_samples() as f64 / f64::from(VGM_RATE),"loop":self.loop_json(),
            "rate":self.u32_field("rate"),"layout":self.layout(),"kinds":self.kinds(),"gd3":self.gd3_json(),"warnings":self.warnings});
        // Chips added later appear only when declared, so earlier summaries
        // keep their exact content.
        if let Some((.., header)) = self.pce_config() {
            summary["huc6280"] = header;
        }
        if let Some((.., header)) = self.ws_config() {
            summary["wonderswan"] = header;
        }
        summary
    }
}

pub fn ay_type_name(chip_type: u8) -> &'static str {
    match chip_type {
        0x00 => "AY8910",
        0x01 => "AY8912",
        0x02 => "AY8913",
        0x03 => "AY8930",
        0x10 => "YM2149",
        0x11 => "YM3439",
        0x12 => "YMZ284",
        0x13 => "YMZ294",
        _ => "undefined",
    }
}

fn data_block_size(kind: u8, field: u32) -> usize {
    (if kind >= 0x80 {
        field & 0x7fff_ffff
    } else {
        field
    }) as usize
}

fn data_block_class(kind: u8) -> &'static str {
    match kind {
        0x00..=0x3f => "uncompressed stream",
        0x40..=0x7e => "compressed stream",
        0x7f => "decompression table",
        0x80..=0xbf => "ROM/RAM image dump",
        0xc0..=0xdf => "RAM write (64 KB or less)",
        _ => "RAM write (above 64 KB)",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A synthetic VGM: header of `header_len` bytes (at least 0x40), the
    /// given version and PSG clock, then `body`, optionally followed by a GD3 tag.
    pub(crate) fn synthetic(version: u32, header_len: usize, body: &[u8], gd3: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; header_len];
        bytes[..4].copy_from_slice(b"Vgm ");
        bytes[8..12].copy_from_slice(&version.to_le_bytes());
        bytes[0x0c..0x10].copy_from_slice(&3_579_545u32.to_le_bytes());
        if version >= 0x150 {
            bytes[0x34..0x38].copy_from_slice(&((header_len - 0x34) as u32).to_le_bytes());
        }
        if version >= 0x110 {
            bytes[0x28..0x2a].copy_from_slice(&9u16.to_le_bytes());
            bytes[0x2a] = 16;
        }
        bytes.extend(body);
        if gd3 {
            let at = bytes.len();
            bytes[0x14..0x18].copy_from_slice(&((at - 0x14) as u32).to_le_bytes());
            let mut text = Vec::new();
            for s in [
                "Test",
                "",
                "Game",
                "",
                "Sega Game Gear",
                "",
                "Me",
                "",
                "2026",
                "",
                "",
            ] {
                for u in s.encode_utf16().chain([0]) {
                    text.extend(u.to_le_bytes());
                }
            }
            bytes.extend(b"Gd3 ");
            bytes.extend(0x100u32.to_le_bytes());
            bytes.extend((text.len() as u32).to_le_bytes());
            bytes.extend(text);
        }
        let len = bytes.len() as u32 - 4;
        bytes[4..8].copy_from_slice(&len.to_le_bytes());
        bytes
    }

    fn body() -> Vec<u8> {
        vec![
            0x50, 0x8e, 0x50, 0x0f, 0x50, 0x90, // tone 0 = 0xfe, full level
            0x61, 0x10, 0x27, // wait 10000
            0x62, 0x63, 0x7f, // 735 + 882 + 16
            0x52, 0x28, 0xf0, // YM2612 key-on
            0x4f, 0xf0, // GG stereo left only
            0x50, 0x9f, 0x66,
        ]
    }

    #[test]
    fn headers_parse_for_each_layout_version() {
        for (version, header_len) in [(0x101, 0x40), (0x110, 0x40), (0x150, 0x40), (0x171, 0x100)] {
            let bytes = synthetic(version, header_len, &body(), true);
            let vgm = Vgm::parse(&bytes).unwrap();
            assert_eq!(vgm.data_start, header_len);
            assert_eq!(vgm.commands.len(), 11, "{version:x}");
            assert_eq!(vgm.total_samples(), 10000 + 735 + 882 + 16);
            assert_eq!(vgm.psg_header()["clock_hz"], 3_579_545);
            let rows = vgm.header_rows();
            let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap().clone();
            assert_eq!(row("rate")["applies"], version >= 0x101);
            assert_eq!(row("sn76489_feedback")["applies"], version >= 0x110);
            assert_eq!(row("sn76489_flags")["applies"], version >= 0x151);
            assert_eq!(row("vgm_data_offset")["applies"], version >= 0x150);
            assert_eq!(row("ga20_clock")["applies"], version >= 0x171);
            let gd3 = vgm.gd3_json();
            assert_eq!(gd3["strings"]["system_name"], "Sega Game Gear");
            assert_eq!(gd3["string_count"], 11);
            // The layout covers every byte exactly once.
            let layout = vgm.layout();
            assert_eq!(layout[0]["start"], 0);
            for pair in layout.windows(2) {
                assert_eq!(pair[0]["end"], pair[1]["start"]);
            }
            assert_eq!(layout.last().unwrap()["end"], bytes.len());
            let (config, _) = vgm.psg_config().unwrap();
            assert_eq!(config, PsgVariant::Sega.config());
        }
    }

    #[test]
    fn pre_160_files_read_0x4x_commands_with_one_operand() {
        let body = [0x41, 0xaa, 0x66];
        assert_eq!(
            Vgm::parse(&synthetic(0x151, 0x80, &body, false))
                .unwrap()
                .commands[0]
                .length,
            2
        );
        // From 1.60 the same opcode has two operands, swallowing the 0x66.
        let error = Vgm::parse(&synthetic(0x160, 0x80, &body, false)).unwrap_err();
        assert!(error.message.contains("no end command"), "{error:?}");
    }

    #[test]
    fn flags_and_header_fields_set_the_psg_configuration() {
        let mut bytes = synthetic(0x151, 0x80, &[0x66], false);
        bytes[0x28] = 3;
        bytes[0x2a] = 15;
        bytes[0x2b] = 0x01 | 0x02 | 0x04;
        let vgm = Vgm::parse(&bytes).unwrap();
        let (config, authority) = vgm.psg_config().unwrap();
        assert_eq!((config.lfsr_width, config.taps), (15, 3));
        assert!(config.zero_period_is_1024 && config.negate_output && !config.stereo_port);
        assert_eq!(authority, "header flags");
        bytes[0x2b] = 0x08;
        assert!(
            Vgm::parse(&bytes)
                .unwrap()
                .psg_config()
                .unwrap_err()
                .contains("divider")
        );
        // A 1.10 file with discrete feedback matches the discrete preset.
        let mut bytes = synthetic(0x110, 0x40, &[0x66], false);
        bytes[0x28] = 3;
        bytes[0x2a] = 15;
        let (config, _) = Vgm::parse(&bytes).unwrap().psg_config().unwrap();
        assert_eq!(config, PsgVariant::Sn76489.config());
    }

    #[test]
    fn malformed_streams_refuse_with_offsets() {
        let truncated = synthetic(0x150, 0x40, &[0x50, 0x9f, 0x61, 0x10], false);
        let error = Vgm::parse(&truncated).unwrap_err();
        assert_eq!(error.offset, 0x42);
        assert!(error.message.contains("needs 3 bytes"));
        let undefined = synthetic(0x150, 0x40, &[0x50, 0x9f, 0x2a, 0x66], false);
        assert_eq!(Vgm::parse(&undefined).unwrap_err().offset, 0x42);
        let mut block = synthetic(
            0x150,
            0x40,
            &[0x67, 0x66, 0x00, 0xff, 0, 0, 0, 0xab, 0x66],
            false,
        );
        assert_eq!(Vgm::parse(&block).unwrap_err().offset, 0x43);
        block[0x43] = 1;
        let vgm = Vgm::parse(&block).unwrap();
        assert_eq!(vgm.commands[0].length, 8);
        assert_eq!(
            vgm.command_json(0, None)["meaning"]["data_range"],
            json!([0x47, 0x48])
        );
        let mut bad_ident = synthetic(0x150, 0x40, &[0x66], false);
        bad_ident[0] = b'X';
        assert_eq!(Vgm::parse(&bad_ident).unwrap_err().offset, 0);
        assert!(
            Vgm::parse(&[0x1f, 0x8b, 8, 0])
                .unwrap_err()
                .message
                .contains("gzip")
        );
        let mut bad_data = synthetic(0x150, 0x40, &[0x66], false);
        bad_data[0x34] = 0xf0;
        assert_eq!(Vgm::parse(&bad_data).unwrap_err().offset, 0x34);
    }

    #[test]
    fn loops_must_point_at_a_command() {
        let mut bytes = synthetic(0x150, 0x40, &body(), false);
        // Command 3 (the 0x61 wait) starts at 0x46.
        bytes[0x1c..0x20].copy_from_slice(&(0x46u32 - 0x1c).to_le_bytes());
        bytes[0x20..0x24].copy_from_slice(&(10000u32 + 735 + 882 + 16).to_le_bytes());
        let vgm = Vgm::parse(&bytes).unwrap();
        assert_eq!(vgm.loop_json()["command_index"], 3);
        assert_eq!(vgm.loop_json()["consistent"], true);
        bytes[0x1c] += 1;
        let vgm = Vgm::parse(&bytes).unwrap();
        assert!(vgm.loop_index.unwrap().is_err());
    }

    #[test]
    fn kinds_list_every_class_including_other_chips() {
        let body = [0x52, 0x28, 0xf0, 0x30, 0x9f, 0x85, 0xe5, 1, 2, 3, 4, 0x66];
        let bytes = synthetic(0x171, 0x100, &body, false);
        let vgm = Vgm::parse(&bytes).unwrap();
        let kinds = vgm.kinds();
        let names: Vec<&str> = kinds.iter().map(|k| k["kind"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "end",
                "second_psg_write",
                "ym2612_dac_write_and_wait",
                "other_chip_write",
                "reserved"
            ]
        );
        assert_eq!(kinds[3]["chips"], json!(["YM2612 port 0"]));
        assert_eq!(vgm.total_samples(), 5);
    }

    #[test]
    fn ay_and_game_boy_headers_and_commands_are_interpreted() {
        let body = [
            0xa0, 0x88, 0x0f, 0xb3, 0x12, 0x80, 0x31, 0xc6, 0xa1, 0x00, 0x00, 0x66,
        ];
        let mut bytes = synthetic(0x171, 0x100, &body, false);
        bytes[0x0c..0x10].fill(0);
        bytes[0x74..0x78].copy_from_slice(&(1_789_773u32 | 0x4000_0000).to_le_bytes());
        bytes[0x78] = 0x10;
        bytes[0x79] = 0x90;
        bytes[0x80..0x84].copy_from_slice(&4_194_304u32.to_le_bytes());
        let vgm = Vgm::parse(&bytes).unwrap();
        let kinds: Vec<Kind> = vgm.commands.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::AyWrite,
                Kind::GbWrite,
                Kind::Ay8910StereoMask,
                Kind::ChipWrite,
                Kind::End
            ]
        );
        let ay = vgm.command_json(0, None)["meaning"].clone();
        assert_eq!(
            (ay["chip"].clone(), ay["second_chip"].clone()),
            (json!("AY8910"), json!(true))
        );
        let mask = vgm.command_json(2, None)["meaning"].clone();
        assert_eq!(mask["chip"], "YM2203 SSG");
        assert_eq!(
            (mask["a"].clone(), mask["b"].clone(), mask["c"].clone()),
            (json!("right"), json!("left"), json!("off"))
        );
        assert_eq!(vgm.command_json(3, None)["meaning"]["chip"], "YM2413");
        let (config, clock, dual, header) = vgm.ay_config().unwrap().unwrap();
        assert_eq!(
            (config.variant, config.clock_divider, clock, dual),
            (AyVariant::Ym2149, 16, 1_789_773, true)
        );
        assert_eq!(config.pan, [Pan::Left, Pan::Center, Pan::Right]);
        assert_eq!(header["chip_type_name"], "YM2149");
        let rows = vgm.header_rows();
        let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap().clone();
        assert_eq!(
            row("ay8910_flags")["meaning"]["ym2149_pin_26_clock_halved"],
            true
        );
        assert_eq!(row("ay8910_clock")["meaning"]["dual_chip"], true);
        assert_eq!(row("gameboy_dmg_clock")["meaning"]["clock_hz"], 4_194_304);
        assert_eq!(vgm.gb_config().unwrap().1, 4_194_304);
        assert!(vgm.psg_config().is_err());
        // An AY8930, an undefined type and pin-26 halving on a GI part refuse rendering.
        for (chip_type, flags, text) in [
            (0x03, 0, "AY8930"),
            (0x07, 0, "not defined"),
            (0x00, 0x10, "pin 26"),
        ] {
            bytes[0x78] = chip_type;
            bytes[0x79] = flags;
            let problem = Vgm::parse(&bytes)
                .unwrap()
                .ay_config()
                .unwrap()
                .unwrap_err();
            assert!(problem.contains(text), "{problem}");
        }
        // Before 1.61 the Game Boy clock field does not apply.
        bytes[8..12].copy_from_slice(&0x160u32.to_le_bytes());
        assert!(Vgm::parse(&bytes).unwrap().gb_config().is_none());
    }

    #[test]
    fn nes_header_commands_and_ram_writes_are_interpreted() {
        let body = [
            0x67, 0x66, 0xc2, 4, 0, 0, 0, 0x40, 0xc0, 0xaa, 0x55, // RAM write at $C040
            0xb4, 0x15, 0x0f, 0xb4, 0x83, 0x08, 0xb4, 0x20, 0x80, 0x66,
        ];
        let mut bytes = synthetic(0x171, 0x100, &body, false);
        bytes[0x0c..0x10].fill(0);
        bytes[0x84..0x88].copy_from_slice(&(1_662_607u32 | 0xc000_0000).to_le_bytes());
        let vgm = Vgm::parse(&bytes).unwrap();
        let kinds: Vec<Kind> = vgm.commands.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::DataBlock,
                Kind::NesWrite,
                Kind::NesWrite,
                Kind::NesWrite,
                Kind::End
            ]
        );
        let (config, clock, dual, fds, header) = vgm.nes_config().unwrap();
        assert_eq!(
            (config.region, clock, dual, fds),
            (crate::nes_apu::NesRegion::Pal, 1_662_607, true, true)
        );
        assert_eq!(header["part"], "RP2A07");
        let block = vgm.command_json(0, None);
        assert_eq!(block["meaning"]["nes_apu_ram_write"]["start"], "$C040");
        assert_eq!(block["meaning"]["nes_apu_ram_write"]["bytes"], 2);
        assert_eq!(block["meaning"]["nes_apu_ram_write"]["end"], "$C041");
        assert!(block["render"].as_str().unwrap().contains("DMC memory"));
        assert_eq!(vgm.data_block(0), Some((0xc2, false, 0x107..0x10b)));
        assert_eq!(vgm.command_json(2, None)["meaning"]["second_chip"], true);
        let fds_write = vgm.command_json(3, None)["meaning"].clone();
        assert_eq!(
            (fds_write["address"].clone(), fds_write["expansion"].clone()),
            (json!("$4080"), json!("FDS"))
        );
        let rows = vgm.header_rows();
        let row = rows.iter().find(|r| r["name"] == "nes_apu_clock").unwrap();
        assert_eq!(row["meaning"]["fds"], true);
        assert_eq!(vgm.summary()["nes_apu"]["region"], "pal");
        // An NTSC clock selects the 2A03 tables.
        bytes[0x84..0x88].copy_from_slice(&1_789_772u32.to_le_bytes());
        let ntsc = Vgm::parse(&bytes).unwrap().nes_config().unwrap().0;
        assert_eq!(ntsc.region, crate::nes_apu::NesRegion::Ntsc);
    }

    #[test]
    fn huc6280_header_and_commands_are_interpreted() {
        let body = [
            0xb9, 0x00, 0x02, 0xb9, 0x84, 0x9f, 0x67, 0x66, 0x05, 1, 0, 0, 0, 0x10, 0x66,
        ];
        let mut bytes = synthetic(0x171, 0x100, &body, false);
        bytes[0x0c..0x10].fill(0);
        // Without the clock the summary has no huc6280 entry.
        assert!(
            Vgm::parse(&bytes)
                .unwrap()
                .summary()
                .get("huc6280")
                .is_none()
        );
        bytes[0xa4..0xa8].copy_from_slice(&(3_579_545u32 | 0xc000_0000).to_le_bytes());
        let vgm = Vgm::parse(&bytes).unwrap();
        let kinds: Vec<Kind> = vgm.commands.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [Kind::PceWrite, Kind::PceWrite, Kind::DataBlock, Kind::End]
        );
        let (config, clock, dual, header) = vgm.pce_config().unwrap();
        assert_eq!((config.clock_hz, clock, dual), (3_579_545, 3_579_545, true));
        assert!(header["bit_31"].is_string());
        let second = vgm.command_json(1, None);
        assert_eq!(second["meaning"]["second_chip"], true);
        assert_eq!(second["meaning"]["chip"], "HuC6280");
        assert_eq!(second["kind"], "huc6280_write");
        assert_eq!(vgm.command_json(2, None)["meaning"]["chip"], "HuC6280");
        let rows = vgm.header_rows();
        let row = rows.iter().find(|r| r["name"] == "huc6280_clock").unwrap();
        assert_eq!(row["meaning"]["dual_chip"], true);
        assert_eq!(vgm.summary()["huc6280"]["clock_hz"], 3_579_545);
    }

    #[test]
    fn wonderswan_header_and_commands_are_interpreted() {
        let body = [
            0xbc, 0x10, 0x01, 0xbc, 0x8f, 0x02, 0xc6, 0x01, 0x23, 0x45, 0x66,
        ];
        let mut bytes = synthetic(0x171, 0x100, &body, false);
        bytes[0x0c..0x10].fill(0);
        assert!(
            Vgm::parse(&bytes)
                .unwrap()
                .summary()
                .get("wonderswan")
                .is_none()
        );
        bytes[0xc0..0xc4].copy_from_slice(&(3_072_000u32 | 0x4000_0000).to_le_bytes());
        let vgm = Vgm::parse(&bytes).unwrap();
        let kinds: Vec<Kind> = vgm.commands.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [Kind::WsWrite, Kind::WsWrite, Kind::WsMemoryWrite, Kind::End]
        );
        let (config, clock, dual, _) = vgm.ws_config().unwrap();
        assert_eq!((config.clock_hz, clock, dual), (3_072_000, 3_072_000, true));
        assert_eq!(vgm.command_json(1, None)["meaning"]["second_chip"], true);
        let memory = vgm.command_json(2, None);
        assert_eq!(memory["kind"], "wonderswan_memory_write");
        assert_eq!(memory["meaning"]["offset"], 0x0123);
        assert_eq!(memory["meaning"]["data"], 0x45);
        let rows = vgm.header_rows();
        let row = rows
            .iter()
            .find(|r| r["name"] == "wonderswan_clock")
            .unwrap();
        assert_eq!(row["meaning"]["dual_chip"], true);
        assert_eq!(vgm.summary()["wonderswan"]["clock_hz"], 3_072_000);
    }
}
