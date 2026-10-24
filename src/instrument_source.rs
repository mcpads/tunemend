//! Exact module ranges in a source container and structural Compile LoROM discovery.
//! Catalog discovery does not interpret songs or establish runtime compatibility.
use crate::store::sha256_hex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceBlock {
    pub address: u16,
    /// Absolute byte offset in the original container, including any copier header.
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceModule {
    pub offset: usize,
    /// Includes every address/length field and the final 0xffff terminator.
    pub bytes: usize,
    pub sha256: String,
    pub blocks: Vec<SourceBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogModule {
    pub index: usize,
    pub pointer: u32,
    pub module: SourceModule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModuleCatalog {
    pub source_sha256: String,
    pub source_bytes: usize,
    pub copier_header_bytes: usize,
    pub pointer_table_offset: usize,
    pub driver_signature_offset: usize,
    pub engine_header: u16,
    pub modules: Vec<CatalogModule>,
    pub banks: Vec<BankCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BankSelection {
    pub driver: usize,
    pub resident: usize,
    pub template: usize,
    pub bank: usize,
    pub sample_modules: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeTrack {
    pub index: usize,
    pub header_address: u16,
    pub header_offset: usize,
    pub channel: u8,
    pub flags: u8,
    pub volume: u8,
    pub vol_env: u8,
    pub vibrato: u8,
    pub transpose: i8,
    pub tempo: u8,
    pub branch: u8,
    pub score_pointer: u16,
    pub srcn: u8,
    pub adsr: u8,
    pub pan: i8,
    pub pan_high_byte: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BankHints {
    pub directory: u16,
    pub tuning: u16,
    pub slots: u16,
    pub envelopes: u16,
    /// Lower bound established by native track references, not full table length.
    pub observed_envelope_count: u16,
    pub sample_memory_end: u32,
    pub song_header: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BankCandidate {
    /// Absolute offsets in the exact original source; selection is explicit.
    pub selection: BankSelection,
    pub module_indices: BankSelection,
    pub hints: BankHints,
    pub native_tracks: Vec<NativeTrack>,
    /// Discovery does not infer consumer ownership or guarantee game requests.
    pub scope: String,
}

fn word(bytes: &[u8], at: usize) -> Result<u16, String> {
    let pair = bytes
        .get(at..at.checked_add(2).ok_or("offset overflow")?)
        .ok_or("truncated module field")?;
    Ok(u16::from_le_bytes([pair[0], pair[1]]))
}

/// Read a module at an explicitly selected container offset. Empty modules and
/// aliases are valid inventory entries. No surrounding bytes are discarded.
pub fn module_at(bytes: &[u8], offset: usize) -> Result<SourceModule, String> {
    let mut cursor = offset;
    let mut blocks: Vec<SourceBlock> = Vec::new();
    loop {
        let address = word(bytes, cursor)?;
        cursor += 2;
        if address == 0xffff {
            return Ok(SourceModule {
                offset,
                bytes: cursor - offset,
                sha256: sha256_hex(&bytes[offset..cursor]),
                blocks,
            });
        }
        let length = usize::from(word(bytes, cursor)?);
        cursor += 2;
        if length == 0 || usize::from(address) + length > 65536 {
            return Err("module block is empty or outside ARAM".into());
        }
        let end = cursor.checked_add(length).ok_or("module range overflow")?;
        if end > bytes.len() {
            return Err("truncated module payload".into());
        }
        if blocks.iter().any(|b| {
            usize::from(address) < usize::from(b.address) + b.length
                && usize::from(b.address) < usize::from(address) + length
        }) {
            return Err("module blocks overlap in ARAM".into());
        }
        blocks.push(SourceBlock {
            address,
            offset: cursor,
            length,
        });
        cursor = end;
    }
}

fn lorom_offset(pointer: u32, copier_header: usize) -> Result<usize, String> {
    let bank = (pointer >> 16) as u8;
    if pointer & 0xffff < 0x8000 || bank == 0x7e || bank == 0x7f {
        return Err("pointer is not in LoROM cartridge space".into());
    }
    Ok(usize::from(bank & 0x7f) * 0x8000 + (pointer as usize & 0x7fff) + copier_header)
}

/// Read an explicit pointer table. The original pointer values and repeated
/// module ranges remain separate entries. No filename or game identity is used.
pub fn catalog_at(
    bytes: &[u8],
    pointer_table_offset: usize,
    count: usize,
    copier_header_bytes: usize,
    driver_signature_offset: usize,
    engine_header: u16,
) -> Result<ModuleCatalog, String> {
    if count == 0 || count > 256 || ![0, 512].contains(&copier_header_bytes) {
        return Err("invalid module count or copier-header size".into());
    }
    let end = pointer_table_offset
        .checked_add(count * 3)
        .ok_or("table range overflow")?;
    let table = bytes
        .get(pointer_table_offset..end)
        .ok_or("truncated module pointer table")?;
    let mut modules = Vec::with_capacity(count);
    for (index, raw) in table.as_chunks::<3>().0.iter().enumerate() {
        let pointer = u32::from_le_bytes([raw[0], raw[1], raw[2], 0]);
        let module = module_at(bytes, lorom_offset(pointer, copier_header_bytes)?)
            .map_err(|reason| format!("module {index}: {reason}"))?;
        if module.offset < end && pointer_table_offset < module.offset + module.bytes {
            return Err("module overlaps its pointer table".into());
        }
        modules.push(CatalogModule {
            index,
            pointer,
            module,
        });
    }
    Ok(ModuleCatalog {
        source_sha256: sha256_hex(bytes),
        source_bytes: bytes.len(),
        copier_header_bytes,
        pointer_table_offset,
        driver_signature_offset,
        engine_header,
        modules,
        banks: Vec::new(),
    })
}

// Loads the song, duration and percussion pointers from a resident engine header.
// Only opcode bytes and the fixed duration-table displacement are constrained.
fn engine_signature(bytes: &[u8]) -> bool {
    const FIXED: &[(usize, u8)] = &[
        (0, 0xe5),
        (3, 0xc4),
        (5, 0xe5),
        (8, 0xc4),
        (10, 0xe5),
        (13, 0xc4),
        (15, 0x60),
        (16, 0x88),
        (17, 0x11),
        (18, 0xc4),
        (20, 0xe5),
        (23, 0xc4),
        (25, 0x88),
        (26, 0),
        (27, 0xc4),
    ];
    if bytes.len() != 29 || !FIXED.iter().all(|&(i, b)| bytes[i] == b) {
        return false;
    }
    // Require the standard pointer-register layout and consecutive header fields.
    bytes[4] == 0x50
        && bytes[9] == 0x51
        && bytes[14] == 0x52
        && bytes[19] == 0x54
        && bytes[24] == 0x53
        && bytes[28] == 0x55
        && word(bytes, 6).ok() == word(bytes, 1).ok().and_then(|a| a.checked_add(1))
        && word(bytes, 11).ok() == word(bytes, 1).ok().and_then(|a| a.checked_add(2))
        && word(bytes, 21).ok() == word(bytes, 1).ok().and_then(|a| a.checked_add(3))
}

/// Discover standard Compile module catalogs in an uncompressed LoROM image.
/// A candidate must have a bounded driver module, a complete contiguous pointer
/// table ending at its first entry's driver module, and a resident module covering the header.
/// Multiple independent catalogs are returned, never silently selected.
pub fn discover_compile_sources(bytes: &[u8]) -> Vec<ModuleCatalog> {
    let copier_header = if bytes.len() % 0x8000 == 512 { 512 } else { 0 };
    let mut catalogs = Vec::new();
    for (signature_offset, candidate) in bytes.windows(29).enumerate() {
        if !engine_signature(candidate)
            || signature_offset
                .checked_sub(4)
                .is_some_and(|at| bytes[at..at + 2] == [0x28, 0x0c])
        {
            continue;
        }
        let engine_header = word(candidate, 1).expect("signature width");
        if engine_header < 0x100 || usize::from(engine_header) + 0x18 > 65536 {
            continue;
        }
        // The standard upload module starts the driver at ARAM 0x0400. This is a
        // format discriminator; no source ROM address or driver hash is assumed.
        for start in signature_offset.saturating_sub(65540)..signature_offset {
            if bytes.get(start..start + 2) != Some(&[0, 4]) {
                continue;
            }
            let Ok(driver) = module_at(bytes, start) else {
                continue;
            };
            if !driver.blocks.iter().any(|block| {
                block.offset <= signature_offset
                    && signature_offset + 29 <= block.offset + block.length
            }) {
                continue;
            }
            let Some(logical_start) = start.checked_sub(copier_header) else {
                continue;
            };
            let bank = logical_start / 0x8000;
            if bank >= 0x80 {
                continue;
            }
            let pointer = ((bank as u32) << 16) | (logical_start as u32 & 0x7fff) | 0x8000;
            for table_offset in start.saturating_sub(256 * 3)..start {
                let distance = start - table_offset;
                if distance < 12 || distance % 3 != 0 {
                    continue;
                }
                let raw = &bytes[table_offset..table_offset + 3];
                let value = u32::from_le_bytes([raw[0], raw[1], raw[2], 0]);
                if value != pointer && value != pointer | 0x800000 {
                    continue;
                }
                let Ok(catalog) = catalog_at(
                    bytes,
                    table_offset,
                    distance / 3,
                    copier_header,
                    signature_offset,
                    engine_header,
                ) else {
                    continue;
                };
                if !catalog.modules.iter().any(|m| {
                    m.module.blocks.iter().any(|b| {
                        usize::from(b.address) <= usize::from(engine_header)
                            && usize::from(engine_header) + 0x18
                                <= usize::from(b.address) + b.length
                    })
                }) {
                    continue;
                }
                catalogs.push(catalog);
                // Later starts are suffixes of this same valid table.
                break;
            }
        }
    }
    catalogs.sort_by_key(|c| c.pointer_table_offset);
    catalogs.dedup_by_key(|c| c.pointer_table_offset);
    for catalog in &mut catalogs {
        catalog.banks = candidate_banks(bytes, catalog);
    }
    catalogs
}

fn module_range<'a>(
    bytes: &'a [u8],
    module: &SourceModule,
    address: u16,
    length: usize,
) -> Option<(usize, &'a [u8])> {
    let address = usize::from(address);
    let block = module.blocks.iter().find(|b| {
        usize::from(b.address) <= address
            && address
                .checked_add(length)
                .is_some_and(|end| end <= usize::from(b.address) + b.length)
    })?;
    let offset = block.offset + address - usize::from(block.address);
    Some((offset, bytes.get(offset..offset + length)?))
}

fn candidate_tracks(bytes: &[u8], module: &SourceModule, header: u16) -> Option<Vec<NativeTrack>> {
    let (_, count) = module_range(bytes, module, header, 1)?;
    let count = usize::from(count[0]);
    if !(1..=8).contains(&count) {
        return None;
    }
    let (offset, raw) = module_range(bytes, module, header, 1 + 14 * count)?;
    let mut tracks = Vec::with_capacity(count);
    for (index, track) in raw[1..].as_chunks::<14>().0.iter().enumerate() {
        let score_pointer = u16::from_le_bytes([track[8], track[9]]);
        // Candidate music tracks must reference the selected template payload.
        module_range(bytes, module, score_pointer, 1)?;
        tracks.push(NativeTrack {
            index,
            header_address: header + 1 + index as u16 * 14,
            header_offset: offset + 1 + index * 14,
            channel: track[0],
            flags: track[1],
            volume: track[2],
            vol_env: track[3],
            vibrato: track[4],
            transpose: track[5] as i8,
            tempo: track[6],
            branch: track[7],
            score_pointer,
            srcn: track[10],
            adsr: track[11],
            pan: track[12] as i8,
            pan_high_byte: track[13],
        });
    }
    Some(tracks)
}

fn candidate_banks(bytes: &[u8], catalog: &ModuleCatalog) -> Vec<BankCandidate> {
    let mut candidates = Vec::new();
    let Some(driver) = catalog.modules.iter().find(|m| {
        m.module.blocks.iter().any(|b| {
            b.offset <= catalog.driver_signature_offset
                && catalog.driver_signature_offset + 29 <= b.offset + b.length
        })
    }) else {
        return candidates;
    };
    for resident in &catalog.modules {
        let Some((_, header)) = module_range(bytes, &resident.module, catalog.engine_header, 0x18)
        else {
            continue;
        };
        let song_table = u16::from_le_bytes([header[0], header[1]]);
        let Some((_, song_pointer)) = song_table
            .checked_add(2)
            .and_then(|address| module_range(bytes, &resident.module, address, 2))
        else {
            continue;
        };
        let song_header = u16::from_le_bytes([song_pointer[0], song_pointer[1]]);
        let tuning = u16::from_le_bytes([header[0x12], header[0x13]]);
        let envelopes = u16::from_le_bytes([header[0xc], header[0xd]]);
        let directory = u16::from(header[0xe]) << 8;
        let Some(tuning_bytes) = song_header.checked_sub(tuning) else {
            continue;
        };
        if tuning_bytes == 0 || tuning_bytes % 2 != 0 || tuning_bytes > 512 {
            continue;
        }
        let slots = tuning_bytes / 2;
        let templates: Vec<_> = catalog
            .modules
            .iter()
            .filter_map(|m| {
                if m.index == resident.index {
                    return None;
                }
                module_range(bytes, &m.module, tuning, usize::from(tuning_bytes))?;
                Some((m, candidate_tracks(bytes, &m.module, song_header)?))
            })
            .collect();
        let Some(first_template) = templates.iter().map(|(m, _)| m.index).min() else {
            continue;
        };
        // Only leading, separate sample modules are proposed as shared samples.
        // They are explicit candidates, not inferred ownership/reservations.
        let mut shared: Vec<&CatalogModule> = Vec::new();
        for m in &catalog.modules[..first_template] {
            if m.module.blocks.is_empty() || !m.module.blocks.iter().all(|b| b.address > directory)
            {
                continue;
            }
            if !shared
                .iter()
                .any(|other| other.module.offset == m.module.offset)
            {
                shared.push(m);
            }
        }
        let Some(sample_memory_end) = shared
            .iter()
            .flat_map(|m| m.module.blocks.iter())
            .map(|b| u32::from(b.address))
            .min()
        else {
            continue;
        };
        for (template, native_tracks) in templates {
            let Some(bank) = catalog.modules.get(template.index + 1) else {
                continue;
            };
            if bank.module.blocks.len() != 1 || bank.module.blocks[0].address != directory {
                continue;
            }
            let bank_block = &bank.module.blocks[0];
            if bank_block.length < usize::from(slots) * 4
                || u32::from(directory) + bank_block.length as u32 > sample_memory_end
            {
                continue;
            }
            let observed_envelope_count = native_tracks
                .iter()
                .map(|t| u16::from(t.adsr) + 1)
                .max()
                .unwrap_or(1);
            if module_range(
                bytes,
                &resident.module,
                envelopes,
                usize::from(observed_envelope_count) * 2,
            )
            .is_none()
            {
                continue;
            }
            let selection = BankSelection {
                driver: driver.module.offset,
                resident: resident.module.offset,
                template: template.module.offset,
                bank: bank.module.offset,
                sample_modules: shared.iter().map(|m| m.module.offset).collect(),
            };
            let module_indices = BankSelection {
                driver: driver.index,
                resident: resident.index,
                template: template.index,
                bank: bank.index,
                sample_modules: shared.iter().map(|m| m.index).collect(),
            };
            candidates.push(BankCandidate { selection,module_indices,
                hints:BankHints{directory,tuning,slots,envelopes,observed_envelope_count,sample_memory_end,song_header},
                native_tracks,
                scope:"Candidate adjacent score/bank pair. Tuning extent ends at the native song header; memory limit comes from leading resident sample modules. Envelope count is an observed lower bound. Consumer reservations and actual game request selection are not inferred.".into()});
        }
    }
    candidates.sort_by_key(|c| (c.selection.resident, c.selection.template, c.selection.bank));
    candidates.dedup_by_key(|c| (c.selection.resident, c.selection.template, c.selection.bank));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(copier: usize) -> Vec<u8> {
        let mut bytes = vec![0; copier + 0x8000];
        let table = copier + 0x100;
        let driver = table + 12;
        let resident = driver + 4 + 64 + 2;
        let empty = resident + 4 + 32 + 2;
        for (index, offset) in [driver, resident, empty, driver].into_iter().enumerate() {
            let pointer = (0x8000 + offset - copier) as u32;
            bytes[table + index * 3..table + index * 3 + 3]
                .copy_from_slice(&pointer.to_le_bytes()[..3]);
        }
        bytes[driver..driver + 4].copy_from_slice(&[0, 4, 64, 0]);
        let signature = [
            0xe5, 0, 0x18, 0xc4, 0x50, 0xe5, 1, 0x18, 0xc4, 0x51, 0xe5, 2, 0x18, 0xc4, 0x52, 0x60,
            0x88, 0x11, 0xc4, 0x54, 0xe5, 3, 0x18, 0xc4, 0x53, 0x88, 0, 0xc4, 0x55,
        ];
        bytes[driver + 12..driver + 41].copy_from_slice(&signature);
        bytes[driver + 68..driver + 70].copy_from_slice(&[255, 255]);
        bytes[resident..resident + 4].copy_from_slice(&[0, 0x18, 32, 0]);
        bytes[resident + 36..resident + 38].copy_from_slice(&[255, 255]);
        bytes[empty..empty + 2].copy_from_slice(&[255, 255]);
        bytes
    }

    #[test]
    fn structural_catalog_retains_aliases_empty_modules_and_header_offsets() {
        for copier in [0, 512] {
            let source = container(copier);
            let found = discover_compile_sources(&source);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].copier_header_bytes, copier);
            assert_eq!(found[0].modules.len(), 4);
            assert_eq!(found[0].modules[0].module, found[0].modules[3].module);
            assert!(found[0].modules[2].module.blocks.is_empty());
            assert_eq!(found[0].modules[2].module.bytes, 2);
            // Other modules may live earlier in the ROM than the pointer table.
            let mut scattered = source.clone();
            let old = &found[0].modules[1].module;
            scattered[copier + 0x40..copier + 0x40 + old.bytes]
                .copy_from_slice(&source[old.offset..old.offset + old.bytes]);
            scattered[copier + 0x103..copier + 0x106].copy_from_slice(&[0x40, 0x80, 0]);
            let moved = discover_compile_sources(&scattered);
            assert_eq!(moved.len(), 1);
            assert_eq!(moved[0].modules[1].module.offset, copier + 0x40);
            let mut corrupt = source.clone();
            corrupt[copier + 0x100 + 3..copier + 0x100 + 6].fill(0);
            assert!(discover_compile_sources(&corrupt).is_empty());
        }
    }

    #[test]
    fn bank_candidates_report_native_fields_without_guessing_ownership() {
        let mut source = vec![0; 0x8000];
        let mut driver = vec![0; 64];
        let original = container(0);
        driver[8..37].copy_from_slice(&original[0x118..0x135]);
        let mut resident = vec![0; 64];
        resident[0..2].copy_from_slice(&0x1824_u16.to_le_bytes());
        resident[12..14].copy_from_slice(&0x1820_u16.to_le_bytes());
        resident[14] = 0x40;
        resident[18..20].copy_from_slice(&0x3000_u16.to_le_bytes());
        resident[0x26..0x28].copy_from_slice(&0x3004_u16.to_le_bytes());
        let mut template = vec![0; 40];
        template[4] = 1;
        template[7] = 11;
        template[13..15].copy_from_slice(&0x3014_u16.to_le_bytes());
        let modules = [
            (0x400_u16, driver),
            (0x1800, resident),
            (0x5000, vec![0; 9]),
            (0x3000, template),
            (0x4000, vec![0; 32]),
        ];
        let mut cursor = 0x100 + modules.len() * 3;
        for (index, (address, payload)) in modules.into_iter().enumerate() {
            let pointer = (0x8000 + cursor) as u32;
            source[0x100 + index * 3..0x103 + index * 3]
                .copy_from_slice(&pointer.to_le_bytes()[..3]);
            source[cursor..cursor + 2].copy_from_slice(&address.to_le_bytes());
            source[cursor + 2..cursor + 4].copy_from_slice(&(payload.len() as u16).to_le_bytes());
            source[cursor + 4..cursor + 4 + payload.len()].copy_from_slice(&payload);
            cursor += 4 + payload.len();
            source[cursor..cursor + 2].fill(255);
            cursor += 2;
        }
        let found = discover_compile_sources(&source);
        assert_eq!(found.len(), 1);
        let bank = &found[0].banks[0];
        assert_eq!(bank.module_indices.template, 3);
        assert_eq!(bank.module_indices.bank, 4);
        assert_eq!(bank.module_indices.sample_modules, [2]);
        assert_eq!(bank.hints.slots, 2);
        assert_eq!(bank.hints.directory, 0x4000);
        assert_eq!(bank.hints.sample_memory_end, 0x5000);
        assert_eq!(bank.hints.observed_envelope_count, 1);
        assert_eq!(bank.native_tracks[0].volume, 11);
        assert_eq!(bank.native_tracks[0].score_pointer, 0x3014);
    }

    #[test]
    fn explicit_module_read_refuses_truncation_and_overlapping_blocks() {
        assert!(module_at(&[0, 4, 2, 0, 1], 0).is_err());
        assert!(module_at(&[0, 4, 2, 0, 1, 2, 1, 4, 1, 0, 3, 255, 255], 0).is_err());
        assert!(module_at(&[0, 4, 1, 0, 1, 255], 0).is_err());
        assert!(module_at(&[], usize::MAX).is_err());
    }
}
