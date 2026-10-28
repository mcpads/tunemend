//! Nintendo DS sound data: inspection of retained SDAT/SWAR/SWAV/SBNK/STRM
//! files, decoded wave and stream export, and explicit SWAV replacement in a
//! SWAR copy. Container rules are in [`crate::nds_sound`], codecs in
//! [`crate::nds_adpcm`]; the decoded WAV re-enters `prepare_sample` as a
//! retained source.
use super::instrument_sources::{SOURCES, SoundSourceRecord};
use super::*;
use crate::nds_adpcm;
use crate::nds_sound::{
    Encoding, FileHeader, InstrumentBody, Kind, NdsError, Sbnk, Sdat, Strm, Swar, Swav, WaveInfo,
};
use std::collections::BTreeMap;

/// Longest audition: first pass plus baked loops, in seconds.
const MAX_AUDITION_SECONDS: f64 = 600.0;
const MAX_AUDITION_LOOPS: u8 = 16;
const DEFAULT_PAGE: u32 = 50;
/// A resolved wave-archive slot: INFO index, FAT member and wave count.
type ResolvedArchive = (usize, Option<u16>, Option<usize>);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NdsView {
    /// Header, blocks, counts, layout and refusals of the selected file.
    #[default]
    Summary,
    /// SWAR/SWAV: paged sample-block infos with raw offsets.
    Waves,
    /// SWAR/SWAV: one wave (`index`) with decoded readback.
    Wave,
    /// SBNK: paged instrument records and note definitions; inside an SDAT
    /// each wave-archive slot is resolved through the INFO bank entry.
    Instruments,
    /// STRM IMA-ADPCM: paged per-block headers of channel `index`.
    Blocks,
    /// SDAT: paged FAT members with kinds, names and INFO references.
    Members,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectNdsSoundInput {
    /// A sound source retained with load_instrument_source: an SDAT, SWAR,
    /// SWAV, SBNK, STRM, SSEQ or SSAR file.
    pub source: String,
    /// SDAT only: interpret this FAT file (0-based) in place. Offsets are
    /// then member-relative; `member_offset` gives the SDAT offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<usize>,
    #[serde(default)]
    pub view: NdsView,
    /// wave view: the wave (default 0); blocks view: the channel (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// Paged views: first item; continue with next_cursor.
    #[serde(default)]
    pub offset: usize,
    /// Items per page, default 50, at most 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportNdsSampleInput {
    pub job_id: String,
    /// Retained SWAR, SWAV, STRM, or SDAT with `member`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<usize>,
    /// SWAR wave index; a SWAV has only wave 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wave: Option<usize>,
    /// STRM channel (0-based).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    /// Looped sources only: also write audition.wav with the first pass and
    /// this many loop traversals baked (1..16, at most 600 seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audition_loops: Option<u8>,
    /// MIDI note written as the WAV smpl unity note (default 60, a
    /// placeholder; an SBNK note definition's base_note is the bank's choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_note: Option<u8>,
    /// New relative folder.
    pub destination: String,
}

/// Encoding of a written SWAV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NdsEncoding {
    Pcm8,
    Pcm16,
    ImaAdpcm,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaceNdsWaveInput {
    pub job_id: String,
    /// Retained SWAR, or SDAT with `member` naming a SWAR. The result is a
    /// new standalone SWAR; an SDAT is not rebuilt.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<usize>,
    /// Wave index to replace.
    pub wave: usize,
    /// Retained RIFF/WAVE source with the new samples, at the rate to write
    /// (resample with prepare_sample first; its target.wav works here).
    pub replacement: String,
    /// 0-based WAV channel; required for multi-channel files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
    pub encoding: NdsEncoding,
    /// Loop start frame; the loop runs to the last frame. Omit both loop
    /// fields for a one-shot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_start_frame: Option<usize>,
    /// Use this WAV smpl loop (0-based); it must end at the last frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smpl_loop: Option<usize>,
    /// Change kinds to accept: sample_values (lossy encoding),
    /// bit_depth_reduction (WAV to 16 bits), padding (one-shot zero padding
    /// to whole 32-bit words).
    #[serde(default)]
    pub accepted_changes: Vec<String>,
    /// File name of the written SWAR (default replaced.swar).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swar_name: Option<String>,
    pub destination: String,
}

fn refusal(e: NdsError, base: usize, member: Option<usize>) -> ToolResponse {
    let location = match member {
        Some(m) => format!("/member/{m}/bytes/{}", e.offset),
        None => format!("/source/bytes/{}", e.offset),
    };
    let mut response = ToolResponse::refused(
        Code::InvalidRange,
        &location,
        format!(
            "{}{e}; the retained original is unchanged",
            member.map_or(String::new(), |m| format!("member {m}, "))
        ),
    );
    response.result = json!({"error_offset":e.offset,"absolute_offset":base + e.offset,"field":e.field,
        "message":e.message,"member":member,"retained":true});
    response
}

fn bad(location: &str, message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::InvalidRange, location, message)
}

fn page(offset: usize, limit: Option<u32>, total: usize) -> (usize, usize, Option<usize>) {
    let start = offset.min(total);
    let end = start
        .saturating_add(limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE) as usize)
        .min(total);
    (start, end, (end < total).then_some(end))
}

fn pcm_bytes(pcm: &[i32]) -> Vec<u8> {
    pcm.iter().flat_map(|&x| (x as i16).to_le_bytes()).collect()
}

fn readback(pcm: &[i32]) -> Value {
    let peak = pcm.iter().map(|x| x.abs()).max().unwrap_or(0);
    let rms =
        (pcm.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / pcm.len().max(1) as f64).sqrt();
    json!({"samples":pcm.len(),"pcm16_sha256":sha256_hex(&pcm_bytes(pcm)),"peak":peak,"rms":rms,
        "first_samples":&pcm[..pcm.len().min(8)],"silent":peak == 0})
}

/// The decoded first pass of a wave, with its loop start, and the facts a
/// report needs.
struct DecodedWave {
    pcm: Vec<i32>,
    loop_start: Option<usize>,
    rate: u32,
    codec: Value,
}

fn decode_wave(file: &[u8], w: &WaveInfo) -> Result<DecodedWave, NdsError> {
    let d = w.decode(file)?;
    let codec = match &d.adpcm {
        None => {
            json!({"encoding":w.encoding.name(),"scaling":if w.encoding == Encoding::Pcm8 {"signed 8-bit; NN plays as NN00h"} else {"signed 16-bit little-endian"}})
        }
        Some((h, loop_state, clamped)) => json!({"encoding":"ima_adpcm",
            "header":{"offset":w.data_offset(),"word":format!("{:08X}h", h.word),"initial_pcm":h.state.pcm,"table_index":h.state.index,
                "unused_bits_23_31":h.unused_bits,"initial_pcm_is_minus_8000h":h.state.pcm == -0x8000},
            "nibble_order":"low nibble first","clamped_codes":clamped,
            "loop_state":loop_state.map(|s| json!({"pcm":s.pcm,"table_index":s.index,
                "note":"the hardware reloads the PCM16 value and index that the first pass had at the loop start (GBATEK), so every traversal decodes like the first"}))}),
    };
    Ok(DecodedWave {
        loop_start: w.looped().then(|| w.loop_sample()),
        pcm: d.pcm,
        rate: w.sample_rate as u32,
        codec,
    })
}

/// A mono PCM16 WAV, with a smpl loop `[loop_start, last]` when looped.
fn decoded_wav(pcm: &[i32], rate: u32, loop_start: Option<usize>, root: u8) -> Vec<u8> {
    let data = pcm_bytes(pcm);
    let period = (1e9 / rate as f64).round() as u32;
    let mut extra = vec![];
    if let Some(start) = loop_start {
        let mut smpl = crate::wav::smpl_body(
            root as u32,
            0,
            &[(0, start as u32, pcm.len() as u32 - 1, 0)],
        );
        smpl[8..12].copy_from_slice(&period.to_le_bytes());
        extra.push((b"smpl", smpl));
    } else if root != 60 {
        let mut smpl = crate::wav::smpl_body(root as u32, 0, &[]);
        smpl[8..12].copy_from_slice(&period.to_le_bytes());
        extra.push((b"smpl", smpl));
    }
    crate::wav::build(1, 1, rate, 16, None, &data, &extra)
}

fn kind_counts(kinds: impl Iterator<Item = &'static str>) -> BTreeMap<&'static str, usize> {
    let mut out = BTreeMap::new();
    for k in kinds {
        *out.entry(k).or_insert(0) += 1;
    }
    out
}

impl Service {
    /// The retained bytes and the selected file range: the whole source, or
    /// one SDAT FAT member.
    fn nds_file(
        &mut self,
        source: &str,
        member: Option<usize>,
    ) -> Result<(Vec<u8>, usize, usize), ToolResponse> {
        let bytes = self.sound_source_bytes(source)?;
        let Some(m) = member else {
            let len = bytes.len();
            return Ok((bytes, 0, len));
        };
        let sdat = Sdat::parse(&bytes).map_err(|e| {
            if bytes.starts_with(b"SDAT") {
                refusal(e, 0, None)
            } else {
                bad(
                    "/member",
                    "member selects a FAT file of an SDAT; this source is not an SDAT",
                )
            }
        })?;
        let entry = sdat.fat.get(m).ok_or_else(|| {
            bad(
                "/member",
                format!(
                    "no FAT file {m}; the SDAT has {} (view members lists them)",
                    sdat.fat.len()
                ),
            )
        })?;
        let (s, e) = entry.range.clone().map_err(|e| refusal(e, 0, None))?;
        Ok((bytes, s, e))
    }

    pub(super) fn inspect_nds_sound(&mut self, input: InspectNdsSoundInput) -> Handled {
        let (bytes, base, end) = self.nds_file(&input.source, input.member)?;
        let file = &bytes[base..end];
        let header = FileHeader::parse(file).map_err(|e| refusal(e, base, input.member))?;
        let fail = |e: NdsError| refusal(e, base, input.member);
        let mut result =
            json!({"source":input.source,"kind":header.kind.name(),"header":header.report()});
        if let Some(m) = input.member {
            result["member"] = json!(m);
            result["member_offset"] = json!(base);
            result["offsets"] = json!("member-relative; add member_offset for the SDAT offset");
        }
        let wrong = |view: &str, kinds: &str| {
            Err(bad(
                "/view",
                format!(
                    "view {view} applies to {kinds}; this file is {}",
                    header.kind.name().to_uppercase()
                ),
            ))
        };
        let summary = match (input.view, header.kind) {
            (NdsView::Summary, Kind::Swar) => {
                let s = Swar::parse(file).map_err(fail)?;
                let l = s.layout();
                let ok: Vec<&WaveInfo> = s.waves.iter().filter_map(|w| w.as_ref().ok()).collect();
                let refused: Vec<Value> = s.waves.iter().enumerate().filter_map(|(i, w)| w.as_ref().err().map(|e| json!({"wave":i,"error_offset":e.offset,"field":e.field,"message":e.message}))).collect();
                result["swar"] = s.report();
                result["waves"] = json!({"count":s.count,"interpreted":ok.len(),"refused":refused.len(),
                    "encodings":kind_counts(ok.iter().map(|w| w.encoding.name())),"looped":ok.iter().filter(|w| w.looped()).count()});
                result["refusals"] = json!(&refused[..refused.len().min(20)]);
                result["layout"] = json!({"increasing_offsets":l.increasing,"shared_blocks":l.shared,"overlapping_blocks":l.overlaps,
                    "unreferenced_ranges":l.gaps.iter().map(|(a,b)| json!({"offset":a,"end":b,"bytes":b-a})).collect::<Vec<_>>()});
                format!("SWAR with {} waves ({} refused)", s.count, refused.len())
            }
            (NdsView::Summary, Kind::Swav) => {
                let s = Swav::parse(file).map_err(fail)?;
                result["swav"] = s.report();
                result["wave"] = s.wave.report();
                format!("SWAV {} wave", s.wave.encoding.name())
            }
            (NdsView::Summary, Kind::Sbnk) => {
                let s = Sbnk::parse(file).map_err(fail)?;
                let (gaps, shared) = s.coverage();
                let refused: Vec<Value> = s.instruments.iter().filter_map(|i| i.body.as_ref().err().map(|e| json!({"instrument":i.index,"error_offset":e.offset,"field":e.field,"message":e.message}))).collect();
                result["sbnk"] = s.report();
                result["instruments"] = json!({"count":s.count,"types":kind_counts(s.instruments.iter().map(|i| match &i.body {
                    Ok(InstrumentBody::Empty) => "empty", Ok(InstrumentBody::Single(_)) => "single", Ok(InstrumentBody::DrumSet{..}) => "drum_set",
                    Ok(InstrumentBody::KeySplit{..}) => "key_split", Ok(InstrumentBody::Unknown) => "undocumented", Err(_) => "refused"})),
                    "refused":refused.len()});
                result["refusals"] = json!(&refused[..refused.len().min(20)]);
                result["coverage"] = json!({"unreferenced_ranges":gaps.iter().map(|(a,b)| json!({"offset":a,"end":b,"bytes":b-a})).collect::<Vec<_>>(),
                    "overlapping_instruments":shared});
                if let Some(member) = input.member {
                    result["bank_entries"] = self.bank_entries(&bytes, member);
                }
                format!("SBNK with {} instrument records", s.count)
            }
            (NdsView::Summary, Kind::Strm) => {
                let s = Strm::parse(file).map_err(fail)?;
                result["strm"] = s.report();
                format!(
                    "STRM {} {} channel(s), {} samples at {} Hz{}",
                    s.encoding.name(),
                    s.channels,
                    s.samples,
                    s.sample_rate,
                    if s.looped() {
                        format!(", loop from {}", s.loop_offset)
                    } else {
                        String::new()
                    }
                )
            }
            (NdsView::Summary, Kind::Sdat) => {
                let s = Sdat::parse(file).map_err(fail)?;
                let mut kinds: BTreeMap<String, (usize, std::collections::BTreeSet<String>)> =
                    BTreeMap::new();
                let mut refused = vec![];
                for f in &s.fat {
                    match &f.range {
                        Ok((a, b)) => {
                            let m = &file[*a..*b];
                            let k = Kind::from_magic(m)
                                .map_or("unknown", |k| k.name())
                                .to_owned();
                            let e = kinds.entry(k).or_default();
                            e.0 += 1;
                            e.1.insert(sha256_hex(m));
                        }
                        Err(e) => refused.push(
                            json!({"member":f.index,"error_offset":e.offset,"message":e.message}),
                        ),
                    }
                }
                result["sdat"] = s.report();
                result["members"] = json!(
                    kinds
                        .iter()
                        .map(|(k, (n, d))| (
                            k.clone(),
                            json!({"paths":n,"distinct_byte_streams":d.len()})
                        ))
                        .collect::<serde_json::Map<_, _>>()
                );
                result["refusals"] = json!(refused);
                result["next"] = json!(
                    "view members pages FAT files with names and INFO references; pass member to inspect one in place"
                );
                format!("SDAT with {} FAT files", s.fat.len())
            }
            (NdsView::Summary, Kind::Sseq | Kind::Ssar) => {
                result["note"] = json!(
                    "sequence data is retained exactly; SSEQ/SSAR event interpretation and playback are not implemented"
                );
                format!("{} header", header.kind.name().to_uppercase())
            }
            (NdsView::Waves, Kind::Swar | Kind::Swav) => {
                let waves: Vec<Result<WaveInfo, NdsError>> = if header.kind == Kind::Swar {
                    Swar::parse(file).map_err(fail)?.waves
                } else {
                    vec![Ok(Swav::parse(file).map_err(fail)?.wave)]
                };
                let (start, stop, next) = page(input.offset, input.limit, waves.len());
                result["waves"] = json!(waves[start..stop].iter().enumerate().map(|(k, w)| match w {
                    Ok(w) => { let mut r = w.report(); r["index"] = json!(start + k); r["block_sha256"] = json!(sha256_hex(&file[w.info_offset..w.block_end()])); r }
                    Err(e) => json!({"index":start + k,"refused":e.to_string(),"error_offset":e.offset}),
                }).collect::<Vec<_>>());
                result["total"] = json!(waves.len());
                result["next_cursor"] = json!(next);
                format!("waves {start}..{stop} of {}", waves.len())
            }
            (NdsView::Wave, Kind::Swar | Kind::Swav) => {
                let index = input.index.unwrap_or(0);
                let w = self
                    .wave(file, header.kind, index)
                    .map_err(|r| r.unwrap_or_else(&fail))?;
                let d = decode_wave(file, &w).map_err(fail)?;
                result["wave"] = w.report();
                result["wave"]["index"] = json!(index);
                result["codec"] = d.codec;
                result["decoded"] = readback(&d.pcm);
                result["prepare_sample"] = match w.encoding {
                    Encoding::ImaAdpcm => {
                        json!({"route":"export_nds_sample writes decoded.wav (PCM16 with a smpl loop when looped) and retains it as a source: prepare_sample encoding wav, wav.smpl_loop 0 with loop_policy preserve"})
                    }
                    e => {
                        let mut args = json!({"source":input.source,"offset":base + w.data_offset(),"length":w.data_bytes(),
                            "encoding":if e == Encoding::Pcm8 {"s8"} else {"s16le"},"sample_rate":w.sample_rate});
                        if w.looped() {
                            args["loop_offset"] = json!(w.loop_words as usize * 4);
                            args["loop_policy"] = json!("preserve");
                        }
                        json!({"route":"the raw PCM bytes are a direct prepare_sample slice of the retained source","arguments":args})
                    }
                };
                format!(
                    "wave {index}: {} samples, {}",
                    w.samples(),
                    w.encoding.name()
                )
            }
            (NdsView::Instruments, Kind::Sbnk) => {
                let s = Sbnk::parse(file).map_err(fail)?;
                let (start, stop, next) = page(input.offset, input.limit, s.instruments.len());
                let archives = input.member.map(|m| self.bank_archives(&bytes, m));
                let rows: Vec<Value> = s.instruments[start..stop].iter().map(|i| {
                    let mut r = i.report();
                    if let Some(Some(archives)) = &archives {
                        r["wave_references"] = json!(i.definitions().iter().map(|d| {
                            let slot = archives.get(d.swar as usize).cloned().flatten();
                            json!({"definition_offset":d.offset,"swar_slot":d.swar,"swav":d.swav,"wave_archive":slot.as_ref().map(|(wa, file, count)| json!({
                                "info_index":wa,"member":file,"waves":count,"swav_in_range":count.map(|c| (d.swav as usize) < c)}))})
                        }).collect::<Vec<_>>());
                    }
                    r
                }).collect();
                result["instruments"] = json!(rows);
                result["total"] = json!(s.instruments.len());
                result["next_cursor"] = json!(next);
                if input.member.is_none() {
                    result["wave_references"] = json!(
                        "swar_slot indexes the bank's four wave archives listed in the SDAT INFO bank entry; inspect the bank as an SDAT member to resolve them"
                    );
                }
                format!("instruments {start}..{stop} of {}", s.instruments.len())
            }
            (NdsView::Blocks, Kind::Strm) => {
                let s = Strm::parse(file).map_err(fail)?;
                let channel = input.index.unwrap_or(0);
                if channel >= s.channels as usize {
                    return Err(bad(
                        "/index",
                        format!(
                            "channel {channel} is outside the stream's {} channel(s)",
                            s.channels
                        ),
                    ));
                }
                if s.encoding != Encoding::ImaAdpcm {
                    return Err(bad(
                        "/view",
                        "blocks lists IMA-ADPCM block headers; this stream is PCM",
                    ));
                }
                let d = s.decode_channel(file, channel).map_err(fail)?;
                let (start, stop, next) = page(input.offset, input.limit, d.block_headers.len());
                result["channel"] = json!(channel);
                result["blocks"] = json!(d.block_headers[start..stop].iter().map(|h| json!({"block":h.block,"offset":h.offset,
                    "word":format!("{:08X}h", h.header.word),"initial_pcm":h.header.state.pcm,"table_index":h.header.state.index,
                    "unused_bits_23_31":h.header.unused_bits,"equals_previous_end_state":h.continues})).collect::<Vec<_>>());
                result["continuing_headers"] = json!(
                    d.block_headers
                        .iter()
                        .filter(|h| h.continues == Some(true))
                        .count()
                );
                result["total"] = json!(d.block_headers.len());
                result["next_cursor"] = json!(next);
                result["decoded"] = readback(&d.pcm);
                result["clamped_codes"] = json!(d.clamped);
                format!(
                    "channel {channel} blocks {start}..{stop} of {}",
                    d.block_headers.len()
                )
            }
            (NdsView::Members, Kind::Sdat) => {
                let s = Sdat::parse(file).map_err(fail)?;
                let (start, stop, next) = page(input.offset, input.limit, s.fat.len());
                result["members"] = json!(s.fat[start..stop].iter().map(|f| {
                    let mut r = json!({"member":f.index,"fat_entry_offset":f.entry_offset,"offset":f.offset,"size":f.size,
                        "reserved_zero":f.reserved == [0;8]});
                    match &f.range {
                        Ok((a, b)) => {
                            let m = &file[*a..*b];
                            r["kind"] = json!(Kind::from_magic(m).map_or("unknown", |k| k.name()));
                            r["sha256"] = json!(sha256_hex(m));
                        }
                        Err(e) => r["refused"] = json!(e.to_string()),
                    }
                    r["references"] = json!(s.references(f.index).iter().map(|&(l, i)| {
                        let e = s.info.as_ref().ok().and_then(|lists| lists[l].entries[i].as_ref()).expect("referenced");
                        let mut v = s.info_entry_report(l, i, e);
                        if l == 2 && let Some(arch) = s.bank_wave_archives(i) {
                            v["wave_archive_members"] = json!(arch.iter().map(|a| a.and_then(|(_, f)| f)).collect::<Vec<_>>());
                        }
                        v
                    }).collect::<Vec<_>>());
                    r
                }).collect::<Vec<_>>());
                result["total"] = json!(s.fat.len());
                result["next_cursor"] = json!(next);
                format!("FAT files {start}..{stop} of {}", s.fat.len())
            }
            (NdsView::Waves | NdsView::Wave, _) => return wrong("waves/wave", "SWAR and SWAV"),
            (NdsView::Instruments, _) => return wrong("instruments", "SBNK"),
            (NdsView::Blocks, _) => return wrong("blocks", "STRM"),
            (NdsView::Members, _) => return wrong("members", "SDAT"),
        };
        done(ToolResponse::ok(summary, result))
    }

    /// One wave of a SWAR or the SWAV's only wave; Err(Ok) is a request
    /// refusal, Err(Err) a format refusal.
    fn wave(
        &self,
        file: &[u8],
        kind: Kind,
        index: usize,
    ) -> Result<WaveInfo, Result<ToolResponse, NdsError>> {
        if kind == Kind::Swav {
            if index != 0 {
                return Err(Ok(bad("/index", "a SWAV holds only wave 0")));
            }
            return Swav::parse(file).map(|s| s.wave).map_err(Err);
        }
        let s = Swar::parse(file).map_err(Err)?;
        match s.waves.get(index) {
            None => Err(Ok(bad(
                "/index",
                format!("wave {index} is outside the archive's {} waves", s.count),
            ))),
            Some(Err(e)) => Err(Err(e.clone())),
            Some(Ok(w)) => Ok(w.clone()),
        }
    }

    /// Bank INFO entries naming this SDAT member, with their wave archives.
    fn bank_entries(&self, bytes: &[u8], member: usize) -> Value {
        let Ok(s) = Sdat::parse(bytes) else {
            return Value::Null;
        };
        json!(s.references(member).iter().filter(|(l, _)| *l == 2).map(|&(_, i)| json!({"bank":i,"name":s.name(2, i),
            "wave_archives":s.bank_wave_archives(i).map(|a| a.iter().map(|x| x.map(|(wa, f)| json!({"info_index":wa,"member":f}))).collect::<Vec<_>>())})).collect::<Vec<_>>())
    }

    /// Wave archives of the first bank entry naming this member: (INFO
    /// index, FAT member, wave count) per slot.
    fn bank_archives(&self, bytes: &[u8], member: usize) -> Option<Vec<Option<ResolvedArchive>>> {
        let s = Sdat::parse(bytes).ok()?;
        let bank = s.references(member).into_iter().find(|(l, _)| *l == 2)?.1;
        let arch = s.bank_wave_archives(bank)?;
        Some(
            arch.iter()
                .map(|a| {
                    a.map(|(wa, f)| {
                        let count = f
                            .and_then(|f| s.member(bytes, f as usize).ok())
                            .and_then(|m| Swar::parse(m).ok())
                            .map(|w| w.count as usize);
                        (wa, f, count)
                    })
                })
                .collect(),
        )
    }

    pub(super) fn export_nds_sample(&mut self, input: ExportNdsSampleInput) -> Handled {
        let (bytes, base, end) = self.nds_file(&input.source, input.member)?;
        let file = &bytes[base..end];
        let fail = |e: NdsError| refusal(e, base, input.member);
        let header = FileHeader::parse(file).map_err(fail)?;
        let root = input.root_note.unwrap_or(60);
        if root > 127 {
            return Err(bad("/root_note", "root_note is a MIDI note 0..127"));
        }
        let (pcm, loop_start, rate, original, mut report) = match header.kind {
            Kind::Swar | Kind::Swav => {
                if input.channel.is_some() {
                    return Err(bad(
                        "/channel",
                        "channel selects a STRM channel; use wave for SWAR/SWAV",
                    ));
                }
                let index = input.wave.unwrap_or(0);
                if header.kind == Kind::Swar && input.wave.is_none() {
                    return Err(bad(
                        "/wave",
                        "select the wave to export (view waves lists them)",
                    ));
                }
                let w = self
                    .wave(file, header.kind, index)
                    .map_err(|r| r.unwrap_or_else(&fail))?;
                let d = decode_wave(file, &w).map_err(fail)?;
                let mut r = json!({"wave":w.report(),"codec":d.codec,"index":index});
                r["wave"]["index"] = json!(index);
                let original = file[w.info_offset..w.block_end()].to_vec();
                r["original"] = json!({"file":"original.bin","contents":"the exact 12-byte sample info and its data","offset":w.info_offset,
                    "absolute_offset":base + w.info_offset,"bytes":original.len()});
                (d.pcm, d.loop_start, d.rate, original, r)
            }
            Kind::Strm => {
                if input.wave.is_some() {
                    return Err(bad(
                        "/wave",
                        "wave selects a SWAR/SWAV wave; use channel for STRM",
                    ));
                }
                let s = Strm::parse(file).map_err(fail)?;
                let channel = input.channel.unwrap_or(0) as usize;
                if channel >= s.channels as usize {
                    return Err(bad(
                        "/channel",
                        format!(
                            "channel {channel} is outside the stream's {} channel(s)",
                            s.channels
                        ),
                    ));
                }
                let d = s.decode_channel(file, channel).map_err(fail)?;
                let r = json!({"strm":s.report(),"channel":channel,"clamped_codes":d.clamped,
                    "block_headers":d.block_headers.len(),"continuing_block_headers":d.block_headers.iter().filter(|h| h.continues == Some(true)).count(),
                    "loop_note":s.looped().then_some("every block restarts from its own header, so a traversal from the loop start decodes like the first pass from there (declared reading; the streaming player is software)"),
                    "original":{"file":"original.bin","contents":"the exact STRM file","bytes":file.len(),"absolute_offset":base}});
                (
                    d.pcm,
                    s.looped().then_some(s.loop_offset as usize),
                    s.sample_rate as u32,
                    file.to_vec(),
                    r,
                )
            }
            k => {
                return Err(bad(
                    "/source",
                    format!(
                        "{} holds no wave data; export from a SWAR, SWAV or STRM (an SDAT member)",
                        k.name().to_uppercase()
                    ),
                ));
            }
        };
        if pcm.is_empty() {
            return Err(bad("/wave", "the selection decodes to no samples"));
        }
        let mut files = vec![("original.bin".to_owned(), original)];
        let wav = decoded_wav(&pcm, rate, loop_start, root);
        files.push(("decoded.wav".into(), wav.clone()));
        report["decoded"] = readback(&pcm);
        report["decoded"]["file"] = json!("decoded.wav");
        report["decoded"]["sample_rate"] = json!(rate);
        report["decoded"]["rate_note"] = json!(
            "the header sample_rate; the timer field's rate and its cents difference are in the header report"
        );
        report["decoded"]["smpl"] =
            json!(loop_start.map(
                |s| json!({"loop_start":s,"loop_end_inclusive":pcm.len() - 1,"unity_note":root})
            ));
        if let Some(loops) = input.audition_loops {
            let Some(start) = loop_start else {
                return Err(bad(
                    "/audition_loops",
                    "the selection is one-shot; decoded.wav is the whole sound",
                ));
            };
            if !(1..=MAX_AUDITION_LOOPS).contains(&loops) {
                return Err(bad(
                    "/audition_loops",
                    format!("audition_loops is 1..{MAX_AUDITION_LOOPS}"),
                ));
            }
            let total = pcm.len() + loops as usize * (pcm.len() - start);
            if total as f64 / rate as f64 > MAX_AUDITION_SECONDS {
                return Err(bad(
                    "/audition_loops",
                    format!(
                        "the audition would last {:.1} s; the limit is {MAX_AUDITION_SECONDS} s",
                        total as f64 / rate as f64
                    ),
                ));
            }
            let mut audition = pcm.clone();
            for _ in 0..loops {
                audition.extend_from_slice(&pcm[start..]);
            }
            files.push((
                "audition.wav".into(),
                crate::sample_codec::mono_wav(&audition, rate),
            ));
            report["audition"] = json!({"file":"audition.wav","samples":total,"loop_traversals":loops,
                "model":"decoded samples only at the header rate: no DS mixer, interpolation, volume/pan, SBNK envelope or sequence"});
        }
        report["source"] = json!(input.source);
        report["member"] = json!(input.member);
        report["member_offset"] = json!(input.member.map(|_| base));
        let report_bytes = serde_json::to_vec_pretty(&report).expect("serializes");
        files.push(("report.json".into(), report_bytes));
        let mut response = self.export_sound_files(&input.job_id, &input.destination, files)?;
        let wav_hash = sha256_hex(&wav);
        let store = self.store_mut(true)?;
        store.put_object(&wav)?;
        let record = SoundSourceRecord {
            source: wav_hash.clone(),
            name: format!("{}-decoded.wav", input.job_id),
            bytes: wav.len(),
            derived_from: Some(
                json!({"source":input.source,"member":input.member,"tool":"export_nds_sample","job_id":input.job_id,
                "wave":input.wave,"channel":input.channel}),
            ),
        };
        let mut prepare = json!({"source":wav_hash,"encoding":"wav","sample_rate":rate});
        if loop_start.is_some() {
            prepare["wav"] = json!({"smpl_loop":0});
            prepare["loop_policy"] = json!("preserve");
        }
        report["decoded_source"] = json!(wav_hash);
        report["prepare_sample"] = json!({"arguments":prepare,"note":"add job_id and target; prepare_sample holds at most 8388608 samples"});
        response.summary = format!(
            "decoded {} samples at {rate} Hz{}; decoded.wav retained as source {}",
            pcm.len(),
            loop_start.map_or(String::new(), |s| format!(" with a loop from {s}")),
            &wav_hash[..12]
        );
        let artifacts = std::mem::take(&mut response.artifacts);
        response.result = json!({"destination":input.destination,"files":artifacts.iter().map(|a| json!({"name":a.kind,"sha256":a.sha256})).collect::<Vec<_>>(),
            "report":report,"readback_checked":true});
        response.artifacts = artifacts;
        response.artifacts.push(Artifact {
            kind: "decoded_nds_sample_source".into(),
            path: store.object_path(&wav_hash).display().to_string(),
            sha256: wav_hash.clone(),
        });
        commit(response, move |writer| {
            writer.put_document("", SOURCES, &wav_hash, &record)
        })
    }

    pub(super) fn replace_nds_wave(&mut self, input: ReplaceNdsWaveInput) -> Handled {
        let name = input.swar_name.as_deref().unwrap_or("replaced.swar");
        if name.is_empty()
            || name.len() > 128
            || name.starts_with('.')
            || name.contains(['/', '\\', ':'])
            || !name.ends_with(".swar")
        {
            return Err(bad(
                "/swar_name",
                "give a plain file name ending in .swar, without a folder",
            ));
        }
        for (i, c) in input.accepted_changes.iter().enumerate() {
            if !["sample_values", "bit_depth_reduction", "padding"].contains(&c.as_str()) {
                return Err(bad(
                    &format!("/accepted_changes/{i}"),
                    format!(
                        "unknown change kind `{c}`; sample_values, bit_depth_reduction or padding"
                    ),
                ));
            }
        }
        let (bytes, base, end) = self.nds_file(&input.source, input.member)?;
        let file = bytes[base..end].to_vec();
        let fail = |e: NdsError| refusal(e, base, input.member);
        let swar = Swar::parse(&file).map_err(|e| {
            if file.starts_with(b"SWAR") {
                fail(e)
            } else {
                bad(
                    "/source",
                    "replace_nds_wave needs a SWAR (or an SDAT member that is one)",
                )
            }
        })?;
        let old = match swar.waves.get(input.wave) {
            None => {
                return Err(bad(
                    "/wave",
                    format!(
                        "wave {} is outside the archive's {} waves",
                        input.wave, swar.count
                    ),
                ));
            }
            Some(Err(e)) => return Err(fail(e.clone())),
            Some(Ok(w)) => w.clone(),
        };
        let layout = swar.layout();
        if let Some(&(a, b)) = layout
            .shared
            .iter()
            .find(|(a, b)| *a == input.wave || *b == input.wave)
        {
            return Err(bad(
                "/wave",
                format!(
                    "waves {a} and {b} share one sample block; replacing it would change both entries"
                ),
            ));
        }
        if let Some(&(a, b)) = layout
            .overlaps
            .iter()
            .find(|(a, b)| *a == input.wave || *b == input.wave)
        {
            return Err(bad(
                "/wave",
                format!("the blocks of waves {a} and {b} overlap; their storage is not separable"),
            ));
        }
        // Replacement samples.
        let wav_bytes = self
            .sound_source_bytes(&input.replacement)
            .map_err(|mut r| {
                if let Some(d) = r.diagnostics.first_mut() {
                    d.location = "/replacement".into();
                }
                r
            })?;
        let wav = crate::wav::parse(&wav_bytes)
            .map_err(|e| bad("/replacement", format!("not a usable RIFF/WAVE file: {e}")))?;
        let channel = match (input.channel, wav.channels) {
            (Some(c), _) => c,
            (None, 1) => 0,
            (None, n) => {
                return Err(bad(
                    "/channel",
                    format!("the WAV has {n} channels; choose one with channel"),
                ));
            }
        };
        let interpreted =
            crate::sample_wav::interpret(&wav, crate::sample_wav::Selection::Channel(channel))
                .map_err(|e| bad("/channel", e))?;
        let mut pcm = interpreted.pcm;
        let rate = wav.sample_rate;
        if !(256..=65535).contains(&rate) {
            return Err(bad(
                "/replacement",
                format!(
                    "{rate} Hz does not fit the 16-bit SWAV rate with a nonzero timer (256..65535 Hz); resample with prepare_sample target_sample_rate"
                ),
            ));
        }
        if pcm.is_empty() {
            return Err(bad("/replacement", "the WAV has no frames"));
        }
        let loop_start = match (input.loop_start_frame, input.smpl_loop) {
            (Some(_), Some(_)) => {
                return Err(bad(
                    "/smpl_loop",
                    "give loop_start_frame or smpl_loop, not both",
                ));
            }
            (Some(s), None) => {
                if s >= pcm.len() {
                    return Err(bad(
                        "/loop_start_frame",
                        format!(
                            "loop start {s} is not before the last frame {}",
                            pcm.len() - 1
                        ),
                    ));
                }
                Some(s)
            }
            (None, Some(i)) => {
                let smpl = match &wav.smpl {
                    Some(Ok(s)) => s,
                    _ => return Err(bad("/smpl_loop", "the WAV has no usable smpl chunk")),
                };
                let l = smpl.loops.get(i).ok_or_else(|| {
                    bad(
                        "/smpl_loop",
                        format!("the smpl chunk has {} loops", smpl.loops.len()),
                    )
                })?;
                if let Some(p) = crate::sample_codec::smpl_loop_problem(l, pcm.len()) {
                    return Err(bad("/smpl_loop", format!("smpl loop {i} {p}")));
                }
                Some(l.start as usize)
            }
            (None, None) => None,
        };
        let encoding = match input.encoding {
            NdsEncoding::Pcm8 => Encoding::Pcm8,
            NdsEncoding::Pcm16 => Encoding::Pcm16,
            NdsEncoding::ImaAdpcm => Encoding::ImaAdpcm,
        };
        let unit = encoding.samples_per_word();
        let mut padding = 0;
        if let Some(start) = loop_start {
            let length = pcm.len() - start;
            if start % unit != 0 || length % unit != 0 {
                let lower = start / unit * unit;
                let fitting: Vec<String> = [lower, lower + unit]
                    .iter()
                    .filter(|&&s| s < pcm.len() && (pcm.len() - s) % unit == 0)
                    .map(|s| s.to_string())
                    .collect();
                return Err(bad(
                    if input.smpl_loop.is_some() {
                        "/smpl_loop"
                    } else {
                        "/loop_start_frame"
                    },
                    format!(
                        "{} loops start and repeat in whole 32-bit words ({unit} samples): loop start {start} and loop length {length} must both be multiples of {unit}. Fitting loop starts for this WAV's {} frames: {}; otherwise trim the WAV so the loop length is a multiple of {unit}, or write a one-shot",
                        encoding.name(),
                        pcm.len(),
                        if fitting.is_empty() {
                            "none (the frame count is not a whole number of words from any nearby start)".to_owned()
                        } else {
                            fitting.join(", ")
                        }
                    ),
                ));
            }
        } else if pcm.len() % unit != 0 {
            padding = unit - pcm.len() % unit;
            pcm.extend(std::iter::repeat_n(0, padding));
        }
        // Encode the new sample block.
        let (data, decoded) = match encoding {
            Encoding::Pcm8 => {
                let d = nds_adpcm::encode_pcm8(&pcm);
                let back = nds_adpcm::decode_pcm8(&d);
                (d, back)
            }
            Encoding::Pcm16 => {
                let d = nds_adpcm::encode_pcm16(&pcm);
                let back = nds_adpcm::decode_pcm16(&d);
                (d, back)
            }
            Encoding::ImaAdpcm => {
                let e = nds_adpcm::encode(&pcm);
                (e.bytes, e.decoded)
            }
        };
        let words = data.len() / 4;
        let loop_words = match (loop_start, encoding) {
            (Some(s), Encoding::ImaAdpcm) => 1 + s / 8,
            (Some(s), e) => s / e.samples_per_word(),
            (None, Encoding::ImaAdpcm) => 1,
            (None, _) => 0,
        };
        if loop_words > u16::MAX as usize {
            return Err(bad(
                "/loop_start_frame",
                format!("the loop starts at word {loop_words}; the SWAV loop offset is 16-bit"),
            ));
        }
        let timer = if rate == old.sample_rate as u32 {
            old.timer
        } else {
            (crate::nds_sound::ARM7_CLOCK_HALF_HZ / rate as f64).floor() as u16
        };
        let mut block = vec![encoding.raw(), loop_start.is_some() as u8];
        block.extend((rate as u16).to_le_bytes());
        block.extend(timer.to_le_bytes());
        block.extend((loop_words as u16).to_le_bytes());
        block.extend(((words - loop_words) as u32).to_le_bytes());
        block.extend(&data);
        debug_assert_eq!(block.len() % 4, 0);
        // Losses.
        let mut changes = serde_json::Map::new();
        for (k, v) in interpreted.changes {
            changes.insert(k, v);
        }
        let errors: Vec<i64> = pcm
            .iter()
            .zip(&decoded)
            .map(|(&a, &b)| (b - a) as i64)
            .collect();
        let changed = errors.iter().filter(|e| **e != 0).count();
        if changed > 0 {
            let first = errors.iter().position(|e| *e != 0);
            changes.insert("sample_values".into(), json!({"changed_samples":changed,"max_absolute_error":errors.iter().map(|e| e.abs()).max(),
                "rms_error":(errors.iter().map(|e| (e * e) as f64).sum::<f64>() / errors.len() as f64).sqrt(),"first_changed_sample":first}));
        }
        if padding > 0 {
            changes.insert("padding".into(), json!({"zero_samples_appended":padding,"reason":format!("{} one-shots end on a whole 32-bit word", encoding.name())}));
        }
        let unaccepted: Vec<&String> = changes
            .keys()
            .filter(|k| !input.accepted_changes.contains(k))
            .collect();
        if !unaccepted.is_empty() {
            let diagnostics = unaccepted
                .iter()
                .map(|k| Diagnostic {
                    code: Code::TargetRefused,
                    location: "/accepted_changes".into(),
                    message: format!(
                        "{k}: {}; list it in accepted_changes to proceed",
                        changes[*k]
                    ),
                })
                .collect();
            let mut r = ToolResponse::blocked(
                format!(
                    "refused: change(s) not accepted ({}); nothing was written",
                    unaccepted
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                diagnostics,
            );
            r.result = json!({"written":false,"changes":changes});
            return Err(r);
        }
        // Rebuild the archive.
        let (old_start, old_end) = (old.info_offset, old.block_end());
        let delta = block.len() as i64 - (old_end - old_start) as i64;
        let file_end = swar.header.end();
        let mut out = file[..old_start].to_vec();
        out.extend(&block);
        out.extend(&file[old_end..]);
        let shift = |v: u32| (v as i64 + delta) as u32;
        let mut header_changes = vec![];
        let mut set32 = |out: &mut Vec<u8>, at: usize, value: u32, field: String| {
            let before = u32::from_le_bytes(out[at..at + 4].try_into().expect("4 bytes"));
            if before != value {
                out[at..at + 4].copy_from_slice(&value.to_le_bytes());
                header_changes
                    .push(json!({"offset":at,"field":field,"before":before,"after":value}));
            }
        };
        set32(
            &mut out,
            8,
            shift(swar.header.file_size),
            "file_size".into(),
        );
        set32(
            &mut out,
            swar.data_block + 4,
            shift(swar.data_size),
            "data_size".into(),
        );
        for (j, &o) in swar.offsets.iter().enumerate() {
            if o as usize >= old_end {
                set32(
                    &mut out,
                    swar.table + 4 * j,
                    shift(o),
                    format!("offset[{j}]"),
                );
            }
        }
        let new_end = (file_end as i64 + delta) as usize;
        // Readback.
        let parsed = Swar::parse(&out)
            .map_err(|e| ToolResponse::error(format!("the written SWAR does not parse: {e}")))?;
        let new_wave = parsed.waves[input.wave]
            .clone()
            .map_err(|e| ToolResponse::error(format!("the written wave does not parse: {e}")))?;
        let back = decode_wave(&out, &new_wave)
            .map_err(|e| ToolResponse::error(format!("the written wave does not decode: {e}")))?;
        if back.pcm != decoded || back.loop_start != loop_start {
            return Err(ToolResponse::error(
                "the written wave decodes differently from the encoder's readback",
            ));
        }
        let mut unchanged = 0;
        for (j, (a, b)) in swar.waves.iter().zip(&parsed.waves).enumerate() {
            if j == input.wave {
                continue;
            }
            match (a, b) {
                (Ok(a), Ok(b))
                    if file[a.info_offset..a.block_end()] == out[b.info_offset..b.block_end()] =>
                {
                    unchanged += 1
                }
                (Err(_), _) => {}
                _ => {
                    return Err(ToolResponse::error(format!(
                        "wave {j} changed during replacement"
                    )));
                }
            }
        }
        // Every byte outside the replaced block and the changed header fields is identical.
        let changed_offsets: std::collections::BTreeSet<usize> = header_changes
            .iter()
            .flat_map(|c| {
                let at = c["offset"].as_u64().expect("offset") as usize;
                at..at + 4
            })
            .collect();
        let before_ok = (0..old_start).all(|i| changed_offsets.contains(&i) || file[i] == out[i]);
        let after_ok = file[old_end..] == out[old_start + block.len()..];
        if !before_ok || !after_ok {
            return Err(ToolResponse::error(
                "bytes outside the replaced block changed",
            ));
        }
        let mut seam = Value::Null;
        if let Some(s) = loop_start {
            let last = *back.pcm.last().expect("samples");
            seam = json!({"last_sample":last,"loop_start_sample":back.pcm[s],"step_at_seam":back.pcm[s] - last});
        }
        let report = json!({
            "source": input.source, "member": input.member, "wave": input.wave,
            "replacement": {"source": input.replacement, "channel": channel, "sample_rate": rate, "frames": pcm.len() - padding},
            "old_wave": old.report(), "new_wave": new_wave.report(), "codec": back.codec,
            "loop": {"start_sample": loop_start, "seam": seam},
            "changes": changes, "accepted_changes": input.accepted_changes,
            "storage": {"old_block": {"offset": old_start, "end": old_end}, "new_block": {"offset": old_start, "end": old_start + block.len()},
                "delta_bytes": delta, "header_changes": header_changes, "file_size": {"before": file_end, "after": new_end},
                "following_blocks_moved": swar.offsets.iter().filter(|&&o| o as usize >= old_end).count()},
            "readback": {"decoded": readback(&back.pcm), "unchanged_waves_identical": unchanged, "other_bytes_identical": true,
                "parsed_waves": parsed.waves.iter().filter(|w| w.is_ok()).count()},
            "timer": {"value": timer, "kept_from_original": rate == old.sample_rate as u32, "rule": "floor(ARM7_CLOCK / 2 / rate) when the rate changes"},
            "files": {"swar": name, "report": "replace.json"},
            "scope": "a standalone SWAR copy; an SDAT, its INFO/FAT and any SBNK note definitions are not rebuilt; no in-game playback",
        });
        let report_bytes = serde_json::to_vec_pretty(&report).expect("serializes");
        let mut response = self.export_sound_files(
            &input.job_id,
            &input.destination,
            vec![
                (name.into(), out.clone()),
                ("replace.json".into(), report_bytes.clone()),
            ],
        )?;
        let hash = sha256_hex(&out);
        let store = self.store_mut(true)?;
        store.put_object(&out)?;
        let report_hash = store.put_object(&report_bytes)?;
        let record = SoundSourceRecord {
            source: hash.clone(),
            name: name.into(),
            bytes: out.len(),
            derived_from: Some(
                json!({"source":input.source,"member":input.member,"tool":"replace_nds_wave","job_id":input.job_id,"wave":input.wave,"report_sha256":report_hash}),
            ),
        };
        response.summary = format!(
            "replaced wave {} with {} {} samples ({}); {} other wave(s) byte-identical; derived SWAR {}",
            input.wave,
            back.pcm.len(),
            encoding.name(),
            if changes.is_empty() {
                "exact".to_owned()
            } else {
                changes.keys().cloned().collect::<Vec<_>>().join(", ")
            },
            unchanged,
            &hash[..12]
        );
        let mut result = report;
        result["derived_source"] = json!(hash);
        result["original_unchanged"] = json!(true);
        result["next"] = json!(
            "inspect_nds_sound reads the new SWAR back; export_nds_sample decodes the replaced wave"
        );
        response.result = result;
        response.artifacts.push(Artifact {
            kind: "derived_nds_swar_source".into(),
            path: store.object_path(&hash).display().to_string(),
            sha256: hash.clone(),
        });
        commit(response, move |writer| {
            writer.put_document("", SOURCES, &hash, &record)
        })
    }
}

/// The `nds_sound` capability section.
pub(super) fn capability() -> Value {
    json!({
        "tools": ["load_instrument_source", "inspect_nds_sound", "export_nds_sample", "replace_nds_wave", "prepare_sample"],
        "files": "SDAT (SYMB names, INFO sequence/bank/wave-archive/stream entries, FAT members interpreted in place with member), SWAR, standalone SWAV, SBNK, STRM; SSEQ and SSAR headers only",
        "header": "16-byte ID, byte order FEFFh (stored FF FE; others refused), version (0100h documented, others reported), file size (truncation refused, trailing bytes reported), header size, block count",
        "waves": "12-byte info: type 0 PCM8, 1 PCM16, 2 IMA-ADPCM (others refused), loop flag 0/1, rate, timer (rate_hz = 16756991 / timer reported with its cents difference), loop offset and length in 32-bit words; IMA-ADPCM samples = 8 * (words - 1) after the header word, loop start = 8 * (loop words - 1); shared, overlapping blocks and unreferenced ranges reported",
        "ima_adpcm": "GBATEK DS variant: header PCM16 bits 0-15 and index bits 16-22 (above 88 refused, bits 23-31 reported), low nibble first, difference as the documented sum of shifted steps, clamp to -7FFFh..7FFFh; at a loop the hardware reloads the first-pass PCM16 and index at the loop start",
        "instruments": "SBNK records: type 0 empty, 1-15 one note definition, 16 drum set (low..high, two-byte unknown per entry), 17 key split (up to eight region ends); definitions: SWAV index, wave-archive slot 0-3, base note, attack, decay, sustain, release, pan; types above 17 retained but not interpreted; inside an SDAT the slot resolves through the INFO bank entry to a FAT member",
        "streams": "STRM HEAD/DATA: type, loop flag and loop sample, 1-2 channels interleaved per block, block/last-block bytes and samples checked against each other (IMA-ADPCM blocks each start with a header), declared sample count checked",
        "export": "export_nds_sample: original.bin (exact), decoded.wav (mono PCM16 at the header rate, smpl loop when looped), optional audition.wav with 1-16 baked loop traversals (at most 600 s), report.json; decoded.wav is retained as a source for prepare_sample (encoding wav, wav.smpl_loop 0); PCM8/PCM16 waves are also direct prepare_sample slices (s8/s16le)",
        "replacement": "replace_nds_wave: a standalone SWAR copy with one wave re-encoded from a retained WAV at its rate; loops need whole-word starts and lengths (4/2/8 samples for PCM8/PCM16/IMA-ADPCM), one-shots are zero-padded to a word (padding); following blocks move and their offsets, the file size and the DATA size change; shared or overlapping blocks refuse; sample_values, bit_depth_reduction and padding must be accepted",
        "not_modelled": "DS mixer, channel volume/pan/hold, interpolation, SBNK envelopes and pitch, SSEQ/SSAR sequence playback, SDAT rebuilding and in-game insertion"
    })
}
