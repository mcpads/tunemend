//! PlayStation SEQ sequences and SEP multi-sequence containers: header,
//! event stream with raw offsets, NRPN conventions (loops, marks, VAB
//! attributes) and export of a sequence as a Standard MIDI File.
//!
//! Interpreted from public descriptions only (psx-spx and a community format
//! note) and readings observed in real files; inferred meanings are labeled.
//! Multi-byte header fields are big-endian.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAGIC: &[u8; 4] = b"pQES";
/// SEQ header bytes before the score data.
pub const SEQ_HEADER_BYTES: usize = 0x0F;
/// SEP file header bytes (magic and 16-bit version) before the first sequence.
pub const SEP_HEADER_BYTES: usize = 0x06;
/// SEP per-sequence header bytes before the score data.
pub const SEP_SEQUENCE_HEADER_BYTES: usize = 0x0D;
/// Most events interpreted in one file.
pub const MAX_EVENTS: usize = 1 << 20;
/// NRPN MSB values with a documented sequence meaning.
pub const NRPN_LOOP_START: u8 = 20;
pub const NRPN_LOOP_END: u8 = 30;
pub const NRPN_MARK: u8 = 40;
/// Loop count meaning endless repetition.
pub const ENDLESS: u8 = 127;
pub const SCOPE: &str = "PlayStation SEQ (pQES, version 1) and SEP (pQES, version 0) sequences: header, every event with its byte offset, running status, tempo and end-of-track metas without length bytes, NRPN loop/mark/VAB-attribute conventions; interpreted from public descriptions. The sound library's scheduling, voice allocation, volume and pan arithmetic and hardware playback are not established";

/// Which container a file holds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    /// Bytes 4..8 `00 00 00 01` (version 1) mean SEQ; `00 00 00 00` (16-bit
    /// version 0, then sequence ID 0) mean SEP; anything else refuses.
    #[default]
    Detect,
    /// One sequence: 15-byte header, score data, end of track.
    Seq,
    /// Several sequences, each with ID, header and data size.
    Sep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqError {
    /// Byte offset within the parsed bytes.
    pub offset: usize,
    /// Which sequence (SEP index), when inside one.
    pub sequence: Option<usize>,
    pub message: String,
}

impl std::fmt::Display for SeqError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.sequence {
            Some(s) => write!(
                f,
                "sequence {s}, byte {} (0x{:X}): {}",
                self.offset, self.offset, self.message
            ),
            None => write!(
                f,
                "byte {} (0x{:X}): {}",
                self.offset, self.offset, self.message
            ),
        }
    }
}

fn error(offset: usize, message: impl Into<String>) -> SeqError {
    SeqError {
        offset,
        sequence: None,
        message: message.into(),
    }
}

/// What an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    NoteOff {
        key: u8,
        velocity: u8,
    },
    NoteOn {
        key: u8,
        velocity: u8,
    },
    PolyPressure {
        key: u8,
        value: u8,
    },
    Control {
        controller: u8,
        value: u8,
    },
    Program {
        program: u8,
    },
    ChannelPressure {
        value: u8,
    },
    /// 14-bit value, 0x2000 centre.
    PitchBend {
        value: u16,
    },
    /// Microseconds per quarter note.
    Tempo {
        microseconds: u32,
    },
    /// The byte after FF 2F.
    End {
        byte: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Offset of the delta time within the parsed bytes.
    pub offset: usize,
    /// Bytes from the delta time through the last data byte.
    pub length: usize,
    pub delta: u32,
    pub tick: u64,
    /// Status in force (0xFF for metas).
    pub status: u8,
    /// The status byte was omitted (running status).
    pub running_status: bool,
    pub kind: Kind,
}

impl Event {
    pub fn channel(&self) -> Option<u8> {
        (self.status < 0xF0).then_some(self.status & 0x0F)
    }

    /// Note-on with velocity 0 counts as a release.
    pub fn is_release(&self) -> bool {
        matches!(
            self.kind,
            Kind::NoteOff { .. } | Kind::NoteOn { velocity: 0, .. }
        )
    }
}

/// A sequence's header fields with their offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Offset of the sequence's first header byte.
    pub start: usize,
    /// SEP sequence ID.
    pub id: Option<u16>,
    pub resolution: u16,
    pub tempo: u32,
    pub numerator: u8,
    /// Power-of-two exponent, as in SMF time signatures.
    pub denominator_exponent: u8,
    /// SEP data size from the score start through end of track.
    pub data_size: Option<u32>,
    pub score_start: usize,
    /// One past the end-of-track bytes.
    pub end: usize,
}

/// A loop section marked by NRPN 20 (start) and 30 (end).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loop {
    pub start_event: usize,
    pub count: Option<u8>,
    /// The CC6 event holding the count.
    pub count_event: Option<usize>,
    pub end_event: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Sequence {
    pub header: Header,
    pub events: Vec<Event>,
    /// Interpretation of controller events by event index.
    pub meanings: Vec<(usize, Value)>,
    pub loops: Vec<Loop>,
    /// Bytes between end of track and the declared SEP data end.
    pub gap_after_end: usize,
}

#[derive(Debug, Clone)]
pub struct SeqFile {
    pub container: Container,
    /// SEQ: 32-bit version; SEP: 16-bit version.
    pub version: u32,
    pub sequences: Vec<Sequence>,
    /// Bytes after the last interpreted sequence, retained and not interpreted.
    pub trailing: usize,
    pub bytes: usize,
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
fn be24(b: &[u8], at: usize) -> u32 {
    (b[at] as u32) << 16 | (b[at + 1] as u32) << 8 | b[at + 2] as u32
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Most container members listed by discovery.
const DISCOVERY_LISTED: usize = 64;

/// What load_instrument_source reports for SEQ/SEP bytes: the whole file, or
/// pQES headers inside a container that parse as a sequence.
pub fn discovery_hint(bytes: &[u8]) -> Option<Value> {
    if bytes.starts_with(b"SEQp") {
        return Some(json!({"format": "psx_sequence_variant", "magic": "SEQp",
            "note": "byte-reversed pQES is not described; retained but not interpreted"}));
    }
    let member = |file: &SeqFile| {
        json!({"container": file.container, "sequences": file.sequences.len(),
            "bytes": file.sequences.last().map_or(0, |s| s.header.end),
            "events": file.sequences.iter().map(|s| s.events.len()).sum::<usize>()})
    };
    if bytes.starts_with(MAGIC) {
        let mut hint = json!({"format": "psx_sequence", "load_with": "load_psx_sequence",
            "note": "pass {\"sequence\": {\"source\": <this>}}; the SEQ does not name its VAB, so load the bank (in observed games the VH/VB of the same stem) for binding and render_psx_sequence"});
        match parse(bytes, Container::Detect) {
            Ok(file) => {
                hint["header"] = member(&file);
                hint["trailing_bytes"] = json!(file.trailing);
            }
            Err(e) => hint["unusable"] = json!(e.to_string()),
        }
        return Some(hint);
    }
    let (mut found, mut count, mut at) = (Vec::new(), 0usize, 1usize);
    while let Some(i) = bytes
        .get(at..)
        .and_then(|rest| rest.windows(4).position(|w| w == MAGIC))
    {
        let offset = at + i;
        match parse(&bytes[offset..], Container::Detect) {
            Ok(file) => {
                count += 1;
                let mut row = member(&file);
                if found.len() < DISCOVERY_LISTED {
                    row["offset"] = json!(offset);
                    found.push(row);
                }
                at = offset + file.sequences.last().map_or(4, |s| s.header.end.max(4));
            }
            Err(_) => at = offset + 4,
        }
    }
    (count > 0).then(|| {
        json!({"format": "psx_sequences_inside", "load_with": "load_psx_sequence", "count": count,
            "first": found, "truncated": count > DISCOVERY_LISTED,
            "note": "pQES sequences inside this container; pass each as {\"sequence\": {\"source\": <this>, \"offset\": offset, \"length\": bytes}}"})
    })
}

/// Parses a SEQ or SEP file.
pub fn parse(bytes: &[u8], container: Container) -> Result<SeqFile, SeqError> {
    if bytes.len() < 8 {
        return Err(error(
            bytes.len(),
            format!("{} bytes are too short for a pQES header", bytes.len()),
        ));
    }
    if &bytes[0..4] != MAGIC {
        let hint = if &bytes[0..4] == b"SEQp" {
            " (byte-reversed SEQp is not described)"
        } else {
            ""
        };
        return Err(error(
            0,
            format!("magic {} is not pQES{hint}", crate::vab::hex(&bytes[0..4])),
        ));
    }
    let container = match container {
        // A SEP's 16-bit version 0 is followed by the first sequence ID 0.
        Container::Detect => match be32(bytes, 4) {
            1 => Container::Seq,
            0 => Container::Sep,
            _ => {
                return Err(error(
                    4,
                    format!(
                        "version bytes {} are neither SEQ version 1 (00 00 00 01) nor SEP version 0 with sequence ID 0 (00 00 00 00); give container seq or sep to read it anyway",
                        crate::vab::hex(&bytes[4..8])
                    ),
                ));
            }
        },
        other => other,
    };
    let mut events_total = 0usize;
    match container {
        Container::Seq => {
            if bytes.len() < SEQ_HEADER_BYTES {
                return Err(error(
                    bytes.len(),
                    "the file ends inside the 15-byte SEQ header",
                ));
            }
            let mut header = Header {
                start: 0,
                id: None,
                resolution: be16(bytes, 8),
                tempo: be24(bytes, 0x0A),
                numerator: bytes[0x0D],
                denominator_exponent: bytes[0x0E],
                data_size: None,
                score_start: SEQ_HEADER_BYTES,
                end: 0,
            };
            let sequence = parse_score(bytes, &mut header, bytes.len(), &mut events_total)?;
            let end = sequence.header.end;
            Ok(SeqFile {
                container,
                version: be32(bytes, 4),
                sequences: vec![sequence],
                trailing: bytes.len() - end,
                bytes: bytes.len(),
            })
        }
        Container::Sep | Container::Detect => {
            let mut sequences: Vec<Sequence> = Vec::new();
            let mut at = SEP_HEADER_BYTES;
            loop {
                let remaining = bytes.len() - at;
                if remaining == 0 {
                    break;
                }
                let id = (remaining >= 2).then(|| be16(bytes, at));
                let expected = sequences.len() as u16;
                if !sequences.is_empty()
                    && (remaining < SEP_SEQUENCE_HEADER_BYTES || id != Some(expected))
                {
                    break;
                }
                let index = sequences.len();
                let located = |mut e: SeqError| {
                    e.sequence = Some(index);
                    e
                };
                if remaining < SEP_SEQUENCE_HEADER_BYTES {
                    return Err(located(error(
                        bytes.len(),
                        "the file ends inside the 13-byte SEP sequence header",
                    )));
                }
                if id != Some(expected) {
                    return Err(located(error(
                        at,
                        format!(
                            "the first SEP sequence ID is {}, not 0",
                            id.unwrap_or_default()
                        ),
                    )));
                }
                let size = be32(bytes, at + 9);
                let score_start = at + SEP_SEQUENCE_HEADER_BYTES;
                let limit = score_start
                    .checked_add(size as usize)
                    .filter(|&end| end <= bytes.len())
                    .ok_or_else(|| {
                        located(error(
                            at + 9,
                            format!(
                                "data size {size} runs past the file end ({} bytes after the score start)",
                                bytes.len() - score_start
                            ),
                        ))
                    })?;
                let mut header = Header {
                    start: at,
                    id,
                    resolution: be16(bytes, at + 2),
                    tempo: be24(bytes, at + 4),
                    numerator: bytes[at + 7],
                    denominator_exponent: bytes[at + 8],
                    data_size: Some(size),
                    score_start,
                    end: 0,
                };
                let mut sequence =
                    parse_score(bytes, &mut header, limit, &mut events_total).map_err(located)?;
                sequence.gap_after_end = limit - sequence.header.end;
                sequences.push(sequence);
                at = limit;
            }
            if sequences.is_empty() {
                return Err(error(
                    SEP_HEADER_BYTES,
                    "the SEP holds no sequence after its 6-byte header",
                ));
            }
            Ok(SeqFile {
                container: Container::Sep,
                version: be16(bytes, 4) as u32,
                sequences,
                trailing: bytes.len() - at,
                bytes: bytes.len(),
            })
        }
    }
}

/// Parses events from `header.score_start` up to end of track, which must
/// lie before `limit`.
fn parse_score(
    bytes: &[u8],
    header: &mut Header,
    limit: usize,
    events_total: &mut usize,
) -> Result<Sequence, SeqError> {
    let mut pos = header.score_start;
    let mut status: Option<u8> = None;
    let mut tick = 0u64;
    let mut events = Vec::new();
    let truncated = |at: usize, what: &str| {
        error(
            at,
            format!("{what} runs past the end of the sequence data at byte {limit}"),
        )
    };
    loop {
        let offset = pos;
        if pos >= limit {
            return Err(error(
                pos,
                "the sequence data ends without end of track (FF 2F)",
            ));
        }
        let mut delta = 0u32;
        let mut ended = false;
        for i in 0..4 {
            let b = *bytes
                .get(pos)
                .filter(|_| pos < limit)
                .ok_or_else(|| truncated(offset, "the delta time"))?;
            pos += 1;
            delta = delta << 7 | (b & 0x7F) as u32;
            if b < 0x80 {
                ended = true;
                break;
            }
            if i == 3 {
                break;
            }
        }
        if !ended {
            return Err(error(
                offset,
                "a delta time longer than 4 bytes is not a MIDI variable-length quantity",
            ));
        }
        tick += delta as u64;
        if pos >= limit {
            return Err(truncated(pos, "an event"));
        }
        let first = bytes[pos];
        let running = first < 0x80;
        let current = if running {
            status.ok_or_else(|| {
                error(
                    pos,
                    format!("data byte 0x{first:02X} with no running status"),
                )
            })?
        } else {
            pos += 1;
            first
        };
        if (0xF0..0xFF).contains(&current) {
            return Err(error(
                pos - 1,
                format!("status 0x{current:02X} (system message) is not described for SEQ data"),
            ));
        }
        status = Some(current);
        let take = |pos: &mut usize, n: usize| -> Result<Vec<u8>, SeqError> {
            if *pos + n > limit {
                return Err(truncated(*pos, "an event's data bytes"));
            }
            let data = bytes[*pos..*pos + n].to_vec();
            if let Some(i) = data.iter().position(|&b| b >= 0x80)
                && current != 0xFF
            {
                return Err(error(
                    *pos + i,
                    format!(
                        "data byte 0x{:02X} of status 0x{current:02X} has its high bit set",
                        data[i]
                    ),
                ));
            }
            *pos += n;
            Ok(data)
        };
        let kind = match current & 0xF0 {
            0x80 => {
                let d = take(&mut pos, 2)?;
                Kind::NoteOff {
                    key: d[0],
                    velocity: d[1],
                }
            }
            0x90 => {
                let d = take(&mut pos, 2)?;
                Kind::NoteOn {
                    key: d[0],
                    velocity: d[1],
                }
            }
            0xA0 => {
                let d = take(&mut pos, 2)?;
                Kind::PolyPressure {
                    key: d[0],
                    value: d[1],
                }
            }
            0xB0 => {
                let d = take(&mut pos, 2)?;
                Kind::Control {
                    controller: d[0],
                    value: d[1],
                }
            }
            0xC0 => Kind::Program {
                program: take(&mut pos, 1)?[0],
            },
            0xD0 => Kind::ChannelPressure {
                value: take(&mut pos, 1)?[0],
            },
            0xE0 => {
                let d = take(&mut pos, 2)?;
                Kind::PitchBend {
                    value: d[0] as u16 | (d[1] as u16) << 7,
                }
            }
            _ => {
                let at = pos;
                let meta = take(&mut pos, 1)?[0];
                match meta {
                    0x51 => {
                        let d = take(&mut pos, 3)?;
                        Kind::Tempo {
                            microseconds: be24(&d, 0),
                        }
                    }
                    0x2F => Kind::End {
                        byte: take(&mut pos, 1)?[0],
                    },
                    other => {
                        return Err(error(
                            at,
                            format!(
                                "meta type 0x{other:02X}: SEQ metas carry no length byte, so only tempo (51) and end of track (2F) can be read"
                            ),
                        ));
                    }
                }
            }
        };
        *events_total += 1;
        if *events_total > MAX_EVENTS {
            return Err(error(
                offset,
                format!("more than {MAX_EVENTS} events in one file"),
            ));
        }
        let end = matches!(kind, Kind::End { .. });
        events.push(Event {
            offset,
            length: pos - offset,
            delta,
            tick,
            status: current,
            running_status: running,
            kind,
        });
        if end {
            break;
        }
    }
    header.end = pos;
    let (meanings, loops) = interpret_controls(&events);
    Ok(Sequence {
        header: header.clone(),
        events,
        meanings,
        loops,
        gap_after_end: 0,
    })
}

/// NRPN tone attributes by CC98 value, selected with CC99 = tone number
/// (community format note, attribute table).
fn tone_attribute(lsb: u8) -> Option<&'static str> {
    Some(match lsb {
        0 => "priority",
        1 => "mode",
        2 => "limit_low",
        3 => "limit_high",
        4 => "adsr_ar_l",
        5 => "adsr_ar_e",
        6 => "adsr_dr",
        7 => "adsr_sl",
        8 => "adsr_sr_l",
        9 => "adsr_sr_e",
        10 => "adsr_rr_l",
        11 => "adsr_rr_e",
        12 => "adsr_sr_sign",
        13 => "vibrate_time",
        14 => "portamento_depth",
        21 => "vibrate_depth",
        22 => "portamento_time",
        _ => return None,
    })
}

/// NRPN reverb and delay settings by CC98 value, selected with CC99 16
/// (community format note, attribute table).
pub fn reverb_attribute(lsb: u8) -> Option<&'static str> {
    Some(match lsb {
        15 => "reverb_type",
        16 => "reverb_depth",
        17 => "echo_feedback",
        18 => "echo_delay_time",
        19 => "delay_delay_time",
        _ => return None,
    })
}

/// CC99 value selecting the reverb rows.
pub const NRPN_REVERB: u8 = 16;

fn controller_name(controller: u8) -> Option<&'static str> {
    Some(match controller {
        0 => "bank_select",
        6 => "data_entry",
        7 => "volume",
        10 => "pan",
        11 => "expression",
        32 => "bank_select_lsb",
        98 => "nrpn_lsb",
        99 => "nrpn_msb",
        _ => return None,
    })
}

/// Interprets controller events: NRPN selection per channel, loop starts
/// and ends, marks and VAB attribute settings.
fn interpret_controls(events: &[Event]) -> (Vec<(usize, Value)>, Vec<Loop>) {
    let mut msb: [Option<u8>; 16] = [None; 16];
    let mut lsb: [Option<u8>; 16] = [None; 16];
    let mut meanings = Vec::new();
    let mut loops: Vec<Loop> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        let Kind::Control { controller, value } = e.kind else {
            continue;
        };
        let ch = (e.status & 0x0F) as usize;
        let meaning = match controller {
            99 => {
                msb[ch] = Some(value);
                lsb[ch] = None;
                match value {
                    NRPN_LOOP_START => {
                        loops.push(Loop {
                            start_event: i,
                            count: None,
                            count_event: None,
                            end_event: None,
                        });
                        json!({"nrpn": "loop_start", "count": "from the next data entry on this channel"})
                    }
                    NRPN_LOOP_END => match loops.iter_mut().rev().find(|l| l.end_event.is_none()) {
                        Some(l) => {
                            l.end_event = Some(i);
                            json!({"nrpn": "loop_end", "start_event": l.start_event})
                        }
                        None => json!({"nrpn": "loop_end", "unmatched": true}),
                    },
                    NRPN_MARK => json!({"nrpn": "mark_select"}),
                    0..=15 => json!({"nrpn": "attribute_select", "tone": value}),
                    NRPN_REVERB => {
                        json!({"nrpn": "attribute_select", "rows": "reverb (CC98 15..19)"})
                    }
                    _ => json!({"nrpn": "msb", "value": value, "known": false}),
                }
            }
            98 => {
                lsb[ch] = Some(value);
                let name = match msb[ch] {
                    Some(0..=15) => tone_attribute(value),
                    Some(NRPN_REVERB) => reverb_attribute(value),
                    _ => None,
                };
                match name {
                    Some(name) => json!({"nrpn": "attribute_select", "attribute": name}),
                    None => json!({"nrpn": "lsb", "msb": msb[ch], "value": value, "known": false}),
                }
            }
            6 => match (msb[ch], lsb[ch]) {
                (Some(NRPN_LOOP_START), _) => {
                    if let Some(l) = loops.last_mut().filter(|l| l.count.is_none()) {
                        l.count = Some(value);
                        l.count_event = Some(i);
                    }
                    json!({"nrpn": "loop_count", "count": value, "endless": value == ENDLESS})
                }
                (Some(NRPN_MARK), _) => json!({"nrpn": "mark", "value": value}),
                (Some(tone @ 0..=15), Some(attr)) if tone_attribute(attr).is_some() => {
                    json!({"nrpn": "attribute", "tone": tone, "attribute": tone_attribute(attr), "value": value, "reverb": false})
                }
                (Some(NRPN_REVERB), Some(attr)) if reverb_attribute(attr).is_some() => {
                    json!({"nrpn": "attribute", "attribute": reverb_attribute(attr), "value": value, "reverb": true})
                }
                (m, l) => json!({"nrpn": "data_entry", "msb": m, "lsb": l, "known": false}),
            },
            other => match controller_name(other) {
                Some(name) => json!({"controller": name}),
                None => json!({"controller": "undescribed"}),
            },
        };
        meanings.push((i, meaning));
    }
    (meanings, loops)
}

impl Event {
    pub fn to_json(&self, index: usize, meaning: Option<&Value>) -> Value {
        let mut v = json!({"index": index, "offset": self.offset, "length": self.length,
            "delta": self.delta, "tick": self.tick, "status": format!("{:02X}", self.status),
            "running_status": self.running_status});
        if let Some(ch) = self.channel() {
            v["channel"] = json!(ch);
        }
        let (kind, fields) = match self.kind {
            Kind::NoteOff { key, velocity } => {
                ("note_off", json!({"key": key, "velocity": velocity}))
            }
            Kind::NoteOn { key, velocity } => (
                if velocity == 0 { "note_off" } else { "note_on" },
                json!({"key": key, "velocity": velocity, "encoding": if velocity == 0 { json!("note_on_velocity_0") } else { Value::Null }}),
            ),
            Kind::PolyPressure { key, value } => {
                ("poly_pressure", json!({"key": key, "value": value}))
            }
            Kind::Control { controller, value } => {
                ("control", json!({"controller": controller, "value": value}))
            }
            Kind::Program { program } => ("program", json!({"program": program})),
            Kind::ChannelPressure { value } => ("channel_pressure", json!({"value": value})),
            Kind::PitchBend { value } => (
                "pitch_bend",
                json!({"value": value, "from_center": value as i32 - 0x2000}),
            ),
            Kind::Tempo { microseconds } => (
                "tempo",
                json!({"microseconds_per_quarter": microseconds, "bpm": 60_000_000.0 / microseconds.max(1) as f64}),
            ),
            Kind::End { byte } => ("end_of_track", json!({"end_byte": byte})),
        };
        v["kind"] = json!(kind);
        for (k, value) in fields.as_object().expect("object") {
            if !value.is_null() {
                v[k] = value.clone();
            }
        }
        if let Some(m) = meaning {
            v["meaning"] = m.clone();
        }
        v
    }
}

impl Sequence {
    pub fn meaning(&self, index: usize) -> Option<&Value> {
        self.meanings
            .binary_search_by_key(&index, |(i, _)| *i)
            .ok()
            .map(|i| &self.meanings[i].1)
    }

    pub fn event_json(&self, index: usize) -> Value {
        self.events[index].to_json(index, self.meaning(index))
    }

    /// Ticks to microseconds through the stream's tempo events, as rational
    /// microseconds times the resolution.
    pub fn duration_microseconds(&self) -> f64 {
        let mut tempo = self.header.tempo as f64;
        let mut total = 0.0;
        for e in &self.events {
            total += e.delta as f64 * tempo;
            if let Kind::Tempo { microseconds } = e.kind {
                tempo = microseconds as f64;
            }
        }
        total / self.header.resolution.max(1) as f64
    }

    pub fn summary(&self, index: usize) -> Value {
        let h = &self.header;
        let mut kinds = std::collections::BTreeMap::<&str, usize>::new();
        let mut channels = std::collections::BTreeSet::new();
        let mut programs = std::collections::BTreeMap::<u8, std::collections::BTreeSet<u8>>::new();
        let mut controllers = std::collections::BTreeMap::<u8, usize>::new();
        let (mut running, mut notes, mut tempos) = (0usize, 0usize, Vec::new());
        for (i, e) in self.events.iter().enumerate() {
            running += e.running_status as usize;
            if let Some(ch) = e.channel() {
                channels.insert(ch);
            }
            let name = match e.kind {
                Kind::NoteOn { velocity, .. } if velocity > 0 => {
                    notes += 1;
                    "note_on"
                }
                Kind::NoteOn { .. } | Kind::NoteOff { .. } => "note_off",
                Kind::PolyPressure { .. } => "poly_pressure",
                Kind::Control { controller, .. } => {
                    *controllers.entry(controller).or_default() += 1;
                    "control"
                }
                Kind::Program { program } => {
                    programs.entry(e.status & 0x0F).or_default().insert(program);
                    "program"
                }
                Kind::ChannelPressure { .. } => "channel_pressure",
                Kind::PitchBend { .. } => "pitch_bend",
                Kind::Tempo { microseconds } => {
                    tempos.push(json!({"event": i, "offset": e.offset, "tick": e.tick, "microseconds_per_quarter": microseconds}));
                    "tempo"
                }
                Kind::End { .. } => "end_of_track",
            };
            *kinds.entry(name).or_default() += 1;
        }
        let end = self.events.last().expect("end of track");
        let Kind::End { byte } = end.kind else {
            unreachable!("sequences end with end of track")
        };
        let undescribed: Vec<Value> = self
            .meanings
            .iter()
            .filter(|(_, m)| m["known"] == json!(false) || m["controller"] == "undescribed")
            .take(64)
            .map(|(i, m)| json!({"event": i, "offset": self.events[*i].offset, "meaning": m}))
            .collect();
        let unmatched_loop_ends = self
            .meanings
            .iter()
            .filter(|(_, m)| m["unmatched"] == json!(true))
            .count();
        let mut fields = json!({
            "resolution": {"offset": h.start + if h.id.is_some() { 2 } else { 8 }, "value": h.resolution, "unit": "ticks per quarter note"},
            "tempo": {"offset": h.start + if h.id.is_some() { 4 } else { 0x0A }, "microseconds_per_quarter": h.tempo, "bpm": 60_000_000.0 / h.tempo.max(1) as f64},
            "time_signature": {"offset": h.start + if h.id.is_some() { 7 } else { 0x0D }, "numerator": h.numerator, "denominator_exponent": h.denominator_exponent,
                "denominator": 1u64.checked_shl(h.denominator_exponent as u32)},
        });
        if let Some(id) = h.id {
            fields["sequence_id"] = json!({"offset": h.start, "value": id});
            fields["data_size"] = json!({"offset": h.start + 9, "value": h.data_size, "counts": "score start through end of track"});
        }
        json!({
            "index": index,
            "start": h.start,
            "score_start": h.score_start,
            "end": h.end,
            "fields": fields,
            "events": self.events.len(),
            "event_kinds": kinds,
            "running_status_events": running,
            "notes": notes,
            "ticks": end.tick,
            "seconds": self.duration_microseconds() / 1e6,
            "channels": channels,
            "programs_by_channel": programs,
            "controllers": controllers,
            "tempo_events": tempos,
            "end_of_track": {"event": self.events.len() - 1, "offset": end.offset, "end_byte": byte, "end_byte_documented": byte == 0},
            "loops": self.loops.iter().map(|l| self.loop_json(l)).collect::<Vec<_>>(),
            "unmatched_loop_ends": unmatched_loop_ends,
            "undescribed_controls": undescribed,
            "gap_after_end": self.gap_after_end,
        })
    }

    pub fn loop_json(&self, l: &Loop) -> Value {
        let at = |i: usize| json!({"event": i, "offset": self.events[i].offset, "tick": self.events[i].tick});
        json!({"start": at(l.start_event), "count": l.count, "endless": l.count == Some(ENDLESS),
            "count_event": l.count_event.map(at),
            "end": l.end_event.map(at)})
    }
}

impl SeqFile {
    pub fn summary(&self) -> Value {
        json!({
            "container": self.container,
            "magic": {"offset": 0, "value": "pQES"},
            "version": {"offset": 4, "value": self.version, "bytes": if self.container == Container::Seq { 4 } else { 2 },
                "documented": (self.container == Container::Seq && self.version == 1) || (self.container == Container::Sep && self.version == 0)},
            "bytes": self.bytes,
            "sequences": self.sequences.iter().enumerate().map(|(i, s)| s.summary(i)).collect::<Vec<_>>(),
            "trailing_bytes": self.trailing,
            "scope": SCOPE,
        })
    }
}

fn varlen(out: &mut Vec<u8>, mut value: u32) {
    let mut stack = vec![(value & 0x7F) as u8];
    value >>= 7;
    while value > 0 {
        stack.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
    out.extend(stack.iter().rev());
}

/// What the SMF export adds besides the sequence's own events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmfOptions {
    /// Marker meta events `loopStart` / `loopEnd` at the NRPN loop events.
    pub loop_markers: bool,
}

/// Writes one sequence as an SMF type 0 file and reads it back: every
/// sequence event becomes one SMF event with the same delta, status and
/// data; the header tempo and time signature become metas at tick 0.
pub fn to_smf(seq: &Sequence, options: SmfOptions) -> Result<(Vec<u8>, Value), String> {
    let h = &seq.header;
    if h.resolution == 0 || h.resolution > 0x7FFF {
        return Err(format!(
            "resolution {} is not an SMF ticks-per-quarter division (1..=32767)",
            h.resolution
        ));
    }
    if h.tempo == 0 {
        return Err("header tempo 0 is not an SMF tempo".into());
    }
    let mut track = Vec::new();
    let mut added = vec![
        json!({"tick": 0, "meta": "tempo", "from": "header tempo", "microseconds_per_quarter": h.tempo}),
        json!({"tick": 0, "meta": "time_signature", "from": "header rhythm", "numerator": h.numerator, "denominator_exponent": h.denominator_exponent,
            "clocks_per_click": 24, "thirty_seconds_per_quarter": 8, "note": "the last two SMF fields are not in SEQ; the SMF defaults 24 and 8 are written"}),
    ];
    track.extend([0x00, 0xFF, 0x51, 0x03]);
    track.extend(&h.tempo.to_be_bytes()[1..]);
    track.extend([
        0x00,
        0xFF,
        0x58,
        0x04,
        h.numerator,
        h.denominator_exponent,
        24,
        8,
    ]);
    let starts: Vec<usize> = seq.loops.iter().map(|l| l.start_event).collect();
    let ends: Vec<usize> = seq.loops.iter().filter_map(|l| l.end_event).collect();
    let mut mapping = Vec::with_capacity(seq.events.len());
    for (i, e) in seq.events.iter().enumerate() {
        let mut delta = e.delta;
        if options.loop_markers {
            for (list, text) in [(&starts, "loopStart"), (&ends, "loopEnd")] {
                if list.contains(&i) {
                    varlen(&mut track, delta);
                    delta = 0;
                    track.extend([0xFF, 0x06, text.len() as u8]);
                    track.extend(text.as_bytes());
                    added.push(
                        json!({"tick": e.tick, "meta": "marker", "text": text, "before_event": i}),
                    );
                }
            }
        }
        varlen(&mut track, delta);
        let at = track.len();
        match e.kind {
            Kind::NoteOff { key, velocity } => track.extend([e.status, key, velocity]),
            Kind::NoteOn { key, velocity } => track.extend([e.status, key, velocity]),
            Kind::PolyPressure { key, value } => track.extend([e.status, key, value]),
            Kind::Control { controller, value } => track.extend([e.status, controller, value]),
            Kind::Program { program } => track.extend([e.status, program]),
            Kind::ChannelPressure { value } => track.extend([e.status, value]),
            Kind::PitchBend { value } => {
                track.extend([e.status, (value & 0x7F) as u8, (value >> 7) as u8])
            }
            Kind::Tempo { microseconds } => {
                track.extend([0xFF, 0x51, 0x03]);
                track.extend(&microseconds.to_be_bytes()[1..]);
            }
            Kind::End { .. } => track.extend([0xFF, 0x2F, 0x00]),
        }
        mapping.push(at);
    }
    let mut out = b"MThd".to_vec();
    out.extend(6u32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(1u16.to_be_bytes());
    out.extend(h.resolution.to_be_bytes());
    out.extend(b"MTrk");
    out.extend((track.len() as u32).to_be_bytes());
    out.extend(&track);
    let readback = smf_readback(&out, seq, options)?;
    let end_byte = match seq.events.last().map(|e| e.kind) {
        Some(Kind::End { byte }) => byte,
        _ => 0,
    };
    let mut not_in_midi = vec![
        json!({"what": "running status", "detail": format!("{} events used it; every SMF event is written with its status byte", seq.events.iter().filter(|e| e.running_status).count())}),
        json!({"what": "binding", "detail": "program numbers select programs of the VAB the game loads with this sequence, not General MIDI instruments; the VAB is not named in the SEQ"}),
        json!({"what": "nrpn conventions", "detail": "loop, mark and VAB attribute NRPNs (CC99/CC98/CC6) are kept as the same controller events; MIDI receivers do not loop or change VAB tones on them"}),
    ];
    if h.id.is_some() {
        not_in_midi.push(
            json!({"what": "SEP layout", "detail": "the sequence ID and data size are not in the SMF"}),
        );
    }
    if end_byte != 0 {
        not_in_midi.push(json!({"what": "end byte", "detail": format!("the byte after FF 2F is {end_byte:#04x}; the SMF writes length 0")}));
    }
    if seq.gap_after_end > 0 {
        not_in_midi.push(json!({"what": "bytes after end of track", "detail": format!("{} bytes before the declared data end", seq.gap_after_end)}));
    }
    let report = json!({
        "format": 0, "tracks": 1, "division": h.resolution,
        "sequence_events": seq.events.len(),
        "smf_events": readback["events"],
        "added": added,
        "not_in_midi": not_in_midi,
        "readback": readback,
    });
    Ok((out, report))
}

/// Parses the written SMF and checks every sequence event's tick, status
/// and data, and the added metas.
fn smf_readback(smf: &[u8], seq: &Sequence, options: SmfOptions) -> Result<Value, String> {
    let fail = |what: String| Err(format!("SMF readback failed: {what}"));
    if smf.len() < 22 || &smf[0..4] != b"MThd" || &smf[14..18] != b"MTrk" {
        return fail("header".into());
    }
    let length = be32(smf, 18) as usize;
    if 22 + length != smf.len() {
        return fail("track length".into());
    }
    let mut pos = 22;
    let mut tick = 0u64;
    let mut parsed: Vec<(u64, Vec<u8>)> = Vec::new();
    while pos < smf.len() {
        let mut delta = 0u64;
        loop {
            let b = smf[pos];
            pos += 1;
            delta = delta << 7 | (b & 0x7F) as u64;
            if b < 0x80 {
                break;
            }
        }
        tick += delta;
        let status = smf[pos];
        let n = match status {
            0xFF => 3 + smf[pos + 2] as usize,
            s if (0xC0..0xE0).contains(&s) => 2,
            _ => 3,
        };
        parsed.push((tick, smf[pos..pos + n].to_vec()));
        pos += n;
    }
    let markers = parsed
        .iter()
        .filter(|(_, b)| b[..2] == [0xFF, 0x06])
        .count();
    let own: Vec<&(u64, Vec<u8>)> = parsed
        .iter()
        .skip(2)
        .filter(|(_, b)| b[..2] != [0xFF, 0x06])
        .collect();
    if own.len() != seq.events.len() {
        return fail(format!(
            "{} events read back for {}",
            own.len(),
            seq.events.len()
        ));
    }
    for (i, (e, (t, b))) in seq.events.iter().zip(own).enumerate() {
        let expected: Vec<u8> = match e.kind {
            Kind::Tempo { microseconds } => {
                let mut v = vec![0xFF, 0x51, 0x03];
                v.extend(&microseconds.to_be_bytes()[1..]);
                v
            }
            Kind::End { .. } => vec![0xFF, 0x2F, 0x00],
            Kind::Program { program } => vec![e.status, program],
            Kind::ChannelPressure { value } => vec![e.status, value],
            Kind::PitchBend { value } => vec![e.status, (value & 0x7F) as u8, (value >> 7) as u8],
            Kind::NoteOff { key, velocity } | Kind::NoteOn { key, velocity } => {
                vec![e.status, key, velocity]
            }
            Kind::PolyPressure { key, value } => vec![e.status, key, value],
            Kind::Control { controller, value } => vec![e.status, controller, value],
        };
        if *t != e.tick || *b != expected {
            return fail(format!("event {i} at byte {} differs", e.offset));
        }
    }
    let expected_markers = if options.loop_markers {
        seq.loops.len() + seq.loops.iter().filter(|l| l.end_event.is_some()).count()
    } else {
        0
    };
    if markers != expected_markers {
        return fail(format!("{markers} markers for {expected_markers}"));
    }
    Ok(
        json!({"events": parsed.len(), "sequence_events_matched": seq.events.len(), "markers": markers,
        "compared": "tick, status and data bytes of every sequence event; header tempo and time signature at tick 0"}),
    )
}

#[cfg(test)]
pub mod fixtures {
    /// A SEQ header with the given resolution, tempo and 4/4.
    pub fn seq_header(resolution: u16, tempo: u32) -> Vec<u8> {
        let mut b = b"pQES".to_vec();
        b.extend(1u32.to_be_bytes());
        b.extend(resolution.to_be_bytes());
        b.extend(&tempo.to_be_bytes()[1..]);
        b.extend([4, 2]);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::seq_header;
    use super::*;

    fn sample_seq() -> Vec<u8> {
        let mut b = seq_header(480, 500_000);
        b.extend([0x00, 0xC0, 0x01]); // program 1
        b.extend([0x00, 0xB0, 0x63, 0x14]); // NRPN loop start
        b.extend([0x00, 0x06, 0x7F]); // running status: count 127
        b.extend([0x00, 0x90, 0x3C, 0x64]); // note on
        b.extend([0x83, 0x60, 0x3C, 0x00]); // 480 ticks later, release
        b.extend([0x00, 0xFF, 0x51, 0x07, 0xA1, 0x20]); // tempo, no length byte
        b.extend([0x10, 0x51, 0x06, 0x1A, 0x80]); // running status meta
        b.extend([0x00, 0xB0, 0x63, 0x1E]); // loop end
        b.extend([0x00, 0xFF, 0x2F, 0x00]);
        b
    }

    #[test]
    fn seq_events_carry_offsets_running_status_and_meanings() {
        let bytes = sample_seq();
        let file = parse(&bytes, Container::Detect).unwrap();
        assert_eq!(file.container, Container::Seq);
        assert_eq!(file.trailing, 0);
        let s = &file.sequences[0];
        assert_eq!(s.events.len(), 9);
        assert_eq!(s.events[0].offset, 15);
        assert!(s.events[2].running_status);
        assert_eq!(s.events[4].tick, 480);
        assert!(s.events[4].is_release());
        assert_eq!(
            s.events[5].kind,
            Kind::Tempo {
                microseconds: 0x07A120
            }
        );
        assert!(s.events[6].running_status);
        assert_eq!(
            s.events[6].kind,
            Kind::Tempo {
                microseconds: 0x061A80
            }
        );
        assert_eq!(s.loops.len(), 1);
        assert_eq!(s.loops[0].count, Some(127));
        assert_eq!(s.loops[0].end_event, Some(7));
        assert_eq!(s.meaning(2).unwrap()["nrpn"], "loop_count");
        let (smf, report) = to_smf(s, SmfOptions { loop_markers: true }).unwrap();
        assert_eq!(&smf[0..4], b"MThd");
        assert_eq!(report["readback"]["markers"], 2);
        assert_eq!(report["readback"]["sequence_events_matched"], 9);
    }

    #[test]
    fn refusals_are_located() {
        let mut bytes = sample_seq();
        bytes[0] = b'X';
        assert_eq!(parse(&bytes, Container::Detect).unwrap_err().offset, 0);
        let mut bytes = sample_seq();
        bytes[7] = 2;
        assert_eq!(parse(&bytes, Container::Detect).unwrap_err().offset, 4);
        // Unknown meta type.
        let mut bytes = seq_header(480, 500_000);
        bytes.extend([0x00, 0xFF, 0x03, 0x01]);
        let e = parse(&bytes, Container::Seq).unwrap_err();
        assert_eq!(e.offset, 17);
        // Data byte before any status.
        let mut bytes = seq_header(480, 500_000);
        bytes.extend([0x00, 0x3C, 0x40]);
        assert_eq!(parse(&bytes, Container::Seq).unwrap_err().offset, 16);
        // SysEx.
        let mut bytes = seq_header(480, 500_000);
        bytes.extend([0x00, 0xF0, 0x01, 0xF7]);
        assert_eq!(parse(&bytes, Container::Seq).unwrap_err().offset, 16);
        // Five-byte delta.
        let mut bytes = seq_header(480, 500_000);
        bytes.extend([0x81, 0x81, 0x81, 0x81, 0x01, 0x90, 0x3C, 0x40]);
        assert_eq!(parse(&bytes, Container::Seq).unwrap_err().offset, 15);
        // No end of track.
        let mut bytes = seq_header(480, 500_000);
        bytes.extend([0x00, 0x90, 0x3C, 0x40]);
        let e = parse(&bytes, Container::Seq).unwrap_err();
        assert_eq!(e.offset, 19);
        // Truncated data bytes.
        let mut bytes = seq_header(480, 500_000);
        bytes.extend([0x00, 0x90, 0x3C]);
        assert_eq!(parse(&bytes, Container::Seq).unwrap_err().offset, 17);
    }

    #[test]
    fn nrpn_uses_without_a_public_description_stay_uninterpreted() {
        let mut b = seq_header(480, 500_000);
        b.extend([0x00, 0xB0, 99, 20, 0x00, 98, 3]); // CC98 after a loop start
        b.extend([0x00, 99, 16, 0x00, 98, 4, 0x00, 6, 9]); // tone attribute under CC99 16
        b.extend([0x00, 99, 2, 0x00, 98, 7, 0x00, 6, 60]); // tone 2 sustain level
        b.extend([0x00, 0xFF, 0x2F, 0x00]);
        let s = &parse(&b, Container::Seq).unwrap().sequences[0];
        assert_eq!(s.loops[0].count, None);
        assert_eq!(s.meaning(1).unwrap()["known"], false);
        assert_eq!(s.meaning(3).unwrap()["known"], false);
        assert_eq!(s.meaning(4).unwrap()["known"], false);
        assert_eq!(s.meaning(7).unwrap()["attribute"], "adsr_sl");
        assert_eq!(s.meaning(7).unwrap()["tone"], 2);
        assert_eq!(
            s.summary(0)["undescribed_controls"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn sep_sequences_follow_their_data_sizes() {
        let mut b = b"pQES".to_vec();
        b.extend([0, 0]);
        for id in 0..2u16 {
            let score = [
                0x00, 0x90, 0x3C, 0x40, 0x10, 0x3C, 0x00, 0x00, 0xFF, 0x2F, 0x00,
            ];
            b.extend(id.to_be_bytes());
            b.extend(96u16.to_be_bytes());
            b.extend([0x07, 0xA1, 0x20, 3, 2]);
            b.extend((score.len() as u32).to_be_bytes());
            b.extend(score);
        }
        b.extend([0xAA, 0xBB]);
        let file = parse(&b, Container::Detect).unwrap();
        assert_eq!(file.container, Container::Sep);
        assert_eq!(file.sequences.len(), 2);
        assert_eq!(file.sequences[1].header.start, 6 + 24);
        assert_eq!(file.sequences[1].header.id, Some(1));
        assert_eq!(file.sequences[1].header.numerator, 3);
        assert_eq!(file.trailing, 2);
        // A data size past the end refuses at the size field.
        let mut bad = b.clone();
        bad[6 + 9..6 + 13].copy_from_slice(&1000u32.to_be_bytes());
        let e = parse(&bad, Container::Sep).unwrap_err();
        assert_eq!((e.offset, e.sequence), (15, Some(0)));
        // An end of track beyond the declared size refuses.
        let mut short = b.clone();
        short[6 + 9..6 + 13].copy_from_slice(&8u32.to_be_bytes());
        assert!(parse(&short, Container::Sep).is_err());
    }
}
