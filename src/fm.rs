//! Exact static OPN register patches inside immutable original containers.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::store::sha256_hex;

pub const OPERATOR_MASKS: [u8; 7] = [0x7f, 0x7f, 0xdf, 0x9f, 0x1f, 0xff, 0x0f];
pub const SCOPE: &str = "Static B0 algorithm/feedback and four register slots at groups 30..90 only. Channel pan/AMS/PMS, global LFO/clock/mode, key masks, driver macros, later carrier attenuation and score bindings are separate. Register equality does not establish sound equality.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpnChip {
    /// Register family only; actual hardware and clock are not established.
    Opn,
    Ym2203,
    Ym2608,
    Ym2612,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperatorPacking {
    /// Seven consecutive register groups per stored operator.
    OperatorRows,
    /// Four consecutive operator slots per register group.
    RegisterColumns,
}

fn register_slot_order() -> [usize; 4] {
    [0, 1, 2, 3]
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FmLayout {
    /// Absolute file offsets, in caller-defined record-index order.
    pub record_offsets: Vec<usize>,
    pub record_size: usize,
    pub algorithm_offset: usize,
    pub operators_offset: usize,
    pub packing: OperatorPacking,
    /// Stored operator positions mapped to hardware register slots 0,4,8,12.
    /// Hardware slots correspond to named operators 1,3,2,4.
    #[serde(default = "register_slot_order")]
    pub slot_order: [usize; 4],
    #[serde(default)]
    pub reserved_records: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FmPatch {
    pub algorithm_feedback: u8,
    /// Hardware register slots 0,4,8,12; groups 30,40,50,60,70,80,90.
    pub operators: [[u8; 7]; 4],
}

impl FmPatch {
    pub fn decoded(&self) -> Value {
        let operators: Vec<_> = self
            .operators
            .iter()
            .enumerate()
            .map(|(slot, r)| {
                json!({
                    "register_slot":slot,"named_operator":([1,3,2,4][slot]),
                    "detune":r[0]>>4,"multiple":r[0]&15,"total_level":r[1],
                    "rate_scale":r[2]>>6,"attack":r[2]&31,"am_enabled":r[3]&128!=0,
                    "decay":r[3]&31,"sustain_rate":r[4],"sustain_level":r[5]>>4,
                    "release":r[5]&15,"ssg_eg":r[6]
                })
            })
            .collect();
        json!({"algorithm":self.algorithm_feedback&7,"feedback":self.algorithm_feedback>>3,"operators":operators})
    }
    pub fn digest(&self) -> String {
        let mut bytes = vec![self.algorithm_feedback];
        bytes.extend(self.operators.iter().flatten());
        sha256_hex(&bytes)
    }
}

#[derive(Debug, Clone)]
pub struct FmBank {
    pub bytes: Vec<u8>,
    pub layout: FmLayout,
    pub chip: OpnChip,
}

impl FmLayout {
    fn fields(&self) -> Result<Vec<(usize, u8)>, String> {
        if self.slot_order.into_iter().collect::<BTreeSet<_>>() != BTreeSet::from([0, 1, 2, 3]) {
            return Err("slot_order must be a permutation of 0,1,2,3".into());
        }
        let mut fields = vec![(self.algorithm_offset, 0x3f)];
        for slot in 0..4 {
            let stored = self
                .slot_order
                .iter()
                .position(|&s| s == slot)
                .expect("validated");
            for (group, mask) in OPERATOR_MASKS.iter().enumerate() {
                let step = match self.packing {
                    OperatorPacking::OperatorRows => stored * 7 + group,
                    OperatorPacking::RegisterColumns => group * 4 + stored,
                };
                fields.push((
                    self.operators_offset
                        .checked_add(step)
                        .ok_or("operator offset overflow")?,
                    *mask,
                ));
            }
        }
        if fields.iter().any(|&(offset, _)| offset >= self.record_size)
            || fields.iter().map(|x| x.0).collect::<BTreeSet<_>>().len() != 29
        {
            return Err("29 distinct register locations must fit inside each record".into());
        }
        Ok(fields)
    }
}

impl FmBank {
    pub fn new(bytes: Vec<u8>, layout: FmLayout, chip: OpnChip) -> Result<Self, String> {
        layout.fields()?;
        if layout.record_offsets.is_empty() || layout.record_offsets.len() > 65536 {
            return Err(
                "declare 1..65536 record offsets; do not infer a bank from arbitrary bytes".into(),
            );
        }
        if layout
            .reserved_records
            .iter()
            .any(|&i| i >= layout.record_offsets.len())
        {
            return Err("reserved record index is outside the bank".into());
        }
        for &start in &layout.record_offsets {
            let end = start
                .checked_add(layout.record_size)
                .ok_or("record extent overflow")?;
            if end > bytes.len() {
                return Err(format!("record at {start} is truncated"));
            }
        }
        // Aliases may be inspected, but copy refuses every overlapping recipient.
        Ok(Self {
            bytes,
            layout,
            chip,
        })
    }
    fn fields(&self) -> Result<Vec<(usize, u8)>, String> {
        let mut fields = self.layout.fields()?;
        if self.chip == OpnChip::Ym2203 {
            for slot in 0..4 {
                fields[1 + slot * 7 + 3].1 = 0x1f;
            }
        }
        Ok(fields)
    }
    pub fn patch(&self, index: usize) -> Result<FmPatch, String> {
        let start = *self
            .layout
            .record_offsets
            .get(index)
            .ok_or("record index is outside the bank")?;
        let fields = self.fields()?;
        let values: Vec<_> = fields
            .iter()
            .map(|&(offset, mask)| self.bytes[start + offset] & mask)
            .collect();
        let mut operators = [[0; 7]; 4];
        for (slot, row) in operators.iter_mut().enumerate() {
            row.copy_from_slice(&values[1 + slot * 7..8 + slot * 7]);
        }
        Ok(FmPatch {
            algorithm_feedback: values[0],
            operators,
        })
    }
    pub fn inspect(&self, index: usize) -> Result<Value, String> {
        let patch = self.patch(index)?;
        let start = self.layout.record_offsets[index];
        let fields = self.fields()?;
        let unmodeled_bits: Vec<_> = fields
            .iter()
            .filter_map(|&(offset, mask)| {
                let value = self.bytes[start + offset] & !mask;
                (value != 0).then(|| json!({"record_offset":offset,"mask":!mask,"value":value}))
            })
            .collect();
        Ok(
            json!({"index":index,"file_offset":start,"raw_record":&self.bytes[start..start+self.layout.record_size],
            "patch":patch,"patch_sha256":patch.digest(),"decoded":patch.decoded(),
            "unmodeled_register_bits":unmodeled_bits,"reserved":self.layout.reserved_records.contains(&index),
            "chip_model_declared":self.chip!=OpnChip::Opn,"scope":SCOPE}),
        )
    }
    pub fn copy_patch(
        &self,
        donor: &Self,
        source_index: usize,
        destination_index: usize,
    ) -> Result<(Self, Value), String> {
        let patch = donor.patch(source_index)?;
        let before = self.patch(destination_index)?;
        let start = self.layout.record_offsets[destination_index];
        let end = start + self.layout.record_size;
        if self.layout.reserved_records.contains(&destination_index) {
            return Err("destination record is reserved; select another record".into());
        }
        if self
            .layout
            .record_offsets
            .iter()
            .enumerate()
            .any(|(i, &s)| i != destination_index && s < end && start < s + self.layout.record_size)
        {
            return Err(
                "destination record aliases or overlaps another record; select an exclusive record"
                    .into(),
            );
        }
        if self.chip == OpnChip::Ym2203 && patch.operators.iter().any(|op| op[3] & 128 != 0) {
            return Err("YM2203 cannot represent the source AM enable setting; choose a compatible destination instead of discarding it".into());
        }
        let fields = self.fields()?;
        let values: Vec<_> = std::iter::once(patch.algorithm_feedback)
            .chain(patch.operators.iter().flatten().copied())
            .collect();
        let mut copied = self.clone();
        let mut writes = Vec::new();
        for (&(offset, mask), &value) in fields.iter().zip(&values) {
            let at = start + offset;
            let old = copied.bytes[at];
            copied.bytes[at] = (old & !mask) | (value & mask);
            if old != copied.bytes[at] {
                writes.push(
                    json!({"file_offset":at,"mask":mask,"before":old,"after":copied.bytes[at]}),
                );
            }
        }
        if copied.patch(destination_index)? != patch {
            return Err("FM patch readback differs".into());
        }
        let allowed: std::collections::BTreeMap<_, _> = fields
            .iter()
            .map(|&(at, mask)| (start + at, mask))
            .collect();
        if self
            .bytes
            .iter()
            .zip(&copied.bytes)
            .enumerate()
            .any(|(i, (&a, &b))| (a ^ b) & !allowed.get(&i).copied().unwrap_or(0) != 0)
        {
            return Err("unselected source bits changed".into());
        }
        for i in 0..self.layout.record_offsets.len() {
            if i != destination_index && self.patch(i)? != copied.patch(i)? {
                return Err("unselected patch changed".into());
            }
        }
        let report = json!({"source_index":source_index,"destination_index":destination_index,"before":before,"after":patch,
            "source_chip":donor.chip,"destination_chip":self.chip,"patch_sha256":patch.digest(),"writes":writes,
            "verification":{"register_parameters_identical":true,"unselected_bits_identical":true,"unselected_records_identical":true,"container_length_identical":true},
            "scope":SCOPE,"binding_policy":"Replaces this bank record for every consumer referencing it; does not select an unused slot or rewrite song bindings.",
            "runtime_sound_verified":false});
        Ok((copied, report))
    }
}
