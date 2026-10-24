//! Semantic instrument bindings for reviewed Compile standard driver implementations.
//! Original module bytes remain authoritative; tables are resolved through their pointers.
use crate::instruments::{InstrumentBank, InstrumentSettings, module_blocks};
use crate::store::sha256_hex;
use serde::Serialize;
use serde_json::{Value, json};

const ENGINE_HEADER: usize = 0x1800;
const PITCH_WORDS: usize = 120;

#[derive(Clone, Copy)]
struct Driver {
    default_pitch: usize,
}

fn driver(bank: &InstrumentBank) -> Result<Driver, String> {
    let bytes = bank
        .assets
        .get("runtime-driver.bin")
        .ok_or("missing driver module")?;
    let blocks = module_blocks(bytes)?;
    if blocks.len() != 1 || blocks[0].address != 0x400 {
        return Err("unsupported Compile driver module layout".into());
    }
    let b = &blocks[0];
    let hash = sha256_hex(&bytes[b.offset..b.offset + b.length]);
    let default_pitch = match hash.as_str() {
        "fea5fa004d91732e0215356dc7d5e8460438b1b15d1dc50bfa7daf22841057d5" => 0x1403,
        "aec33e0fe34525cd0a217224c3b3861c8356376ac57ed46dc47dea770f913a8d" => 0x13f3,
        _ => return Err("Compile driver implementation is not reviewed; supply a supported driver or add a verified interpretation before transferring opaque settings".into()),
    };
    if bank.layout.directory as usize != byte(bank, ENGINE_HEADER + 14)? as usize * 256
        || bank.layout.tuning as usize != word(bank, ENGINE_HEADER + 18)?
        || bank.layout.envelopes as usize != word(bank, ENGINE_HEADER + 12)?
    {
        return Err("declared bank layout disagrees with the Compile engine header".into());
    }
    Ok(Driver { default_pitch })
}

pub fn is_supported(bank: &InstrumentBank) -> bool {
    driver(bank).is_ok()
}
fn byte(bank: &InstrumentBank, at: usize) -> Result<u8, String> {
    Ok(bank.read(at, 1)?[0])
}
fn word(bank: &InstrumentBank, at: usize) -> Result<usize, String> {
    let raw = bank.read(at, 2)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]) as usize)
}
fn table(bank: &InstrumentBank, offset: usize) -> Result<usize, String> {
    word(bank, ENGINE_HEADER + offset)
}
fn pitch_pointer(bank: &InstrumentBank, contract: Driver, index: u8) -> Result<usize, String> {
    if index >= 128 {
        return Err(
            "pitch-table index aliases an earlier entry; explicit interpretation is required"
                .into(),
        );
    }
    if index == 0 {
        Ok(contract.default_pitch)
    } else {
        word(bank, table(bank, 22)? + index as usize * 2)
    }
}
fn pitch(bank: &InstrumentBank, contract: Driver, index: u8) -> Result<Vec<u8>, String> {
    let pointer = pitch_pointer(bank, contract, index)?;
    // The driver accepts indices 1..=120, including the final adjacent word.
    bank.read(pointer + 2, PITCH_WORDS * 2)
}

/// Read a bounded duration/value program without interpreting its audible effect.
/// Stop, backward-loop and restart commands close its reachable byte range.
fn script(bank: &InstrumentBank, pointer: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut boundaries = Vec::new();
    while bytes.len() < 256 {
        let at = bytes.len();
        boundaries.push(at);
        let command = byte(bank, pointer + at)?;
        bytes.push(command);
        match command {
            0..=0x7f => {
                if bytes.len() == 256 {
                    return Err("envelope program wraps its byte index".into());
                }
                bytes.push(byte(bank, pointer + bytes.len())?);
            }
            0x80 => return Ok(bytes),
            // These reviewed drivers compare the cursor, not the command, after 0x81.
            0x82..=0xff if at < 0x83 => return Ok(bytes),
            0x81 => {
                if bytes.len() == 256 {
                    return Err("envelope loop operand wraps its byte index".into());
                }
                let distance = byte(bank, pointer + bytes.len())? as usize;
                bytes.push(distance as u8);
                if distance == 0 || distance > at || !boundaries.contains(&(at - distance)) {
                    return Err("envelope loop does not target an earlier duration boundary".into());
                }
                return Ok(bytes);
            }
            _ => return Err("conditional envelope programs need an explicit interpretation; no settings were changed".into()),
        }
    }
    Err("envelope program has no bounded stop or loop".into())
}
fn indexed_script(
    bank: &InstrumentBank,
    table_offset: usize,
    index: u8,
) -> Result<Vec<u8>, String> {
    if index >= 128 {
        return Err("envelope index aliases an earlier pointer entry".into());
    }
    script(
        bank,
        word(bank, table(bank, table_offset)? + index as usize * 2)?,
    )
}
fn map_script(
    destination: &InstrumentBank,
    donor: &InstrumentBank,
    offset: usize,
    index: u8,
    name: &str,
) -> Result<(u8, Value), String> {
    if index == 0 {
        return Ok((0, json!({"kind":name,"disabled":true})));
    }
    let bytes = indexed_script(donor, offset, index).map_err(|e| format!("{name}: {e}"))?;
    let mapped = (1..128).find(|candidate| indexed_script(destination, offset, *candidate).is_ok_and(|other| other == bytes))
        .ok_or_else(|| format!("destination has no byte-identical {name} program for source index {index}; install that program in an explicitly available table slot before transfer"))?;
    Ok((
        mapped,
        json!({"kind":name,"source_index":index,"destination_index":mapped,"bytes":bytes.len(),"sha256":sha256_hex(&bytes),"exact":true}),
    ))
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeTrack {
    pub index: usize,
    pub header_address: u16,
    pub song_index: u8,
    pub channel: u8,
    pub flags: u8,
    pub volume: u8,
    pub tempo: u8,
    pub branch_id: u8,
    pub score_pointer: u16,
    pub pan: i8,
    pub reserved: u8,
    pub settings: InstrumentSettings,
}

pub fn native_tracks(bank: &InstrumentBank) -> Result<Vec<NativeTrack>, String> {
    driver(bank)?;
    // The loaded song template supplies the engine's ordinary music request (1).
    let address = word(bank, table(bank, 0)? + 2)?;
    let count = byte(bank, address)? as usize;
    if !(1..=16).contains(&count) {
        return Err("loaded template has no bounded native music header at song index 1".into());
    }
    bank.file_offset("template.bin", address, 1 + count * 14)?;
    let mut tracks = Vec::new();
    for index in 0..count {
        let at = address + 1 + index * 14;
        let raw = bank.read(at, 14)?;
        let score_pointer = u16::from_le_bytes([raw[8], raw[9]]);
        if raw[0] >= 16 || raw[1] == 0 {
            return Err("native header contains a disabled or out-of-range channel".into());
        }
        bank.file_offset("template.bin", score_pointer as usize, 1)?;
        tracks.push(NativeTrack {
            index,
            header_address: at as u16,
            song_index: 1,
            channel: raw[0],
            flags: raw[1],
            volume: raw[2],
            tempo: raw[6],
            branch_id: raw[7],
            score_pointer,
            pan: raw[12] as i8,
            reserved: raw[13],
            settings: InstrumentSettings {
                srcn: raw[10],
                adsr: raw[11],
                octave_shift: 0,
                transpose: raw[5] as i8 as i16,
                vol_env: raw[3],
                vibrato: raw[4],
            },
        });
    }
    Ok(tracks)
}

pub fn describe(bank: &InstrumentBank) -> Result<Value, String> {
    let contract = driver(bank)?;
    Ok(
        json!({"format":"compile_standard","engine_header":ENGINE_HEADER,
        "global_transpose":byte(bank,ENGINE_HEADER+15)? as i8,
        "directory":bank.layout.directory,"tuning":table(bank,18)?,
        "pitch_tables":table(bank,22)?,"default_pitch_pointer":contract.default_pitch,
        "pitch_words":PITCH_WORDS,"adsr":table(bank,12)?,
        "volume_envelopes":table(bank,4)?,"vibrato":table(bank,6)?,"gain_envelopes":table(bank,10)?,
        "native_tracks":native_tracks(bank)?,
        "preservation":"exact sample and referenced tables; source musical context remains separate"}),
    )
}

pub struct MappedInstrument {
    pub tuning: Vec<u8>,
    pub settings: InstrumentSettings,
    pub report: Value,
}

pub fn map_settings(
    destination: &InstrumentBank,
    donor: &InstrumentBank,
    settings: &InstrumentSettings,
) -> Result<MappedInstrument, String> {
    if settings.srcn >= 128 {
        return Err("high sample indices alter the reviewed driver's pitch carry; select a source sample below 128 or add an explicit interpretation".into());
    }
    let source_contract = driver(donor)?;
    let destination_contract = driver(destination)?;
    let source = donor.sample(settings.srcn)?;
    let pitch_bytes = pitch(donor, source_contract, source.tuning[1])?;
    let pitch_index = (0..128).find(|index| pitch(destination, destination_contract, *index).is_ok_and(|other| other == pitch_bytes))
        .ok_or_else(|| format!("destination has no exact {}-word pitch table for source index {}; install that table in an explicitly available slot before transfer", PITCH_WORDS, source.tuning[1]))?;
    let mut result = settings.clone();
    let envelope_report = if settings.adsr < 128 {
        let envelope = donor.envelope(settings.adsr)?;
        result.adsr = (0..destination.layout.envelope_count.min(128)).find(|i| destination.envelope(*i as u8).is_ok_and(|other| other == envelope))
            .ok_or("destination has no identical ADSR pair; select an available envelope slot explicitly before transfer")? as u8;
        json!({"mode":"adsr","bytes":envelope,"source_index":settings.adsr,"destination_index":result.adsr,"exact":true})
    } else if settings.adsr == 128 {
        json!({"mode":"fixed_gain","gain":127,"exact":true})
    } else {
        let (index, report) =
            map_script(destination, donor, 10, settings.adsr & 127, "gain envelope")?;
        result.adsr = index | 128;
        report
    };
    let (volume, volume_report) =
        map_script(destination, donor, 4, settings.vol_env, "volume envelope")?;
    let (vibrato, vibrato_report) = map_script(destination, donor, 6, settings.vibrato, "vibrato")?;
    result.vol_env = volume;
    result.vibrato = vibrato;
    let source_global = byte(donor, ENGINE_HEADER + 15)?;
    let target_global = byte(destination, ENGINE_HEADER + 15)?;
    result.transpose = transpose(settings.transpose, source_global, target_global)?;
    let tuning = vec![source.tuning[0], pitch_index];
    Ok(MappedInstrument {
        tuning: tuning.clone(),
        settings: result,
        report: json!({"kind":"compile_standard_semantic_mapping","pitch_words":PITCH_WORDS,
            "pitch_sha256":sha256_hex(&pitch_bytes),"source_tuning":source.tuning,"destination_tuning":tuning,
            "envelope":envelope_report,"volume_envelope":volume_report,"vibrato":vibrato_report,
            "source_global_transpose":source_global as i8,"destination_global_transpose":target_global as i8,
            "effective_transpose_preserved":true,"table_contents_identical":true,
            "runtime_sound_verified":false}),
    })
}

/// Explicit unused table indices to populate when the destination lacks exact data.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableAllocation {
    #[serde(default)]
    pub pitch: Option<u8>,
    #[serde(default)]
    pub adsr: Option<u8>,
    #[serde(default)]
    pub volume: Option<u8>,
    #[serde(default)]
    pub vibrato: Option<u8>,
    #[serde(default)]
    pub gain: Option<u8>,
}
impl TableAllocation {
    pub fn is_empty(&self) -> bool {
        self.pitch.is_none()
            && self.adsr.is_none()
            && self.volume.is_none()
            && self.vibrato.is_none()
            && self.gain.is_none()
    }
}

pub fn used_indices(bank: &InstrumentBank) -> Result<Value, String> {
    driver(bank)?;
    let references = crate::native_references::referenced_indices(bank)?;
    let pitch_indices = referenced_pitch_indices(bank, &references.samples)?;
    Ok(
        json!({"pitch":pitch_indices,"adsr":references.adsr,"volume":references.volume,
        "vibrato":references.vibrato,"gain":references.gain,
        "native_reference_analysis":references,
        "reference_coverage":"all loaded native headers and reachable streams, including sample references beyond the declared directory; external unloaded consumers are not represented"}),
    )
}

fn referenced_pitch_indices(
    bank: &InstrumentBank,
    samples: &std::collections::BTreeSet<u8>,
) -> Result<std::collections::BTreeSet<u8>, String> {
    let mut pitch_indices = std::collections::BTreeSet::new();
    for slot in 0..bank.layout.slots {
        pitch_indices
            .insert(byte(bank, bank.layout.tuning as usize + slot as usize * 2 + 1)? & 127);
    }
    for sample in samples {
        // Both reviewed drivers double SRCN in eight-bit A, transfer it to Y,
        // then increment Y to read the pitch-table index. High SRCNs alias the
        // lower tuning records even when outside the declared sample directory.
        let offset = sample.wrapping_mul(2) as usize + 1;
        let index = byte(bank, bank.layout.tuning as usize + offset).map_err(|e| {
            format!("cannot protect pitch-table reference of native sample {sample}: {e}")
        })?;
        pitch_indices.insert(index & 127);
    }
    Ok(pitch_indices)
}

struct BankExtension<'a> {
    original: &'a InstrumentBank,
    assets: std::collections::BTreeMap<String, Vec<u8>>,
    layout: crate::instruments::BankLayout,
    writes: Vec<Value>,
    readbacks: Vec<(usize, u8, Option<Vec<u8>>)>,
}
impl<'a> BankExtension<'a> {
    fn append(&mut self, bytes: &[u8], role: &str) -> Result<usize, String> {
        let block = module_blocks(&self.assets["bank.bin"])?[0].clone();
        let start = block.address + block.length;
        let end = start
            .checked_add(bytes.len())
            .ok_or("table allocation size overflow")?;
        if end > self.layout.sample_memory_end as usize {
            return Err(format!(
                "{role} needs {} additional bytes; bank append capacity is {}. Select another bank or reserve more verified memory",
                bytes.len(),
                self.layout.sample_memory_end as usize - start
            ));
        }
        for (name, asset) in &self.assets {
            if name == "bank.bin" {
                continue;
            }
            for other in module_blocks(asset)? {
                if start < other.address + other.length && other.address < end {
                    return Err(format!(
                        "{role} allocation overlaps module {name}; declared free memory is occupied"
                    ));
                }
            }
        }
        let length = u16::try_from(block.length + bytes.len())
            .map_err(|_| "allocated bank exceeds its block size field")?;
        let bank = self.assets.get_mut("bank.bin").expect("bank present");
        bank.splice(bank.len() - 2..bank.len() - 2, bytes.iter().copied());
        bank[2..4].copy_from_slice(&length.to_le_bytes());
        self.writes.push(
            json!({"role":role,"address":start,"bytes":bytes.len(),"sha256":sha256_hex(bytes)}),
        );
        Ok(start)
    }
    fn header_pointer(&mut self, field: usize, address: usize) -> Result<(), String> {
        let at = self
            .original
            .file_offset("runtime-data.bin", ENGINE_HEADER + field, 2)?;
        let bytes = self
            .assets
            .get_mut("runtime-data.bin")
            .ok_or("missing resident module")?;
        let old = bytes[at..at + 2].to_vec();
        bytes[at..at + 2].copy_from_slice(&(address as u16).to_le_bytes());
        self.writes.push(json!({"role":"engine_table_pointer","header_offset":field,"old":old,"address":address}));
        Ok(())
    }
    fn pointer_table(
        &mut self,
        field: usize,
        index: u8,
        payload: &[u8],
        role: &str,
    ) -> Result<(), String> {
        let mut entries = self.original.read(table(self.original, field)?, 256)?;
        let start = self.append(payload, role)?;
        entries[index as usize * 2..index as usize * 2 + 2]
            .copy_from_slice(&(start as u16).to_le_bytes());
        let new_table = self.append(&entries, &format!("{role} pointer table"))?;
        self.readbacks.push((field, index, Some(payload.to_vec())));
        self.header_pointer(field, new_table)
    }
}

pub fn prepare_destination(
    destination: &InstrumentBank,
    donor: &InstrumentBank,
    settings: &InstrumentSettings,
    allocation: &TableAllocation,
) -> Result<(InstrumentBank, Value), String> {
    if settings.srcn >= 128 {
        return Err("high sample indices alter the reviewed driver's pitch carry; select a source sample below 128 or add an explicit interpretation".into());
    }
    let source_contract = driver(donor)?;
    driver(destination)?;
    if allocation.is_empty() {
        return Ok((
            InstrumentBank::new(destination.layout.clone(), destination.assets.clone())?,
            json!({"allocated":false}),
        ));
    }
    let used = used_indices(destination)?;
    let check = |kind: &str, index: u8, allow_zero: bool| -> Result<(), String> {
        if index >= 128 || (!allow_zero && index == 0) {
            return Err(format!(
                "{kind} allocation index must be {}..127",
                if allow_zero { 0 } else { 1 }
            ));
        }
        if used[kind]
            .as_array()
            .ok_or("missing reference inventory")?
            .iter()
            .any(|v| v.as_u64() == Some(index as u64))
        {
            return Err(format!(
                "{kind} index {index} is referenced by existing instruments or native streams; choose an unused index from inspection"
            ));
        }
        Ok(())
    };
    let mut extension = BankExtension {
        original: destination,
        assets: destination.assets.clone(),
        layout: destination.layout.clone(),
        writes: vec![],
        readbacks: vec![],
    };
    if let Some(index) = allocation.pitch {
        check("pitch", index, false)?;
        let source = donor.sample(settings.srcn)?;
        extension.pointer_table(
            22,
            index,
            &donor.read(
                pitch_pointer(donor, source_contract, source.tuning[1])?,
                2 + PITCH_WORDS * 2,
            )?,
            "pitch",
        )?;
    }
    if let Some(index) = allocation.adsr {
        check("adsr", index, true)?;
        if settings.adsr >= 128 {
            return Err("source uses GAIN rather than an ADSR pair".into());
        }
        let mut pairs = destination.read(table(destination, 12)?, 256)?;
        pairs[index as usize * 2..index as usize * 2 + 2]
            .copy_from_slice(&donor.envelope(settings.adsr)?);
        let address = extension.append(&pairs, "ADSR table")?;
        extension.header_pointer(12, address)?;
        extension.layout.envelopes = address as u16;
        extension.layout.envelope_count = 128;
        extension.readbacks.push((12, index, None));
    }
    for (kind, field, selected, source) in [
        ("volume", 4, allocation.volume, settings.vol_env),
        ("vibrato", 6, allocation.vibrato, settings.vibrato),
        ("gain", 10, allocation.gain, settings.adsr & 127),
    ] {
        if let Some(index) = selected {
            check(kind, index, false)?;
            if source == 0 || (kind == "gain" && settings.adsr < 129) {
                return Err(format!("source has no active {kind} program to allocate"));
            }
            let program = indexed_script(donor, field, source)?;
            extension.pointer_table(field, index, &program, kind)?;
        }
    }
    let result = InstrumentBank::new(extension.layout, extension.assets)?;
    driver(&result)?;
    for (field, index, payload) in &extension.readbacks {
        let old_table = table(destination, *field)?;
        let new_table = table(&result, *field)?;
        for other in 0..128 {
            if other != *index as usize
                && destination.read(old_table + other * 2, 2)?
                    != result.read(new_table + other * 2, 2)?
            {
                return Err("unselected runtime table entry changed during allocation".into());
            }
        }
        if let Some(payload) = payload {
            let address = word(&result, new_table + *index as usize * 2)?;
            if result.read(address, payload.len())? != *payload {
                return Err("allocated runtime table payload differs on readback".into());
            }
        } else if result.envelope(*index)? != donor.envelope(settings.adsr)? {
            return Err("allocated ADSR pair differs on readback".into());
        }
    }
    Ok((
        result,
        json!({"allocated":true,"selected_indices":allocation,"writes":extension.writes,
        "preservation":"existing table entries preserved except explicitly selected unreferenced indices; original bank and runtime retained",
        "reference_inventory":used,"table_readback":true,"unselected_entries_checked_per_table":127,
        "activation":{"complete_matching_modules_required":true,"refresh_cached_engine_tables":true,
            "reviewed_host_command":241,"restore_matching_runtime_when_switching_banks":true,
            "verified_by":"static driver control flow; no target execution"}}),
    ))
}

fn transpose(source: i16, source_global: u8, destination_global: u8) -> Result<i16, String> {
    if !(-128..=127).contains(&source) {
        return Err("native transpose must fit a signed byte".into());
    }
    Ok((source as u8)
        .wrapping_add(source_global)
        .wrapping_sub(destination_global) as i8 as i16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruments::BankLayout;
    use std::collections::BTreeMap;

    fn module(address: u16, payload: &[u8]) -> Vec<u8> {
        [
            address.to_le_bytes().as_slice(),
            (payload.len() as u16).to_le_bytes().as_slice(),
            payload,
            &[255, 255],
        ]
        .concat()
    }
    fn fixture(limit: u32) -> InstrumentBank {
        let mut resident = vec![0; 0x1800];
        resident[22..24].copy_from_slice(&0x1900_u16.to_le_bytes());
        for i in 0..128 {
            resident[0x100 + i * 2..0x102 + i * 2]
                .copy_from_slice(&(0x2000_u16 + i as u16 * 2).to_le_bytes());
        }
        let mut bank = vec![0; 128];
        bank[..8].copy_from_slice(&[0x10, 0x50, 0x10, 0x50, 0x19, 0x50, 0x19, 0x50]);
        bank[16] = 1;
        bank[25] = 1;
        InstrumentBank::new(
            BankLayout {
                directory: 0x5000,
                slots: 2,
                tuning: 0x4000,
                envelopes: 0x2800,
                envelope_count: 4,
                sample_memory_end: limit,
                reserved_slots: vec![],
            },
            BTreeMap::from([
                ("runtime-driver.bin".into(), module(0x400, &[1, 2, 3])),
                ("runtime-data.bin".into(), module(0x1800, &resident)),
                ("template.bin".into(), module(0x4000, &[0; 16])),
                ("bank.bin".into(), module(0x5000, &bank)),
            ]),
        )
        .unwrap()
    }

    #[test]
    fn relocated_table_retains_other_entries_and_exact_payload_with_capacity_refusal() {
        let bank = fixture(0x6000);
        let payload: Vec<u8> = (0..242).map(|i| (i * 37) as u8).collect();
        let mut extension = BankExtension {
            original: &bank,
            assets: bank.assets.clone(),
            layout: bank.layout.clone(),
            writes: vec![],
            readbacks: vec![],
        };
        extension.pointer_table(22, 91, &payload, "pitch").unwrap();
        let derived = InstrumentBank::new(extension.layout, extension.assets).unwrap();
        assert_eq!(derived.read(0x5080, 242).unwrap(), payload);
        let new_table = word(&derived, ENGINE_HEADER + 22).unwrap();
        assert_eq!(new_table, 0x5080 + 242);
        for i in 0..128 {
            let actual = word(&derived, new_table + i * 2).unwrap();
            assert_eq!(actual, if i == 91 { 0x5080 } else { 0x2000 + i * 2 });
        }
        for (name, before) in &bank.assets {
            let after = &derived.assets[name];
            for (i, byte) in before.iter().enumerate() {
                let selected = (name == "runtime-data.bin" && (26..28).contains(&i))
                    || (name == "bank.bin" && ((2..4).contains(&i) || i >= before.len() - 2));
                if !selected {
                    assert_eq!(*byte, after[i], "{name} byte {i}");
                }
            }
        }
        let tight = fixture(0x5080 + 242 + 255);
        let before = tight.assets.clone();
        let mut extension = BankExtension {
            original: &tight,
            assets: tight.assets.clone(),
            layout: tight.layout.clone(),
            writes: vec![],
            readbacks: vec![],
        };
        assert!(
            extension
                .pointer_table(22, 91, &payload, "pitch")
                .unwrap_err()
                .contains("append capacity")
        );
        assert_eq!(tight.assets, before);
    }

    #[test]
    fn envelope_program_retains_values_and_checks_backward_loop_boundaries() {
        let mut bank = fixture(0x6000);
        let at = bank.file_offset("runtime-data.bin", 0x2200, 6).unwrap();
        let program = [3, 0x88, 2, 0x7f, 0x81, 4];
        bank.assets.get_mut("runtime-data.bin").unwrap()[at..at + 6].copy_from_slice(&program);
        let valid = InstrumentBank::new(bank.layout.clone(), bank.assets.clone()).unwrap();
        assert_eq!(script(&valid, 0x2200).unwrap(), program);
        bank.assets.get_mut("runtime-data.bin").unwrap()[at + 5] = 3;
        let invalid = InstrumentBank::new(bank.layout, bank.assets).unwrap();
        assert!(
            script(&invalid, 0x2200)
                .unwrap_err()
                .contains("duration boundary")
        );
    }

    #[test]
    fn native_transpose_preserves_effective_pitch_across_global_offsets() {
        assert_eq!(transpose(-24, 36, 12).unwrap(), 0);
        assert_eq!(transpose(120, 64, 0).unwrap(), -72);
        for source in [-128, -24, 0, 120, 127] {
            let target = transpose(source, 36, 12).unwrap();
            for note in [0_u8, 1, 60, 119, 255] {
                assert_eq!(
                    note.wrapping_add(source as u8).wrapping_add(36),
                    note.wrapping_add(target as u8).wrapping_add(12)
                );
            }
        }
        assert!(transpose(128, 36, 12).is_err());
    }

    #[test]
    fn native_sample_references_protect_tuning_outside_declared_slots_and_wrap() {
        let mut bank = fixture(0x6000);
        let mut tuning = vec![0; 256];
        tuning[54 * 2 + 1] = 6;
        tuning[225_u8.wrapping_mul(2) as usize + 1] = 17;
        bank.assets
            .insert("template.bin".into(), module(0x4000, &tuning));
        let bank = InstrumentBank::new(bank.layout, bank.assets).unwrap();
        let samples = std::collections::BTreeSet::from([54, 225]);
        assert_eq!(
            referenced_pitch_indices(&bank, &samples).unwrap(),
            std::collections::BTreeSet::from([0, 6, 17])
        );
        let short = fixture(0x6000);
        assert!(
            referenced_pitch_indices(&short, &samples)
                .unwrap_err()
                .contains("cannot protect pitch-table reference of native sample 54")
        );
    }
}
