//! Loading a source is independent of musical projection and target mapping.
//! Keep every byte, expose every decoded event and retain opaque chunk data.

use super::*;
use midly::SmfBytemap;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct ChunkRange {
    pub id: [u8; 4],
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone)]
pub struct SourceLayout {
    pub smf_offset: usize,
    pub smf_length: usize,
    pub chunks: Vec<ChunkRange>,
    pub riff: Option<(usize, usize)>, // data chunk header, RIFF end
}

fn invalid(message: impl Into<String>) -> Vec<Diagnostic> {
    vec![Diagnostic {
        code: Code::UnsupportedMidi,
        location: "/source".into(),
        message: message.into(),
    }]
}

fn chunk_end(offset: usize, size: usize, bound: usize) -> Result<usize, Vec<Diagnostic>> {
    offset
        .checked_add(8)
        .and_then(|n| n.checked_add(size))
        .filter(|n| *n <= bound)
        .ok_or_else(|| invalid("chunk length exceeds its container"))
}

/// Locate SMF and RIFF chunks without dropping unrecognized bytes.
pub fn source_layout(bytes: &[u8]) -> Result<SourceLayout, Vec<Diagnostic>> {
    let (smf_offset, smf_length, riff) = if bytes.starts_with(b"RIFF") {
        if bytes.len() < 12 || &bytes[8..12] != b"RMID" {
            return Err(invalid("expected a RIFF RMID form"));
        }
        let end = chunk_end(
            0,
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len(),
        )?;
        let mut at = 12;
        let mut data = None;
        while at < end {
            if at + 8 > end {
                return Err(invalid("incomplete RIFF chunk header"));
            }
            let length = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            let next = chunk_end(at, length, end)?;
            if &bytes[at..at + 4] == b"data" && data.is_none() {
                data = Some((at, length));
            }
            at = next + length % 2;
            if at > end {
                return Err(invalid("missing RIFF alignment byte"));
            }
        }
        let (at, length) = data.ok_or_else(|| invalid("RIFF RMID contains no MIDI data chunk"))?;
        (at + 8, length, Some((at, end)))
    } else {
        (0, bytes.len(), None)
    };
    if bytes.get(smf_offset..smf_offset + 4) != Some(b"MThd") {
        return Err(invalid("expected an SMF header"));
    }
    let end = smf_offset + smf_length;
    let mut at = smf_offset;
    let mut chunks = Vec::new();
    while at < end {
        if at + 8 > end {
            return Err(invalid("incomplete SMF chunk header"));
        }
        let id = bytes[at..at + 4].try_into().unwrap();
        let length = u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let next = chunk_end(at, length, end)?;
        chunks.push(ChunkRange {
            id,
            offset: at,
            length,
        });
        at = next;
    }
    Ok(SourceLayout {
        smf_offset,
        smf_length,
        chunks,
        riff,
    })
}

fn range(bytes: &[u8], part: &[u8]) -> Value {
    let offset = part.as_ptr() as usize - bytes.as_ptr() as usize;
    json!({"offset": offset, "length": part.len()})
}

fn payload(bytes: &[u8], part: &[u8]) -> Value {
    json!({"range": range(bytes, part), "sha256": crate::store::sha256_hex(part),
        "bytes": &part[..part.len().min(64)], "complete": part.len() <= 64,
        "utf8": std::str::from_utf8(part).ok().filter(|_| part.len() <= 256)})
}

fn timing(timing: Timing) -> Value {
    match timing {
        Timing::Metrical(ppq) => json!({"kind": "metrical", "ticks_per_quarter": ppq.as_int()}),
        Timing::Timecode(fps, subframe) => {
            let (numerator, denominator) = match fps {
                midly::Fps::Fps24 => (24, 1),
                midly::Fps::Fps25 => (25, 1),
                midly::Fps::Fps29 => (30000, 1001),
                midly::Fps::Fps30 => (30, 1),
            };
            json!({"kind": "smpte", "frames_per_second": {"numerator": numerator, "denominator": denominator}, "ticks_per_frame": subframe})
        }
    }
}

fn event_value(bytes: &[u8], kind: TrackEventKind<'_>) -> Value {
    match kind {
        TrackEventKind::Midi { channel, message } => {
            let mut value = match message {
                MidiMessage::NoteOn { key, vel } => {
                    json!({"kind": "note_on", "key": key.as_int(), "velocity": vel.as_int()})
                }
                MidiMessage::NoteOff { key, vel } => {
                    json!({"kind": "note_off", "key": key.as_int(), "velocity": vel.as_int()})
                }
                MidiMessage::Aftertouch { key, vel } => {
                    json!({"kind": "poly_aftertouch", "key": key.as_int(), "pressure": vel.as_int()})
                }
                MidiMessage::Controller { controller, value } => {
                    json!({"kind": "controller", "controller": controller.as_int(), "value": value.as_int()})
                }
                MidiMessage::ProgramChange { program } => {
                    json!({"kind": "program_change", "program": program.as_int()})
                }
                MidiMessage::ChannelAftertouch { vel } => {
                    json!({"kind": "channel_aftertouch", "pressure": vel.as_int()})
                }
                MidiMessage::PitchBend { bend } => {
                    json!({"kind": "pitch_bend", "value": bend.as_int()})
                }
            };
            value["channel"] = json!(channel.as_int());
            value
        }
        TrackEventKind::SysEx(data) => json!({"kind": "sysex", "payload": payload(bytes, data)}),
        TrackEventKind::Escape(data) => json!({"kind": "escape", "payload": payload(bytes, data)}),
        TrackEventKind::Meta(meta) => match meta {
            MetaMessage::TrackNumber(number) => json!({"kind": "track_number", "number": number}),
            MetaMessage::MidiChannel(channel) => {
                json!({"kind": "channel_prefix", "channel": channel.as_int()})
            }
            MetaMessage::MidiPort(port) => json!({"kind": "port", "port": port.as_int()}),
            MetaMessage::EndOfTrack => json!({"kind": "end_of_track"}),
            MetaMessage::Tempo(value) => {
                json!({"kind": "tempo", "microseconds_per_quarter": value.as_int()})
            }
            MetaMessage::TimeSignature(n, d, c, t) => {
                json!({"kind": "time_signature", "numerator": n, "denominator_power": d, "clocks_per_click": c, "thirty_seconds_per_quarter": t})
            }
            MetaMessage::KeySignature(sharps, minor) => {
                json!({"kind": "key_signature", "sharps": sharps, "minor": minor})
            }
            MetaMessage::SmpteOffset(value) => {
                json!({"kind": "smpte_offset", "hours": value.hour(), "minutes": value.minute(), "seconds": value.second(), "frames": value.frame(), "subframes": value.subframe(), "frame_rate": format!("{:?}", value.fps())})
            }
            MetaMessage::Unknown(id, data) => {
                json!({"kind": "unknown_meta", "type": id, "payload": payload(bytes, data)})
            }
            other => {
                let (kind, data) = match other {
                    MetaMessage::Text(data) => ("text", data),
                    MetaMessage::Copyright(data) => ("copyright", data),
                    MetaMessage::TrackName(data) => ("track_name", data),
                    MetaMessage::InstrumentName(data) => ("instrument_name", data),
                    MetaMessage::Lyric(data) => ("lyric", data),
                    MetaMessage::Marker(data) => ("marker", data),
                    MetaMessage::CuePoint(data) => ("cue_point", data),
                    MetaMessage::ProgramName(data) => ("program_name", data),
                    MetaMessage::DeviceName(data) => ("device_name", data),
                    MetaMessage::SequencerSpecific(data) => ("sequencer_specific", data),
                    _ => unreachable!("fixed metadata handled above"),
                };
                json!({"kind": kind, "payload": payload(bytes, data)})
            }
        },
    }
}

pub fn source_summary(bytes: &[u8]) -> Result<Value, Vec<Diagnostic>> {
    let layout = source_layout(bytes)?;
    let smf = SmfBytemap::parse(bytes).map_err(|e| invalid(e.to_string()))?;
    let tracks: Vec<_> = smf.tracks.iter().enumerate().map(|(index, events)| {
        let mut kinds = BTreeMap::<String, usize>::new();
        for (_, event) in events {
            let kind = event_value(bytes, event.kind)["kind"].as_str().unwrap().to_owned();
            *kinds.entry(kind).or_default() += 1;
        }
        json!({"track": index, "events": events.len(), "end_tick": events.iter().map(|(_, e)| u64::from(e.delta.as_int())).sum::<u64>(), "event_kinds": kinds})
    }).collect();
    Ok(
        json!({"source": crate::store::sha256_hex(bytes), "bytes": bytes.len(), "container": if layout.riff.is_some() {"rmid"} else {"smf"},
        "format": match smf.header.format {Format::SingleTrack => 0, Format::Parallel => 1, Format::Sequential => 2},
        "timing": timing(smf.header.timing), "tracks": tracks, "events": smf.tracks.iter().map(Vec::len).sum::<usize>(),
        "chunks": layout.chunks.iter().map(|c| json!({"id": String::from_utf8_lossy(&c.id), "offset": c.offset, "length": c.length + 8, "payload_offset": c.offset+8, "payload_length": c.length})).collect::<Vec<_>>(),
        "preservation": "all original bytes retained, including RIFF wrappers, extension chunks, running status, metadata payloads and event order",
        "next_action": "inspect_midi pages every event or raw byte range. export_midi_source writes exact bytes. import_midi is an optional editable projection; target mapping is a separate choice."}),
    )
}

pub fn source_events(
    bytes: &[u8],
    selected_track: Option<usize>,
    offset: usize,
    limit: usize,
) -> Result<Value, Vec<Diagnostic>> {
    let layout = source_layout(bytes)?;
    let smf = SmfBytemap::parse(bytes).map_err(|e| invalid(e.to_string()))?;
    if selected_track.is_some_and(|track| track >= smf.tracks.len()) {
        return Err(invalid("track index is outside this source"));
    }
    let chunks: Vec<_> = layout.chunks.iter().filter(|c| &c.id == b"MTrk").collect();
    let total: usize = smf
        .tracks
        .iter()
        .enumerate()
        .filter(|(i, _)| selected_track.is_none_or(|track| track == *i))
        .map(|(_, events)| events.len())
        .sum();
    let mut ordinal = 0;
    let mut page = Vec::new();
    for (track, events) in smf.tracks.iter().enumerate() {
        if selected_track.is_some_and(|selected| selected != track) {
            continue;
        }
        let mut tick = 0;
        let mut previous_end = chunks[track].offset + 8;
        for (index, (raw, event)) in events.iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            let message_offset = raw.as_ptr() as usize - bytes.as_ptr() as usize;
            let end = message_offset + raw.len();
            if ordinal >= offset && page.len() < limit {
                page.push(json!({"id": format!("t{track}:e{index}"), "track": track, "index": index,
                    "delta": event.delta.as_int(), "tick": tick, "message": event_value(bytes, event.kind),
                    "encoded_range": {"offset": previous_end, "length": end - previous_end},
                    "message_range": {"offset": message_offset, "length": raw.len()},
                    "encoded_sha256": crate::store::sha256_hex(&bytes[previous_end..end])}));
            }
            previous_end = end;
            ordinal += 1;
        }
    }
    Ok(
        json!({"events": page, "total_events": total, "offset": offset, "next_cursor": (offset + page.len() < total).then_some(offset + page.len()),
        "order": "original track order, then original event order; same-tick events are never merged"}),
    )
}

/// Replace only track payloads, preserving all other SMF/RIFF bytes.
pub fn replace_source_tracks(bytes: &[u8], tracks: &[Vec<u8>]) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let layout = source_layout(bytes)?;
    let mut smf = Vec::new();
    let mut index = 0;
    for chunk in &layout.chunks {
        if &chunk.id == b"MTrk" {
            let body = tracks
                .get(index)
                .ok_or_else(|| invalid("track count changed"))?;
            let length = u32::try_from(body.len()).map_err(|_| invalid("track is too large"))?;
            smf.extend(b"MTrk");
            smf.extend(length.to_be_bytes());
            smf.extend(body);
            index += 1;
        } else {
            smf.extend(&bytes[chunk.offset..chunk.offset + 8 + chunk.length]);
        }
    }
    if index != tracks.len() {
        return Err(invalid("track count changed"));
    }
    if let Some((data_header, riff_end)) = layout.riff {
        let old_end = layout.smf_offset + layout.smf_length;
        let old_padded_end = old_end + layout.smf_length % 2;
        let mut out = bytes[..data_header + 4].to_vec();
        out.extend(
            u32::try_from(smf.len())
                .map_err(|_| invalid("SMF is too large"))?
                .to_le_bytes(),
        );
        out.extend(&smf);
        if smf.len() % 2 == 1 {
            out.push(if layout.smf_length % 2 == 1 {
                bytes[old_end]
            } else {
                0
            });
        }
        out.extend(&bytes[old_padded_end..riff_end]);
        let length = u32::try_from(out.len() - 8).map_err(|_| invalid("RIFF is too large"))?;
        out[4..8].copy_from_slice(&length.to_le_bytes());
        out.extend(&bytes[riff_end..]);
        Ok(out)
    } else {
        Ok(smf)
    }
}

fn varlen(mut number: u32, out: &mut Vec<u8>) {
    let mut buffer = [0u8; 4];
    let mut at = 3;
    buffer[at] = (number & 0x7f) as u8;
    while {
        number >>= 7;
        number != 0
    } {
        at -= 1;
        buffer[at] = (number as u8 & 0x7f) | 0x80;
    }
    out.extend(&buffer[at..]);
}

fn encoded_message(kind: TrackEventKind<'_>) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let smf = Smf {
        header: Header::new(Format::SingleTrack, Timing::Metrical(u15::new(480))),
        tracks: vec![vec![TrackEvent {
            delta: u28::new(0),
            kind,
        }]],
    };
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes)
        .map_err(|e| invalid(e.to_string()))?;
    // Fixed six-byte SMF header, track header, then one-byte zero delta.
    Ok(bytes[23..].to_vec())
}

pub(super) fn meta_payload(raw: &[u8]) -> Option<(u8, &[u8])> {
    if raw.first() != Some(&0xff) {
        return None;
    }
    let id = *raw.get(1)?;
    let mut length = 0usize;
    for index in 2..6 {
        let byte = *raw.get(index)?;
        length = (length << 7) | (byte & 0x7f) as usize;
        if byte & 0x80 == 0 {
            return Some((id, raw.get(index + 1..index + 1 + length)?));
        }
    }
    None
}

fn status(kind: TrackEventKind<'_>) -> Option<u8> {
    match kind {
        TrackEventKind::Midi { channel, message } => Some(
            channel.as_int()
                | match message {
                    MidiMessage::NoteOff { .. } => 0x80,
                    MidiMessage::NoteOn { .. } => 0x90,
                    MidiMessage::Aftertouch { .. } => 0xa0,
                    MidiMessage::Controller { .. } => 0xb0,
                    MidiMessage::ProgramChange { .. } => 0xc0,
                    MidiMessage::ChannelAftertouch { .. } => 0xd0,
                    MidiMessage::PitchBend { .. } => 0xe0,
                },
        ),
        _ => None,
    }
}

/// Preserve original message encodings and all metadata extension bytes. Delta
/// encodings may change. Insert a status byte only when a running-status event
/// would otherwise inherit the wrong status after an intentional edit.
pub(super) fn write_source_tracks(
    bytes: &[u8],
    tracks: &[Vec<TrackEvent<'_>>],
) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let source = SmfBytemap::parse(bytes).map_err(|e| invalid(e.to_string()))?;
    if source.tracks.len() != tracks.len() {
        return Err(invalid("track count changed"));
    }
    let mut encoded = Vec::new();
    for (original, output) in source.tracks.iter().zip(tracks) {
        let mut at = 0u64;
        let originals: Vec<_> = original
            .iter()
            .map(|(raw, event)| {
                at += u64::from(event.delta.as_int());
                (at, *raw, event.kind)
            })
            .collect();
        let mut by_event: HashMap<_, std::collections::VecDeque<usize>> = HashMap::new();
        for (index, (tick, _, kind)) in originals.iter().enumerate() {
            by_event.entry((*tick, *kind)).or_default().push_back(index);
        }
        let mut used = vec![false; originals.len()];
        let mut tick = 0;
        let mut body = Vec::new();
        let mut running = None;
        for event in output {
            tick += u64::from(event.delta.as_int());
            varlen(event.delta.as_int(), &mut body);
            let exact = by_event.get_mut(&(tick, event.kind)).and_then(|queue| {
                while queue.front().is_some_and(|index| used[*index]) {
                    queue.pop_front();
                }
                queue.pop_front()
            });
            let original = exact.or_else(|| {
                if let TrackEventKind::Meta(new) = event.kind {
                    originals.iter().enumerate().position(|(i, (at, _, kind))| {
                        !used[i]
                            && (*at == tick || matches!(new, MetaMessage::EndOfTrack))
                            && match kind {
                                TrackEventKind::Meta(old) => {
                                    std::mem::discriminant(old) == std::mem::discriminant(&new)
                                }
                                _ => false,
                            }
                    })
                } else {
                    None
                }
            });
            let raw = if let Some(index) = original {
                used[index] = true;
                let (_, raw, kind) = originals[index];
                if kind == event.kind {
                    raw.to_vec()
                } else {
                    let encoded = encoded_message(event.kind)?;
                    if let (Some((id, old)), Some((new_id, new))) =
                        (meta_payload(raw), meta_payload(&encoded))
                    {
                        if id == new_id && old.len() > new.len() {
                            let mut preserved = vec![0xff, id];
                            varlen(old.len() as u32, &mut preserved);
                            preserved.extend(new);
                            preserved.extend(&old[new.len()..]);
                            preserved
                        } else {
                            encoded
                        }
                    } else {
                        encoded
                    }
                }
            } else {
                encoded_message(event.kind)?
            };
            let next_status = status(event.kind);
            if raw.first().is_some_and(|byte| *byte < 0x80) && running != next_status {
                body.push(
                    next_status.ok_or_else(|| invalid("non-channel event has no status byte"))?,
                );
            }
            body.extend(raw);
            running = next_status;
        }
        encoded.push(body);
    }
    replace_source_tracks(bytes, &encoded)
}

/// Container bytes outside track streams; track sizes and RIFF data sizes are
/// derived encoding fields, so they are excluded from semantic comparison.
pub(super) fn container_data(bytes: &[u8]) -> Result<Vec<Vec<u8>>, Vec<Diagnostic>> {
    let layout = source_layout(bytes)?;
    let mut parts = vec![if layout.riff.is_some() {
        b"RMID".to_vec()
    } else {
        b"SMF".to_vec()
    }];
    for chunk in &layout.chunks {
        if &chunk.id == b"MTrk" {
            parts.push(b"MTrk".to_vec());
        } else {
            parts.push(bytes[chunk.offset..chunk.offset + 8 + chunk.length].to_vec());
        }
    }
    if let Some((data, end)) = layout.riff {
        let mut at = 12;
        while at < end {
            let length = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            let next = at + 8 + length + length % 2;
            if at == data {
                parts.push(b"data".to_vec());
            } else {
                parts.push(bytes[at..next].to_vec());
            }
            at = next;
        }
        parts.push(bytes[end..].to_vec());
    }
    Ok(parts)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedMidi {
    pub source: String,
    pub offset: usize,
    pub length: usize,
}

/// Find structurally valid MIDI streams in an otherwise opaque container.
/// No filename, game, index-table layout or song mapping is assumed. A child's
/// extent ends after the declared final track; all other bytes stay in parent.
pub fn embedded_midi(bytes: &[u8]) -> Vec<EmbeddedMidi> {
    fn extent(bytes: &[u8], start: usize) -> Option<usize> {
        if bytes.get(start..start + 4)? == b"RIFF" && bytes.get(start + 8..start + 12)? == b"RMID" {
            let size =
                u32::from_le_bytes(bytes.get(start + 4..start + 8)?.try_into().ok()?) as usize;
            return start
                .checked_add(8)?
                .checked_add(size)
                .filter(|end| *end <= bytes.len());
        }
        if bytes.get(start..start + 4)? != b"MThd" {
            return None;
        }
        let header_length =
            u32::from_be_bytes(bytes.get(start + 4..start + 8)?.try_into().ok()?) as usize;
        if header_length < 6 {
            return None;
        }
        let count = u16::from_be_bytes(bytes.get(start + 10..start + 12)?.try_into().ok()?);
        if count == 0 {
            return None;
        }
        let mut at = chunk_end(start, header_length, bytes.len()).ok()?;
        let mut tracks = 0;
        while tracks < count {
            let id = bytes.get(at..at + 4)?;
            // A new header denotes another stream, not a continuation.
            if id == b"MThd" {
                return None;
            }
            let length = u32::from_be_bytes(bytes.get(at + 4..at + 8)?.try_into().ok()?) as usize;
            let end = chunk_end(at, length, bytes.len()).ok()?;
            if id == b"MTrk" {
                tracks += 1;
            }
            at = end;
        }
        Some(at)
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start + 12 <= bytes.len() {
        if matches!(bytes.get(start..start + 4), Some(b"MThd" | b"RIFF"))
            && let Some(end) = extent(bytes, start)
            && source_summary(&bytes[start..end]).is_ok()
        {
            result.push(EmbeddedMidi {
                source: crate::store::sha256_hex(&bytes[start..end]),
                offset: start,
                length: end - start,
            });
            start = end;
        } else {
            start += 1;
        }
    }
    result
}

pub fn container_summary(bytes: &[u8], children: &[EmbeddedMidi]) -> Value {
    let mut retained = Vec::new();
    let mut previous = 0;
    for child in children {
        if child.offset > previous {
            retained.push(json!({"offset": previous, "length": child.offset-previous}));
        }
        previous = child.offset + child.length;
    }
    if previous < bytes.len() {
        retained.push(json!({"offset": previous, "length": bytes.len()-previous}));
    }
    json!({"source": crate::store::sha256_hex(bytes), "bytes": bytes.len(), "container": "opaque_with_embedded_midi",
        "children": children, "uninterpreted_ranges": retained,
        "preservation": "the complete parent and each discovered MIDI stream are retained byte-exactly",
        "boundary_rule": "each SMF child ends after its final declared track; RIFF children use their declared size. Unclassified trailing data stays in the parent. No archive index semantics are inferred.",
        "next_action": "inspect_midi with a child source ID for its headers and events; use the parent source ID for raw bytes or exact container export. Mapping is optional."})
}
