//! SoundFont 2 file reading (SF2.04 §3–§7, §10).
//!
//! The reader accepts the RIFF `sfbk` form with its three lists in the fixed
//! order INFO, sdta, pdta and the nine hydra sub-chunks in their fixed order.
//! Structural defects named by the specification — wrong record sizes,
//! non-monotonic or mismatched bag indices, out-of-range instrument or sample
//! indices, sample points outside the sample data, truncated chunks,
//! unknown chunks outside INFO — are refused with the chunk path and byte
//! offset. Zone-level defects the specification says to ignore are ignored.
//! ROM samples are refused because no sound ROM is available.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::modulator::{GENERATORS, Modulator};

/// A located refusal: where in the file reading stopped and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundFontError {
    /// Byte offset in the file.
    pub offset: usize,
    /// Chunk path and record, such as `pdta/pbag record 4`.
    pub location: String,
    pub message: String,
}

impl std::fmt::Display for SoundFontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at byte {}: {}",
            self.location, self.offset, self.message
        )
    }
}

impl std::error::Error for SoundFontError {}

fn refuse<T>(
    offset: usize,
    location: impl Into<String>,
    message: impl Into<String>,
) -> Result<T, SoundFontError> {
    Err(SoundFontError {
        offset,
        location: location.into(),
        message: message.into(),
    })
}

/// One preset or instrument zone after the per-zone rules of §7.3–§7.9.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Zone {
    pub key_range: Option<(u8, u8)>,
    pub velocity_range: Option<(u8, u8)>,
    /// Generator amounts by operator; ranges and the terminal index are
    /// kept in their own fields.
    pub generators: Vec<Option<i16>>,
    pub modulators: Vec<Modulator>,
    /// Instrument index (preset zones) or sample index (instrument zones).
    pub target: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub name: String,
    pub program: u16,
    pub bank: u16,
    pub global: Option<Zone>,
    pub zones: Vec<Zone>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instrument {
    pub name: String,
    pub global: Option<Zone>,
    pub zones: Vec<Zone>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SampleHeader {
    pub name: String,
    pub start: u32,
    pub end: u32,
    pub loop_start: u32,
    pub loop_end: u32,
    pub sample_rate: u32,
    pub original_key: u8,
    pub correction: i8,
    pub kind: u16,
}

/// A parsed SoundFont: sample data normalized to −1…1 and the hydra.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundFont {
    pub version: (u16, u16),
    pub name: String,
    /// Sample data points, 16-bit or 24-bit, as fractions of full scale.
    pub data: Arc<[f32]>,
    pub presets: Vec<Preset>,
    pub instruments: Vec<Instrument>,
    pub samples: Vec<SampleHeader>,
    /// The active preset for each (bank, program): the first in file order.
    by_number: BTreeMap<(u16, u16), usize>,
}

struct Chunk<'a> {
    id: [u8; 4],
    data: &'a [u8],
    /// Offset of the chunk data in the file.
    offset: usize,
}

/// Reads the chunks in `bytes[start..end]`.
fn chunks<'a>(
    file: &'a [u8],
    start: usize,
    end: usize,
    path: &str,
) -> Result<Vec<Chunk<'a>>, SoundFontError> {
    let mut out = Vec::new();
    let mut at = start;
    while at < end {
        if end - at < 8 {
            return refuse(at, path, "a chunk header is truncated");
        }
        let id: [u8; 4] = file[at..at + 4].try_into().unwrap();
        let size = u32::from_le_bytes(file[at + 4..at + 8].try_into().unwrap()) as usize;
        let data_start = at + 8;
        if size > end - data_start {
            return refuse(
                at,
                format!("{path}/{}", fourcc(&id)),
                format!(
                    "the chunk declares {size} bytes but only {} remain",
                    end - data_start
                ),
            );
        }
        out.push(Chunk {
            id,
            data: &file[data_start..data_start + size],
            offset: data_start,
        });
        at = data_start + size + (size & 1);
    }
    Ok(out)
}

fn fourcc(id: &[u8; 4]) -> String {
    id.iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

/// A LIST chunk's form type and contents.
fn list<'a>(
    file: &'a [u8],
    chunk: &Chunk<'a>,
    kind: &[u8; 4],
    path: &str,
) -> Result<Vec<Chunk<'a>>, SoundFontError> {
    if &chunk.id != b"LIST" || chunk.data.len() < 4 || &chunk.data[..4] != kind {
        return refuse(
            chunk.offset.saturating_sub(8),
            path,
            format!("expected the {} list here", fourcc(kind)),
        );
    }
    chunks(
        file,
        chunk.offset + 4,
        chunk.offset + chunk.data.len(),
        &fourcc(kind),
    )
}

fn name(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .map(|&b| {
            if b.is_ascii() && !b.is_ascii_control() {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn dword(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// Splits a hydra sub-chunk into fixed-size records, at least `minimum`.
fn records<'a>(
    chunk: &Chunk<'a>,
    size: usize,
    minimum: usize,
) -> Result<Vec<&'a [u8]>, SoundFontError> {
    let path = format!("pdta/{}", fourcc(&chunk.id));
    if !chunk.data.len().is_multiple_of(size) {
        return refuse(
            chunk.offset,
            path,
            format!("the size {} is not a multiple of {size}", chunk.data.len()),
        );
    }
    if chunk.data.len() / size < minimum {
        return refuse(
            chunk.offset,
            path,
            format!("at least {minimum} records are required"),
        );
    }
    Ok(chunk.data.chunks_exact(size).collect())
}

/// Index ranges of the zones of each header: `index(record)` gives a header's
/// first bag; the terminal header bounds the last.
fn bag_ranges(
    headers: &[&[u8]],
    index: impl Fn(&[u8]) -> u16,
    bags: usize,
    chunk: &Chunk,
    bag_chunk: &str,
) -> Result<Vec<std::ops::Range<usize>>, SoundFontError> {
    let path = format!("pdta/{}", fourcc(&chunk.id));
    let size = chunk.data.len() / headers.len();
    let indices: Vec<usize> = headers.iter().map(|h| index(h) as usize).collect();
    for (i, pair) in indices.windows(2).enumerate() {
        if pair[1] < pair[0] {
            return refuse(
                chunk.offset + (i + 1) * size,
                format!("{path} record {}", i + 1),
                format!(
                    "{bag_chunk} index {} is below the previous {}",
                    pair[1], pair[0]
                ),
            );
        }
    }
    let last = *indices.last().unwrap();
    if last + 1 != bags {
        return refuse(
            chunk.offset + (headers.len() - 1) * size,
            format!("{path} terminal record"),
            format!("{bag_chunk} index {last} does not match its {bags} records"),
        );
    }
    Ok(indices.windows(2).map(|p| p[0]..p[1]).collect())
}

/// Generator and modulator ranges of each bag, checked against their lists.
#[allow(clippy::type_complexity)]
fn bag_lists(
    bags: &[&[u8]],
    chunk: &Chunk,
    generators: usize,
    modulators: usize,
) -> Result<Vec<(std::ops::Range<usize>, std::ops::Range<usize>)>, SoundFontError> {
    let path = format!("pdta/{}", fourcc(&chunk.id));
    for (i, pair) in bags.windows(2).enumerate() {
        if word(pair[1], 0) < word(pair[0], 0) || word(pair[1], 2) < word(pair[0], 2) {
            return refuse(
                chunk.offset + (i + 1) * 4,
                format!("{path} record {}", i + 1),
                "generator or modulator indices decrease",
            );
        }
    }
    let last = bags.last().unwrap();
    if word(last, 0) as usize + 1 != generators || word(last, 2) as usize + 1 != modulators {
        return refuse(
            chunk.offset + (bags.len() - 1) * 4,
            format!("{path} terminal record"),
            format!(
                "indices {} and {} do not match {} generator and {} modulator records",
                word(last, 0),
                word(last, 2),
                generators,
                modulators
            ),
        );
    }
    Ok(bags
        .windows(2)
        .map(|p| {
            (
                word(p[0], 0) as usize..word(p[1], 0) as usize,
                word(p[0], 2) as usize..word(p[1], 2) as usize,
            )
        })
        .collect())
}

/// Applies §7.5/§7.9 to one zone's generator list. `terminal` is 41
/// (instrument) or 53 (sampleID).
fn zone(generators: &[&[u8]], modulators: &[&[u8]], terminal: u16) -> Zone {
    let mut zone = Zone {
        generators: vec![None; GENERATORS],
        ..Zone::default()
    };
    for (i, record) in generators.iter().enumerate() {
        let (operator, raw) = (word(record, 0), word(record, 2));
        let [low, high] = raw.to_le_bytes();
        match operator {
            43 if i == 0 => zone.key_range = Some((low, high)),
            44 if i == 0 || (i == 1 && word(generators[0], 0) == 43) => {
                zone.velocity_range = Some((low, high))
            }
            43 | 44 => {}
            _ if operator == terminal => {
                zone.target = Some(raw as usize);
                break;
            }
            41 | 53 => {}
            operator if (operator as usize) < GENERATORS => {
                // A later duplicate replaces the earlier one.
                zone.generators[operator as usize] = Some(raw as i16);
            }
            _ => {}
        }
    }
    for record in modulators {
        let modulator = Modulator {
            source: word(record, 0),
            destination: word(record, 2),
            amount: word(record, 4) as i16,
            amount_source: word(record, 6),
            transform: word(record, 8),
        };
        if !modulator.is_supported() {
            continue;
        }
        // A second identical modulator in a zone replaces the first.
        zone.modulators.retain(|m| !m.same_as(&modulator));
        zone.modulators.push(modulator);
    }
    zone
}

/// Splits a header's zones into an optional global zone and local zones.
fn split_zones(mut zones: Vec<Zone>) -> (Option<Zone>, Vec<Zone>) {
    let global = if zones.len() > 1 && zones[0].target.is_none() {
        Some(zones.remove(0))
    } else {
        None
    };
    // Other zones without a terminal generator are ignored (§7.3, §7.7).
    zones.retain(|z| z.target.is_some());
    (global, zones)
}

impl SoundFont {
    /// Reads a SoundFont 2 file.
    pub fn parse(file: &[u8]) -> Result<Self, SoundFontError> {
        if file.len() < 12 || &file[..4] != b"RIFF" || &file[8..12] != b"sfbk" {
            return refuse(0, "RIFF", "not a RIFF sfbk file");
        }
        let size = dword(file, 4) as usize;
        if size < 4 || size > file.len() - 8 {
            return refuse(
                4,
                "RIFF",
                format!(
                    "the form declares {size} bytes but the file holds {}",
                    file.len() - 8
                ),
            );
        }
        let top = chunks(file, 12, 8 + size, "RIFF")?;
        if top.len() != 3 {
            return refuse(
                12,
                "RIFF",
                format!(
                    "expected the INFO, sdta and pdta lists, found {} chunks",
                    top.len()
                ),
            );
        }
        let info = list(file, &top[0], b"INFO", "RIFF")?;
        let sdta = list(file, &top[1], b"sdta", "RIFF")?;
        let pdta = list(file, &top[2], b"pdta", "RIFF")?;

        let mut version = None;
        let mut font_name = String::new();
        for chunk in &info {
            match &chunk.id {
                b"ifil" => {
                    if chunk.data.len() != 4 {
                        return refuse(chunk.offset, "INFO/ifil", "the version is not 4 bytes");
                    }
                    version = Some((word(chunk.data, 0), word(chunk.data, 2)));
                }
                b"INAM" => font_name = name(chunk.data),
                // Other INFO sub-chunks, known or not, are ignored (§10.2).
                _ => {}
            }
        }
        let Some(version) = version else {
            return refuse(
                top[0].offset,
                "INFO",
                "the ifil version sub-chunk is missing",
            );
        };
        if version.0 != 2 {
            return refuse(
                top[0].offset,
                "INFO/ifil",
                format!("version {}.{} is not SoundFont 2", version.0, version.1),
            );
        }

        let (mut smpl, mut sm24) = (None, None);
        for chunk in &sdta {
            match &chunk.id {
                b"smpl" if smpl.is_none() => smpl = Some(chunk),
                b"sm24" if sm24.is_none() && smpl.is_some() => sm24 = Some(chunk),
                _ => {
                    return refuse(
                        chunk.offset - 8,
                        format!("sdta/{}", fourcc(&chunk.id)),
                        "unexpected chunk in the sample data list",
                    );
                }
            }
        }
        let points = smpl.map_or(0, |c| c.data.len() / 2);
        // §6.2: sm24 counts only for 2.04 files and the exact matching size.
        let low_bytes = sm24.filter(|c| version.1 >= 4 && c.data.len() == points + points % 2);
        let data: Arc<[f32]> = match smpl {
            None => Arc::from(Vec::new()),
            Some(smpl) => (0..points)
                .map(|i| {
                    let high = i16::from_le_bytes([smpl.data[2 * i], smpl.data[2 * i + 1]]);
                    match low_bytes {
                        Some(low) => {
                            ((i32::from(high) << 8) | i32::from(low.data[i])) as f32 / 8_388_608.0
                        }
                        None => f32::from(high) / 32_768.0,
                    }
                })
                .collect(),
        };

        const HYDRA: [(&[u8; 4], usize); 9] = [
            (b"phdr", 38),
            (b"pbag", 4),
            (b"pmod", 10),
            (b"pgen", 4),
            (b"inst", 22),
            (b"ibag", 4),
            (b"imod", 10),
            (b"igen", 4),
            (b"shdr", 46),
        ];
        if pdta.len() != HYDRA.len() {
            return refuse(
                top[2].offset,
                "pdta",
                format!("expected 9 sub-chunks, found {}", pdta.len()),
            );
        }
        let mut tables = Vec::new();
        for (chunk, (id, size)) in pdta.iter().zip(HYDRA) {
            if &chunk.id != id {
                return refuse(
                    chunk.offset - 8,
                    format!("pdta/{}", fourcc(&chunk.id)),
                    format!("expected the {} sub-chunk here", fourcc(id)),
                );
            }
            let minimum = if matches!(id, b"phdr" | b"inst") {
                2
            } else {
                1
            };
            tables.push(records(chunk, size, minimum)?);
        }
        let [phdr, pbag, pmod, pgen, inst, ibag, imod, igen, shdr]: [Vec<&[u8]>; 9] =
            tables.try_into().unwrap();

        let mut samples = Vec::new();
        for (i, record) in shdr[..shdr.len() - 1].iter().enumerate() {
            let header = SampleHeader {
                name: name(&record[..20]),
                start: dword(record, 20),
                end: dword(record, 24),
                loop_start: dword(record, 28),
                loop_end: dword(record, 32),
                sample_rate: dword(record, 36),
                original_key: record[40],
                correction: record[41] as i8,
                kind: word(record, 44),
            };
            let at = pdta[8].offset + i * 46;
            let location = format!("pdta/shdr record {i}");
            if header.kind & 0x8000 != 0 {
                return refuse(
                    at,
                    location,
                    "ROM samples need a sound ROM, which is not available",
                );
            }
            if header.start > header.end || header.end as usize > points {
                return refuse(
                    at,
                    location,
                    format!(
                        "points {}–{} lie outside the {points} sample data points",
                        header.start, header.end
                    ),
                );
            }
            samples.push(header);
        }

        let ibag_lists = bag_lists(&ibag, &pdta[5], igen.len(), imod.len())?;
        let mut instruments = Vec::new();
        for (i, bags) in bag_ranges(&inst, |r| word(r, 20), ibag.len(), &pdta[4], "ibag")?
            .into_iter()
            .enumerate()
        {
            let mut zones = Vec::new();
            for (generators, modulators) in &ibag_lists[bags] {
                let z = zone(&igen[generators.clone()], &imod[modulators.clone()], 53);
                if let Some(sample) = z.target
                    && sample >= samples.len()
                {
                    return refuse(
                        pdta[7].offset + generators.end.saturating_sub(1) * 4,
                        format!("pdta/igen of instrument {i}"),
                        format!("sample {sample} is past the {} samples", samples.len()),
                    );
                }
                zones.push(z);
            }
            let (global, zones) = split_zones(zones);
            instruments.push(Instrument {
                name: name(&inst[i][..20]),
                global,
                zones,
            });
        }

        let pbag_lists = bag_lists(&pbag, &pdta[1], pgen.len(), pmod.len())?;
        let mut presets = Vec::new();
        let mut by_number = BTreeMap::new();
        for (i, bags) in bag_ranges(&phdr, |r| word(r, 24), pbag.len(), &pdta[0], "pbag")?
            .into_iter()
            .enumerate()
        {
            let mut zones = Vec::new();
            for (generators, modulators) in &pbag_lists[bags] {
                let z = zone(&pgen[generators.clone()], &pmod[modulators.clone()], 41);
                if let Some(instrument) = z.target
                    && instrument >= instruments.len()
                {
                    return refuse(
                        pdta[3].offset + generators.end.saturating_sub(1) * 4,
                        format!("pdta/pgen of preset {i}"),
                        format!(
                            "instrument {instrument} is past the {} instruments",
                            instruments.len()
                        ),
                    );
                }
                zones.push(z);
            }
            let (global, zones) = split_zones(zones);
            let (program, bank) = (word(phdr[i], 20), word(phdr[i], 22));
            // Presets without zones and with invalid numbers are kept but
            // cannot be played (§7.2).
            if !zones.is_empty() && program < 128 && bank <= 128 {
                by_number.entry((bank, program)).or_insert(i);
            }
            presets.push(Preset {
                name: name(&phdr[i][..20]),
                program,
                bank,
                global,
                zones,
            });
        }

        Ok(SoundFont {
            version,
            name: font_name,
            data,
            presets,
            instruments,
            samples,
            by_number,
        })
    }

    /// The playable preset for a bank and program, if the file has one.
    pub fn preset_index(&self, bank: u16, program: u16) -> Option<usize> {
        self.by_number.get(&(bank, program)).copied()
    }

    /// The first playable preset in (bank, program) order.
    pub fn first_preset(&self) -> Option<usize> {
        self.by_number.values().next().copied()
    }

    pub fn presets(&self) -> &[Preset] {
        &self.presets
    }

    pub fn instruments(&self) -> &[Instrument] {
        &self.instruments
    }

    pub fn samples(&self) -> &[SampleHeader] {
        &self.samples
    }
}
