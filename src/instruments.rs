//! Exact BRR reuse in declared module banks. No resampling or BRR encoding.
use crate::store::sha256_hex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BankLayout {
    /// ARAM address of little-endian (start, loop) directory entries.
    pub directory: u16,
    pub slots: u16,
    /// Two opaque driver tuning bytes per sample slot in the template.
    pub tuning: u16,
    /// Two envelope bytes per entry in the runtime modules.
    pub envelopes: u16,
    pub envelope_count: u16,
    /// Exclusive ARAM limit available to the variable bank; never inferred from zero bytes.
    pub sample_memory_end: u32,
    /// Slots owned by effects or other protected consumers.
    #[serde(default)]
    pub reserved_slots: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstrumentSettings {
    pub srcn: u8,
    pub adsr: u8,
    /// Copy from the donor track; this is a musical mapping, not inferred from BRR.
    pub octave_shift: i16,
    #[serde(default)]
    pub transpose: i16,
    #[serde(default)]
    pub vol_env: u8,
    #[serde(default)]
    pub vibrato: u8,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SamplePlacement {
    /// Append in declared free memory, preserving all old sample bytes.
    #[default]
    Append,
    /// Reuse the selected slot's exclusive sample range if the new sample fits.
    Replace,
}

#[derive(Debug, Clone)]
pub struct ModuleBlock {
    pub address: usize,
    pub offset: usize,
    pub length: usize,
}

pub fn module_blocks(bytes: &[u8]) -> Result<Vec<ModuleBlock>, String> {
    let mut offset = 0;
    let mut blocks = Vec::new();
    loop {
        let address = word(bytes, offset)? as usize;
        offset += 2;
        if address == 0xffff {
            if offset != bytes.len() {
                return Err("module has trailing data".into());
            }
            return Ok(blocks);
        }
        let length = word(bytes, offset)? as usize;
        offset += 2;
        if length == 0 || address + length > 65536 || offset + length > bytes.len() {
            return Err("module has an empty, truncated or out-of-range block".into());
        }
        if blocks
            .iter()
            .any(|b: &ModuleBlock| overlaps(address, length, b.address, b.length))
        {
            return Err("module blocks overlap".into());
        }
        blocks.push(ModuleBlock {
            address,
            offset,
            length,
        });
        offset += length;
    }
}
fn word(data: &[u8], at: usize) -> Result<u16, String> {
    let value = data.get(at..at + 2).ok_or("missing two-byte field")?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}
fn overlaps(a: usize, len_a: usize, b: usize, len_b: usize) -> bool {
    a < b + len_b && b < a + len_a
}

#[derive(Debug, Clone)]
pub struct Sample {
    pub start: usize,
    pub loop_address: usize,
    pub loop_offset: Option<usize>,
    pub looping: bool,
    pub bytes: Vec<u8>,
    pub tuning: Vec<u8>,
}
impl Sample {
    pub fn summary(&self) -> Value {
        json!({"start": self.start, "loop_address": self.loop_address, "loop_offset": self.loop_offset,
            "looping": self.looping, "bytes": self.bytes.len(), "blocks": self.bytes.len()/9,
            "decoded_samples_per_pass": self.bytes.len()/9*16, "sha256": sha256_hex(&self.bytes), "tuning": self.tuning})
    }
}

pub struct InstrumentBank {
    pub layout: BankLayout,
    pub assets: BTreeMap<String, Vec<u8>>,
    memory: Vec<Option<u8>>,
    bank_block: ModuleBlock,
}
impl InstrumentBank {
    pub fn new(layout: BankLayout, assets: BTreeMap<String, Vec<u8>>) -> Result<Self, String> {
        if layout.slots == 0
            || layout.slots > 256
            || layout.envelope_count == 0
            || layout.envelope_count > 256
            || layout.sample_memory_end > 65536
            || layout
                .reserved_slots
                .iter()
                .any(|s| u16::from(*s) >= layout.slots)
        {
            return Err("invalid bank layout or reserved slot".into());
        }
        if !assets.contains_key("template.bin")
            || !assets.contains_key("bank.bin")
            || !assets.keys().any(|name| name.starts_with("runtime-"))
        {
            return Err("template, bank and driver/runtime modules are required".into());
        }
        let mut memory = vec![None; 65536];
        for (name, bytes) in &assets {
            for block in module_blocks(bytes).map_err(|e| format!("{name}: {e}"))? {
                for index in 0..block.length {
                    let address = block.address + index;
                    let byte = bytes[block.offset + index];
                    if memory[address].is_some_and(|existing| existing != byte) {
                        return Err(format!(
                            "module inputs disagree at ARAM {address:#06x}; supply the final effective modules"
                        ));
                    }
                    memory[address] = Some(byte);
                }
            }
        }
        let blocks = module_blocks(&assets["bank.bin"])?;
        if blocks.len() != 1 {
            return Err("variable bank must contain exactly one contiguous block".into());
        }
        let bank_block = blocks[0].clone();
        if bank_block.address + bank_block.length > layout.sample_memory_end as usize {
            return Err("bank exceeds declared sample memory".into());
        }
        let bank = Self {
            layout,
            assets,
            memory,
            bank_block,
        };
        bank.file_offset(
            "bank.bin",
            bank.layout.directory as usize,
            bank.layout.slots as usize * 4,
        )?;
        bank.file_offset(
            "template.bin",
            bank.layout.tuning as usize,
            bank.layout.slots as usize * 2,
        )?;
        bank.read(
            bank.layout.envelopes as usize,
            bank.layout.envelope_count as usize * 2,
        )?;
        Ok(bank)
    }
    pub(crate) fn read(&self, at: usize, length: usize) -> Result<Vec<u8>, String> {
        self.memory
            .get(at..at.checked_add(length).ok_or("address overflow")?)
            .ok_or("address outside ARAM")?
            .iter()
            .map(|value| {
                value.ok_or_else(|| {
                    format!(
                        "no supplied module covers ARAM {at:#06x}..{:#06x}",
                        at + length
                    )
                })
            })
            .collect()
    }
    pub(crate) fn file_offset(
        &self,
        name: &str,
        at: usize,
        length: usize,
    ) -> Result<usize, String> {
        module_blocks(&self.assets[name])?
            .into_iter()
            .find(|b| b.address <= at && at + length <= b.address + b.length)
            .map(|b| b.offset + at - b.address)
            .ok_or_else(|| format!("{name} does not contain the declared table/range"))
    }
    fn directory(&self, slot: u8) -> Result<(usize, usize), String> {
        if u16::from(slot) >= self.layout.slots {
            return Err("sample slot is outside the directory".into());
        }
        let raw = self.read(self.layout.directory as usize + slot as usize * 4, 4)?;
        Ok((word(&raw, 0)? as usize, word(&raw, 2)? as usize))
    }
    pub fn envelope(&self, index: u8) -> Result<Vec<u8>, String> {
        if u16::from(index) >= self.layout.envelope_count {
            return Err("envelope index is outside the declared table".into());
        }
        self.read(self.layout.envelopes as usize + index as usize * 2, 2)
    }
    pub fn sample(&self, slot: u8) -> Result<Sample, String> {
        let (start, loop_address) = self.directory(slot)?;
        let mut bytes = Vec::new();
        let mut cursor = start;
        loop {
            let block = self.read(cursor, 9)?;
            let header = block[0];
            bytes.extend(block);
            cursor += 9;
            if header & 1 != 0 {
                let looping = header & 2 != 0;
                let loop_offset = (start..cursor)
                    .contains(&loop_address)
                    .then(|| loop_address - start)
                    .filter(|offset| offset % 9 == 0);
                if looping && loop_offset.is_none() {
                    return Err("active BRR loop is not an aligned block inside the sample; transfer of separate loop regions is not supported".into());
                }
                return Ok(Sample {
                    start,
                    loop_address,
                    loop_offset,
                    looping,
                    bytes,
                    tuning: self.read(self.layout.tuning as usize + slot as usize * 2, 2)?,
                });
            }
        }
    }
    pub fn samples(&self) -> Vec<Value> {
        (0..self.layout.slots).map(|slot| match self.sample(slot as u8) {
            Ok(sample) => {let mut result = sample.summary(); result["slot"] = json!(slot); result["available"] = json!(true); result["reserved"] = json!(self.layout.reserved_slots.contains(&(slot as u8))); result},
            Err(reason) => json!({"slot":slot,"available":false,"reason":reason,"directory":self.directory(slot as u8).ok()}),
        }).collect()
    }
    /// Runtime equality includes driver code, envelopes and pitch interpretation.
    pub fn runtime_identity(&self) -> String {
        let identities: BTreeMap<_, _> = self
            .assets
            .iter()
            .filter(|(name, _)| name.starts_with("runtime-"))
            .map(|(name, bytes)| (name, sha256_hex(bytes)))
            .collect();
        sha256_hex(
            serde_json::to_string(&identities)
                .expect("serializes")
                .as_bytes(),
        )
    }
    pub fn summary(&self) -> Value {
        let samples = self.samples();
        json!({"layout":self.layout,"runtime_sha256":self.runtime_identity(),"bank_start":self.bank_block.address,
            "bank_end":self.bank_block.address+self.bank_block.length,"append_capacity":self.layout.sample_memory_end as usize-self.bank_block.address-self.bank_block.length,
            "available_samples":samples.iter().filter(|v| v["available"]==true).count(),"slots":self.layout.slots,
            "envelopes":(0..self.layout.envelope_count).map(|i| json!({"index":i,"bytes":self.envelope(i as u8).expect("table checked")})).collect::<Vec<_>>()})
    }
    pub fn copy_from(
        &self,
        donor: &Self,
        settings: &InstrumentSettings,
        slot: u8,
        placement: SamplePlacement,
    ) -> Result<(Self, Value), String> {
        let semantic = if crate::compile_instruments::is_supported(self)
            && crate::compile_instruments::is_supported(donor)
        {
            Some(crate::compile_instruments::map_settings(
                self, donor, settings,
            )?)
        } else {
            if self.runtime_identity() != donor.runtime_identity()
                || self.layout.envelopes != donor.layout.envelopes
                || self.layout.envelope_count != donor.layout.envelope_count
            {
                return Err("runtime/envelope contracts differ and no verified semantic adapter applies; copying opaque indices would not preserve their meaning".into());
            }
            None
        };
        if u16::from(slot) >= self.layout.slots || self.layout.reserved_slots.contains(&slot) {
            return Err(
                "destination slot is outside the directory or reserved by another consumer".into(),
            );
        }
        if semantic.is_some() && slot >= 128 {
            return Err("high sample indices alter the reviewed driver's pitch carry; choose a destination slot below 128".into());
        }
        let source = donor.sample(settings.srcn)?;
        let envelope = if semantic.is_none() {
            Some(donor.envelope(settings.adsr)?)
        } else {
            None
        };
        let tuning = semantic
            .as_ref()
            .map(|mapped| mapped.tuning.as_slice())
            .unwrap_or(&source.tuning);
        let mut binding = semantic
            .as_ref()
            .map(|mapped| mapped.settings.clone())
            .unwrap_or_else(|| settings.clone());
        let semantic_report = semantic.as_ref().map(|mapped| mapped.report.clone());
        let previous = self.sample(slot).ok();
        let existing = (0..self.layout.slots).find_map(|s| {
            self.sample(s as u8).ok().filter(|other| {
                other.bytes == source.bytes
                    && other.looping == source.looping
                    && (!source.looping || other.loop_offset == source.loop_offset)
            })
        });
        let mut assets = self.assets.clone();
        let (start, method, write_range) = if let Some(existing) = existing {
            (existing.start, "reuse_identical", None)
        } else {
            match placement {
                SamplePlacement::Append => {
                    let start = self.bank_block.address + self.bank_block.length;
                    let end = start
                        .checked_add(source.bytes.len())
                        .ok_or("bank size overflow")?;
                    if end > self.layout.sample_memory_end as usize
                        || self.memory[start..end.min(65536)]
                            .iter()
                            .any(Option::is_some)
                    {
                        return Err(format!(
                            "sample needs {} bytes; append capacity is {}. Select a compatible bank with room or explicitly replace a large exclusive slot",
                            source.bytes.len(),
                            self.layout.sample_memory_end as usize - start
                        ));
                    }
                    let new_length = self.bank_block.length + source.bytes.len();
                    let length = u16::try_from(new_length)
                        .map_err(|_| "bank block exceeds its size field")?;
                    let bytes = assets.get_mut("bank.bin").expect("bank");
                    bytes.splice(
                        bytes.len() - 2..bytes.len() - 2,
                        source.bytes.iter().copied(),
                    );
                    bytes[2..4].copy_from_slice(&length.to_le_bytes());
                    (start, "append", Some((start, source.bytes.len())))
                }
                SamplePlacement::Replace => {
                    let old = previous
                        .as_ref()
                        .ok_or("selected slot has no bounded sample to replace; use append")?;
                    if source.bytes.len() > old.bytes.len() {
                        return Err(format!(
                            "sample needs {} bytes; selected sample holds {} bytes; use append or another slot",
                            source.bytes.len(),
                            old.bytes.len()
                        ));
                    }
                    let start = old.start;
                    let end = start + source.bytes.len();
                    let at = self.file_offset("bank.bin", start, source.bytes.len())?;
                    if overlaps(
                        start,
                        source.bytes.len(),
                        self.layout.directory as usize,
                        self.layout.slots as usize * 4,
                    ) {
                        return Err("sample overlaps its directory".into());
                    }
                    for other_slot in 0..self.layout.slots {
                        if other_slot as u8 == slot {
                            continue;
                        }
                        let (other_start, other_loop) = self.directory(other_slot as u8)?;
                        if (start..end).contains(&other_start) || (start..end).contains(&other_loop)
                        {
                            return Err(format!(
                                "sample storage is shared with slot {other_slot}; use append"
                            ));
                        }
                        match self.sample(other_slot as u8) {
                            Ok(other)
                                if overlaps(
                                    start,
                                    source.bytes.len(),
                                    other.start,
                                    other.bytes.len(),
                                ) =>
                            {
                                return Err(format!(
                                    "replacement would alter slot {other_slot}; use append"
                                ));
                            }
                            Err(_) if (self.bank_block.address..end).contains(&other_start) => {
                                return Err(format!(
                                    "cannot prove slot {other_slot}'s sample ends before replacement; use append"
                                ));
                            }
                            _ => {}
                        }
                    }
                    assets.get_mut("bank.bin").expect("bank")[at..at + source.bytes.len()]
                        .copy_from_slice(&source.bytes);
                    (
                        start,
                        "replace_exclusive",
                        Some((start, source.bytes.len())),
                    )
                }
            }
        };
        let loop_address = start + source.loop_offset.unwrap_or(0);
        let entry = self.file_offset(
            "bank.bin",
            self.layout.directory as usize + slot as usize * 4,
            4,
        )?;
        let bank_bytes = assets.get_mut("bank.bin").expect("bank");
        bank_bytes[entry..entry + 2].copy_from_slice(&(start as u16).to_le_bytes());
        bank_bytes[entry + 2..entry + 4].copy_from_slice(&(loop_address as u16).to_le_bytes());
        let tune = self.file_offset(
            "template.bin",
            self.layout.tuning as usize + slot as usize * 2,
            2,
        )?;
        assets.get_mut("template.bin").expect("template")[tune..tune + 2].copy_from_slice(tuning);
        let result = Self::new(self.layout.clone(), assets)?;
        let copied = result.sample(slot)?;
        if copied.bytes != source.bytes
            || copied.tuning != tuning
            || copied.looping != source.looping
            || (source.looping && copied.loop_offset != source.loop_offset)
        {
            return Err("copied sample readback differs".into());
        }
        // All other samples keep exact directory entries, payloads and tuning.
        for other in 0..self.layout.slots {
            if other as u8 == slot {
                continue;
            }
            if self.directory(other as u8)? != result.directory(other as u8)? {
                return Err("unselected directory entry changed".into());
            }
            if let Ok(original) = self.sample(other as u8) {
                let after = result.sample(other as u8)?;
                if original.bytes != after.bytes || original.tuning != after.tuning {
                    return Err("unselected instrument changed".into());
                }
            }
        }
        if semantic.is_some() {
            let readback = crate::compile_instruments::map_settings(&result, donor, settings)?;
            if readback.tuning != copied.tuning
                || serde_json::to_value(&readback.settings).expect("serializes")
                    != serde_json::to_value(&binding).expect("serializes")
            {
                return Err("semantic instrument readback differs".into());
            }
        }
        binding.srcn = slot;
        Ok((
            result,
            json!({"method":method,"source_sample":source.summary(),"replaced_sample":previous.map(|p|p.summary()),
            "destination_sample":copied.summary(),"binding":binding,"envelope_bytes":envelope.or_else(||semantic_report.as_ref().and_then(|r|serde_json::from_value(r["envelope"]["bytes"].clone()).ok())),"semantic_mapping":semantic_report,"sample_write":write_range.map(|(start,length)|json!({"start":start,"bytes":length})),
            "verification":{"brr_bytes_identical":true,"active_loop_offset_identical":true,"tuning_bytes_identical":copied.tuning==source.tuning,"tuning_semantics_identical":true,"envelope_bytes_identical":true,"unselected_samples_identical":true,"reencoded":false},
            "scope":"sample and selected instrument settings only; track melodies, mix, modulation context and runtime sound are separate"}),
        ))
    }
    pub fn apply_native_binding(
        &self,
        index: usize,
        binding: &InstrumentSettings,
    ) -> Result<(Self, Value), String> {
        if binding.octave_shift != 0 || !(-128..=127).contains(&binding.transpose) {
            return Err("native binding needs octave_shift=0 and a signed-byte transpose; use source_track to preserve native pitch settings".into());
        }
        let tracks = crate::compile_instruments::native_tracks(self)?;
        let track = tracks
            .get(index)
            .ok_or("destination native track is outside the loaded song")?;
        if track.flags & 8 != 0 {
            return Err("destination track starts in noise mode; select a tonal track for BRR instrument reuse".into());
        }
        let at = self.file_offset("template.bin", track.header_address as usize, 14)?;
        let mut assets = self.assets.clone();
        let template = assets.get_mut("template.bin").expect("template");
        for (offset, value) in [
            (3, binding.vol_env),
            (4, binding.vibrato),
            (5, binding.transpose as u8),
            (10, binding.srcn),
            (11, binding.adsr),
        ] {
            template[at + offset] = value;
        }
        let result = Self::new(self.layout.clone(), assets)?;
        let after = crate::compile_instruments::native_tracks(&result)?;
        if serde_json::to_value(&after[index].settings).expect("serializes")
            != serde_json::to_value(binding).expect("serializes")
        {
            return Err("native header readback differs".into());
        }
        Ok((
            result,
            json!({"track":index,"header_address":track.header_address,"before":track.settings,"after":binding,
            "score_stream_bytes_unchanged":true,"scope":"initial tonal instrument only; later score commands and mix are retained"}),
        ))
    }
}
