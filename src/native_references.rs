//! Conservative table-reference discovery in loaded Compile standard native songs.
//! Both outcomes of conditional control flow are inspected; this is not playback.
use crate::instruments::InstrumentBank;
use serde::Serialize;
use std::collections::{BTreeSet, VecDeque};

#[derive(Debug, Default, Serialize)]
pub struct References {
    pub adsr: BTreeSet<u8>,
    pub volume: BTreeSet<u8>,
    pub vibrato: BTreeSet<u8>,
    pub gain: BTreeSet<u8>,
    pub samples: BTreeSet<u8>,
    pub song_table_entries: usize,
    pub unresolved_requests: Vec<u8>,
    pub header_count: usize,
    pub command_count: usize,
    pub scope: String,
}

impl References {
    fn envelope(&mut self, value: u8) {
        if value & 0x80 != 0 {
            self.gain.insert(value & 0x7f);
        } else {
            self.adsr.insert(value);
        }
    }
}

fn read_word(
    read: &impl Fn(usize, usize) -> Result<Vec<u8>, String>,
    at: usize,
) -> Result<usize, String> {
    let raw = read(at, 2)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]) as usize)
}

/// Inspect all 128 request pointers and every reachable instruction of their
/// structurally valid immutable headers, including resident effects and music.
/// Unresolved requests, mutable scores and unloaded future banks stay explicit.
pub fn referenced_indices(bank: &InstrumentBank) -> Result<References, String> {
    if !crate::compile_instruments::is_supported(bank) {
        return Err("reference discovery needs a reviewed Compile standard driver".into());
    }
    collect(&|at, length| bank.read(at, length), &|at, length| {
        bank.file_offset("template.bin", at, length).is_ok()
            || bank.file_offset("runtime-data.bin", at, length).is_ok()
    })
}

fn collect(
    read: &impl Fn(usize, usize) -> Result<Vec<u8>, String>,
    score_range: &impl Fn(usize, usize) -> bool,
) -> Result<References, String> {
    let table = read_word(read, 0x1800)?;
    let mut result = References::default();
    let mut queue = VecDeque::new();
    let mut headers = BTreeSet::new();
    // The reviewed driver doubles an eight-bit request, so there are 128
    // distinct pointer entries. There is no authored table-count field.
    for request in 0..128 {
        let pointer = read_word(read, table + request * 2)?;
        let count = read(pointer, 1).ok().map(|raw| usize::from(raw[0]));
        let header = count
            .filter(|count| (1..=16).contains(count))
            .filter(|count| score_range(pointer, 1 + count * 14))
            .and_then(|count| read(pointer + 1, count * 14).ok());
        let Some(raw) = header.filter(|raw| {
            raw.as_chunks::<14>().0.iter().all(|track| {
                track[0] < 16 && score_range(u16::from_le_bytes([track[8], track[9]]) as usize, 1)
            })
        }) else {
            result.unresolved_requests.push(request as u8);
            continue;
        };
        result.song_table_entries += 1;
        if headers.insert(pointer) {
            for track in raw.as_chunks::<14>().0 {
                result.samples.insert(track[10]);
                result.envelope(track[11]);
                result.volume.insert(track[3] & 0x7f);
                result.vibrato.insert(track[4] & 0x7f);
                queue.push_back(u16::from_le_bytes([track[8], track[9]]) as usize);
            }
        }
    }
    if result.unresolved_requests.contains(&1) {
        return Err("native music request has no complete immutable header and score".into());
    }
    let percussion = read_word(read, 0x1802)?
        .checked_add(17)
        .ok_or("percussion address overflow")?;
    let mut visited = BTreeSet::new();
    while let Some(pc) = queue.pop_front() {
        if pc >= 65536 {
            return Err("native score leaves ARAM".into());
        }
        if !visited.insert(pc) {
            continue;
        }
        if !score_range(pc, 1) {
            return Err(format!(
                "native score at {pc:#06x} leaves loaded score modules"
            ));
        }
        let opcode = read(pc, 1).map_err(|e| format!("native score at {pc:#06x}: {e}"))?[0];
        let length = match opcode {
            0xa3 => {
                if read(pc + 1, 1)?[0] == 0 {
                    3
                } else {
                    2
                }
            }
            0x00..=0x7f
            | 0xc0..=0xdd
            | 0xdf..=0xee
            | 0x82
            | 0x86
            | 0x8c
            | 0x99
            | 0x9b
            | 0xa1
            | 0xa2
            | 0xa6
            | 0xa7
            | 0xa8
            | 0xa9
            | 0xaa
            | 0xb0
            | 0xae
            | 0xaf => 1,
            0x83
            | 0x84
            | 0x87..=0x8a
            | 0x8e..=0x92
            | 0x94
            | 0x96
            | 0x98
            | 0x9d
            | 0x9f
            | 0xa0
            | 0xa4
            | 0xab
            | 0xac
            | 0xb1
            | 0xde
            | 0xf0..=0xff => 2,
            0x80 | 0x8b | 0x8d | 0x93 | 0x95 | 0x97 | 0x9a | 0x9e | 0xef => 3,
            0x81 | 0x85 | 0x9c | 0xa5 | 0xad => 4,
            _ => {
                return Err(format!(
                    "uninterpreted native opcode {opcode:#04x} at {pc:#06x}; table allocation is not safe"
                ));
            }
        };
        if !score_range(pc, length) {
            return Err("native instruction crosses its module boundary".into());
        }
        let raw = read(pc, length)?;
        match opcode {
            0x83 => {
                result.vibrato.insert(raw[1] & 0x7f);
            }
            0x88 => {
                result.volume.insert(raw[1] & 0x7f);
            }
            0x9f => result.envelope(raw[1]),
            0xa0 => {
                result.samples.insert(raw[1]);
            }
            0xc0..=0xdd => {
                let entry = read(percussion + usize::from(opcode - 0xc0) * 8, 8)?;
                result.samples.insert(entry[0]);
                result.envelope(entry[1]);
                result.volume.insert(entry[2] & 0x7f);
                if entry[4] & 0x80 == 0 {
                    result.vibrato.insert(entry[3] & 0x7f);
                }
            }
            _ => {}
        }
        match opcode {
            0x82 | 0x86 | 0x9b => {}
            0x80 => queue.push_back(u16::from_le_bytes([raw[1], raw[2]]) as usize),
            0x95 | 0x9a | 0x9e => {
                queue.push_back(u16::from_le_bytes([raw[1], raw[2]]) as usize);
                queue.push_back(pc + length);
            }
            0x81 | 0xa5 | 0xad => {
                queue.push_back(u16::from_le_bytes([raw[2], raw[3]]) as usize);
                queue.push_back(pc + length);
            }
            0xa4 => {
                queue.push_back(pc + 2);
                queue.push_back(pc + 4);
            }
            _ => queue.push_back(pc + length),
        }
    }
    result.header_count = headers.len();
    result.command_count = visited.len();
    result.scope = "All reachable instructions from structurally valid immutable headers among the 128 request entries, including loaded music and resident effects. Both conditional paths are included. Unresolved requests, future banks, dynamically written scores and runtime execution are not covered.".into();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Vec<u8> {
        let mut bytes = vec![0; 65536];
        bytes[0x1800..0x1804].copy_from_slice(&[0, 0x20, 0, 0x22]);
        bytes[0x2000..0x2004].copy_from_slice(&[4, 0x20, 4, 0x20]);
        bytes[0x2004] = 1;
        bytes[0x2005 + 8..0x2005 + 12].copy_from_slice(&[0, 0x30, 7, 2]);
        bytes[0x3000..0x3003].copy_from_slice(&[0x9a, 0, 0x31]);
        bytes[0x3003..0x3007].copy_from_slice(&[0xa5, 0, 0, 0x32]);
        bytes[0x3007..0x300a].copy_from_slice(&[0x88, 9, 0x82]);
        bytes[0x3100..0x3110].copy_from_slice(&[
            0x8c, 0x83, 5, 0x95, 0, 0x32, 0xef, 6, 4, 0xa3, 0, 0xb2, 0xa3, 2, 0xa8, 0x9b,
        ]);
        bytes[0x3200..0x320b]
            .copy_from_slice(&[0x9f, 0x84, 0xa0, 12, 0xc0, 0xff, 3, 0x9e, 0, 0x31, 0x82]);
        bytes[0x2211..0x2219].copy_from_slice(&[13, 8, 11, 10, 0, 0, 0, 60]);
        bytes
    }
    #[test]
    fn references_follow_calls_both_branches_and_percussion_without_running_loops() {
        let bytes = memory();
        let refs = collect(
            &|at, n| {
                bytes
                    .get(at..at + n)
                    .map(<[u8]>::to_vec)
                    .ok_or("outside".into())
            },
            &|_, _| true,
        )
        .unwrap();
        assert_eq!(refs.samples, BTreeSet::from([7, 12, 13]));
        assert_eq!(refs.adsr, BTreeSet::from([2, 8]));
        assert_eq!(refs.gain, BTreeSet::from([4]));
        assert_eq!(refs.volume, BTreeSet::from([0, 9, 11]));
        assert_eq!(refs.vibrato, BTreeSet::from([0, 5, 10]));
        assert_eq!(refs.header_count, 1);
        assert_eq!(refs.song_table_entries, 2);
    }
    #[test]
    fn unknown_or_truncated_reachable_commands_prevent_reference_claims() {
        let mut bytes = memory();
        bytes[0x3100] = 0xb2;
        assert!(
            collect(
                &|at, n| bytes
                    .get(at..at + n)
                    .map(<[u8]>::to_vec)
                    .ok_or("outside".into()),
                &|_, _| true
            )
            .unwrap_err()
            .contains("uninterpreted native opcode")
        );
        bytes[0x3100] = 0x9a;
        assert!(
            collect(
                &|at, n| if at == 0x3100 && n > 1 {
                    Err("truncated".into())
                } else {
                    Ok(bytes[at..at + n].to_vec())
                },
                &|_, _| true
            )
            .is_err()
        );
    }
}
