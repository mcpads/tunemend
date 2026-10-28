//! Plays one PlayStation SEQ sequence against a VAB bank on at most 24 SPU
//! voice models: program to VAB program, key to the tones whose key range
//! holds it, tone tuning and ADSR, declared volume/pan/bend readings, and
//! NRPN loops taken a requested number of times.
//!
//! The sound library's own scheduling, allocation and arithmetic are not
//! documented; every reading used here is declared in the report.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::psx_seq::{Kind, Sequence};
use crate::spu_voice::{OUTPUT_RATE, Voice};
use crate::vab::{PROGRAM_TABLE, TONE_BYTES, Vab};

/// SPU hardware voices.
pub const SPU_VOICES: usize = 24;
/// Longest render, in seconds of output including the tail.
pub const MAX_SECONDS: u32 = 600;
pub const MAX_TAIL_SECONDS: u32 = 10;
pub const MAX_LOOP_REPEATS: u32 = 64;
/// Overflow records listed in the report.
const LISTED: usize = 64;

pub const READINGS: [&str; 9] = [
    "events are scheduled at their exact tick time (header tempo, then each tempo event, over the resolution) rounded down to a 44100 Hz sample; the sound library's tick interrupt rate is not modelled",
    "a channel plays VAB program 0 until its first program change; a program change selects the VAB program of that number",
    "a note-on starts one voice for every used tone of the channel's program whose key range (min..max) holds the key; a note-off (or note-on velocity 0) keys off every held voice of that channel and key",
    "pitch = 4096 * 2^((key - center + fine_tune/128 + bend) / 12), the VAB pitch reading; bend = (value - 0x2000) / 0x2000 times the tone's pbmax (up) or pbmin (down) in semitones (the unit is inferred from typical values 2, 12 and 24)",
    "voice gain = velocity/127 * tone volume/127 * program volume/127 * master volume/127 * CC7/127 * CC11/127; CC7 and CC11 start at 127 and apply to sounding voices",
    "pan position = 64 + (tone pan - 64) + (program pan - 64) + (master pan - 64) + (CC10 - 64), clamped to 0..127, CC10 starting at 64; left = min(1, (127 - p)/63), right = min(1, p/64)",
    "a voice occupies one of the 24 SPU voices from key-on until its envelope reaches 0 or its region ends; when none is free, voice_overflow decides",
    "at a loop end (NRPN 30) with a jump remaining, the later events at the same tick except end of track are processed, then playback continues after the loop start (NRPN 20) event; channel state and sounding voices carry over",
    "a finite source loop count limits the jumps to that count (the documented meaning of counts other than 127 is not stated); count 127 is endless and the request bounds it",
];

/// What happens when a note needs a voice and all 24 are occupied.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Overflow {
    /// Refuse the render at the note's location.
    #[default]
    Refuse,
    /// Skip the new voice; every skipped voice is counted and listed.
    Drop,
    /// Take the releasing voice with the lowest envelope level; drop the new
    /// voice when every voice is still held.
    StealReleasing,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Jumps back at the loop end.
    pub loop_repeats: u32,
    pub max_seconds: u32,
    pub tail_seconds: f64,
    pub overflow: Overflow,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderError {
    /// Event index and byte offset, when an event refused.
    pub event: Option<(usize, usize)>,
    pub message: String,
}

fn refuse(index: usize, seq: &Sequence, message: impl Into<String>) -> RenderError {
    RenderError {
        event: Some((index, seq.events[index].offset)),
        message: message.into(),
    }
}

#[derive(Debug, Clone)]
struct Tone<'a> {
    program: usize,
    tone: usize,
    volume: u8,
    pan: u8,
    center: u8,
    fine: u8,
    min: u8,
    max: u8,
    bend_down: u8,
    bend_up: u8,
    adsr1: u16,
    adsr2: u16,
    vag: usize,
    region: Option<&'a [u8]>,
}

#[derive(Debug, Clone, Copy)]
struct Channel {
    program: u8,
    volume: u8,
    expression: u8,
    pan: u8,
    bend: u16,
}

impl Default for Channel {
    fn default() -> Self {
        Channel {
            program: 0,
            volume: 127,
            expression: 127,
            pan: 64,
            bend: 0x2000,
        }
    }
}

struct Active<'a> {
    voice: Voice<'a>,
    channel: u8,
    key: u8,
    held: bool,
    tone: usize,
    /// velocity * tone, program and master volume, without the channel.
    base_gain: f32,
    left: f32,
    right: f32,
}

pub struct Rendered {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub report: Value,
}

fn pan_gains(position: i32) -> (f32, f32) {
    let p = position.clamp(0, 127) as f32;
    (((127.0 - p) / 63.0).min(1.0), (p / 64.0).min(1.0))
}

/// The pitch register for a tone at a key and channel bend.
fn pitch_of(tone: &Tone, key: u8, bend: u16) -> (u16, bool) {
    let offset = bend as f64 - 8192.0;
    let range = if offset >= 0.0 {
        tone.bend_up
    } else {
        tone.bend_down
    } as f64;
    let semis =
        key as f64 - tone.center as f64 + tone.fine as f64 / 128.0 + offset / 8192.0 * range;
    let exact = 4096.0 * 2f64.powf(semis / 12.0);
    let value = exact.round().clamp(1.0, u16::MAX as f64) as u16;
    (value, exact > 0x3FFF as f64)
}

/// Tones of each program with tones, in tone order.
fn tones(vab: &Vab) -> Vec<Vec<Tone<'_>>> {
    let mut by_program: Vec<Vec<Tone>> = (0..crate::vab::PROGRAMS).map(|_| Vec::new()).collect();
    for &program in &vab.tone_programs {
        let count = vab.header[PROGRAM_TABLE + program * 16] as usize;
        for tone in 0..count {
            let Some(at) = vab.tone_offset(program, tone) else {
                continue;
            };
            let t = &vab.header[at..at + TONE_BYTES];
            let vag = u16::from_le_bytes([t[0x16], t[0x17]]) as usize;
            let region = vab
                .region_bytes(vag)
                .ok()
                .filter(|r| crate::vab::stream_of(r, vab.regions[vag].start).is_ok());
            by_program[program].push(Tone {
                program,
                tone,
                volume: t[2],
                pan: t[3],
                center: t[4],
                fine: t[5],
                min: t[6],
                max: t[7],
                bend_down: t[0x0C],
                bend_up: t[0x0D],
                adsr1: u16::from_le_bytes([t[0x10], t[0x11]]),
                adsr2: u16::from_le_bytes([t[0x12], t[0x13]]),
                vag,
                region,
            });
        }
    }
    by_program
}

#[derive(Default)]
struct Counts {
    notes: usize,
    voices: usize,
    peak_voices: usize,
    overflow: usize,
    stolen: usize,
    dropped: usize,
    overflow_list: Vec<Value>,
    /// Per channel: stolen and dropped voices.
    overflow_by_channel: std::collections::BTreeMap<usize, [usize; 2]>,
    /// Ticks of the first and last overflow.
    overflow_ticks: Option<(u64, u64)>,
    skipped_notes: usize,
    skipped_list: Vec<Value>,
    pitch_clamped: usize,
    reverb_settings: Vec<Value>,
    marks: Vec<Value>,
    mark_count: usize,
    reverb_count: usize,
    loop_jumps: Vec<Value>,
    events_processed: usize,
    tones_used: std::collections::BTreeSet<(usize, usize)>,
    programs_used: std::collections::BTreeSet<u8>,
}

/// Renders the sequence as stereo float samples at 44100 Hz.
pub fn render(seq: &Sequence, vab: &Vab, options: &Options) -> Result<Rendered, RenderError> {
    let plain = |message: String| RenderError {
        event: None,
        message,
    };
    if options.loop_repeats > MAX_LOOP_REPEATS {
        return Err(plain(format!("loop_repeats is at most {MAX_LOOP_REPEATS}")));
    }
    if options.max_seconds == 0 || options.max_seconds > MAX_SECONDS {
        return Err(plain(format!("max_seconds is 1..={MAX_SECONDS}")));
    }
    if !(0.0..=MAX_TAIL_SECONDS as f64).contains(&options.tail_seconds) {
        return Err(plain(format!("tail_seconds is 0..={MAX_TAIL_SECONDS}")));
    }
    let resolution = seq.header.resolution as u128;
    if resolution == 0 {
        return Err(plain("resolution 0 has no tick duration".into()));
    }
    let max_samples = options.max_seconds as usize * OUTPUT_RATE as usize;
    let tones = tones(vab);
    let master_volume = vab.header[0x18] as f32 / 127.0;
    let master_pan = vab.header[0x19] as i32 - 64;
    let program_volume = |p: usize| vab.header[PROGRAM_TABLE + p * 16 + 1] as f32 / 127.0;
    let program_pan = |p: usize| vab.header[PROGRAM_TABLE + p * 16 + 4] as i32 - 64;
    let mut channels = [Channel::default(); 16];
    let mut voices: Vec<Active> = Vec::new();
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut counts = Counts::default();
    let mut jumps_left: Vec<u32> = seq
        .loops
        .iter()
        .map(|l| match l.count {
            Some(c) if c != crate::psx_seq::ENDLESS => options.loop_repeats.min(c as u32),
            _ => options.loop_repeats,
        })
        .collect();
    let mut tempo = seq.header.tempo as u128;
    let mut elapsed: u128 = 0; // microseconds times resolution
    let mut cursor = 0usize;
    let mut pending_jump: Option<usize> = None;
    let mut end_reason = "end_of_track";

    let advance = |voices: &mut Vec<Active>,
                   left: &mut Vec<f32>,
                   right: &mut Vec<f32>,
                   until: usize|
     -> Result<(), RenderError> {
        while left.len() < until {
            let (mut l, mut r) = (0f32, 0f32);
            for a in voices.iter_mut() {
                let s = a.voice.next_sample().map_err(plain)? as f32 / 32768.0;
                l += s * a.left;
                r += s * a.right;
            }
            left.push(l);
            right.push(r);
        }
        voices.retain(|a| !a.voice.is_off());
        Ok(())
    };
    let channel_gains = |a: &mut Active, ch: &Channel, tone: &Tone| {
        let gain = a.base_gain * ch.volume as f32 / 127.0 * ch.expression as f32 / 127.0;
        let (gl, gr) = pan_gains(
            64 + (tone.pan as i32 - 64)
                + program_pan(tone.program)
                + master_pan
                + (ch.pan as i32 - 64),
        );
        a.left = gain * gl;
        a.right = gain * gr;
    };
    let tone_of = |a: &Active| -> &Tone { &tones[a.tone >> 8][a.tone & 0xFF] };

    loop {
        let index = cursor;
        let e = &seq.events[index];
        elapsed += e.delta as u128 * tempo;
        let at = (elapsed * OUTPUT_RATE as u128 / (1_000_000 * resolution)) as usize;
        if at >= max_samples {
            end_reason = "max_seconds";
            advance(&mut voices, &mut left, &mut right, max_samples)?;
            break;
        }
        advance(&mut voices, &mut left, &mut right, at)?;
        counts.events_processed += 1;
        let ch_index = (e.status & 0x0F) as usize;
        match e.kind {
            Kind::NoteOn { key, velocity } if velocity > 0 => {
                counts.notes += 1;
                let ch = channels[ch_index];
                counts.programs_used.insert(ch.program);
                let program = ch.program as usize;
                let candidates: Vec<usize> = tones[program]
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| (t.min..=t.max).contains(&key))
                    .map(|(i, _)| i)
                    .collect();
                if candidates.is_empty() {
                    counts.skipped_notes += 1;
                    if counts.skipped_list.len() < LISTED {
                        counts.skipped_list.push(json!({"event": index, "offset": e.offset, "channel": ch_index, "program": program, "key": key,
                            "reason": if tones[program].is_empty() { "the program has no tones in this VAB" } else { "no tone's key range holds the key" }}));
                    }
                }
                for i in candidates {
                    let tone = &tones[program][i];
                    let Some(region) = tone.region else {
                        counts.skipped_notes += 1;
                        if counts.skipped_list.len() < LISTED {
                            counts.skipped_list.push(json!({"event": index, "offset": e.offset, "channel": ch_index, "program": program, "key": key,
                                "tone": i, "vag": tone.vag, "reason": "the tone's VAG region does not decode"}));
                        }
                        continue;
                    };
                    if voices.len() >= SPU_VOICES {
                        counts.overflow += 1;
                        counts.overflow_ticks = Some(match counts.overflow_ticks {
                            Some((first, _)) => (first, e.tick),
                            None => (e.tick, e.tick),
                        });
                        let held = voices.iter().filter(|a| a.held).count();
                        let detail = json!({"event": index, "offset": e.offset, "tick": e.tick, "channel": ch_index, "key": key,
                            "held": held, "releasing": voices.len() - held});
                        match options.overflow {
                            Overflow::Refuse => {
                                return Err(refuse(
                                    index,
                                    seq,
                                    format!(
                                        "the note needs a voice beyond the {SPU_VOICES} SPU voices: {held} held and {} releasing; choose voice_overflow drop or steal_releasing to continue",
                                        voices.len() - held
                                    ),
                                ));
                            }
                            Overflow::Drop => {
                                counts.dropped += 1;
                                counts.overflow_by_channel.entry(ch_index).or_default()[1] += 1;
                                if counts.overflow_list.len() < LISTED {
                                    let mut d = detail;
                                    d["action"] = json!("dropped");
                                    counts.overflow_list.push(d);
                                }
                                continue;
                            }
                            Overflow::StealReleasing => {
                                let victim = voices
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, a)| !a.held)
                                    .min_by_key(|(_, a)| a.voice.level())
                                    .map(|(i, _)| i);
                                let mut d = detail;
                                match victim {
                                    Some(v) => {
                                        voices.remove(v);
                                        counts.stolen += 1;
                                        counts.overflow_by_channel.entry(ch_index).or_default()
                                            [0] += 1;
                                        d["action"] = json!("stole_releasing");
                                    }
                                    None => {
                                        counts.dropped += 1;
                                        counts.overflow_by_channel.entry(ch_index).or_default()
                                            [1] += 1;
                                        d["action"] = json!("dropped_all_held");
                                    }
                                }
                                if counts.overflow_list.len() < LISTED {
                                    counts.overflow_list.push(d.clone());
                                }
                                if d["action"] == "dropped_all_held" {
                                    continue;
                                }
                            }
                        }
                    }
                    let (pitch, clamped) = pitch_of(tone, key, ch.bend);
                    counts.pitch_clamped += clamped as usize;
                    let voice = Voice::key_on(region, pitch, tone.adsr1, tone.adsr2, false)
                        .map_err(|m| refuse(index, seq, m))?;
                    let base_gain = velocity as f32 / 127.0 * tone.volume as f32 / 127.0
                        * program_volume(program)
                        * master_volume;
                    let mut a = Active {
                        voice,
                        channel: ch_index as u8,
                        key,
                        held: true,
                        tone: program << 8 | i,
                        base_gain,
                        left: 0.0,
                        right: 0.0,
                    };
                    channel_gains(&mut a, &ch, tone);
                    voices.push(a);
                    counts.voices += 1;
                    counts.tones_used.insert((program, tone.tone));
                }
                counts.peak_voices = counts.peak_voices.max(voices.len());
            }
            Kind::NoteOn { key, .. } | Kind::NoteOff { key, .. } => {
                for a in voices
                    .iter_mut()
                    .filter(|a| a.held && a.channel as usize == ch_index && a.key == key)
                {
                    a.voice.key_off();
                    a.held = false;
                }
            }
            Kind::Program { program } => channels[ch_index].program = program,
            Kind::PitchBend { value } => {
                channels[ch_index].bend = value;
                for a in voices.iter_mut().filter(|a| a.channel as usize == ch_index) {
                    let (pitch, clamped) = pitch_of(tone_of(a), a.key, value);
                    counts.pitch_clamped += clamped as usize;
                    a.voice.set_pitch(pitch);
                }
            }
            Kind::Tempo { microseconds } => tempo = microseconds as u128,
            Kind::Control { controller, value } => {
                let meaning = seq.meaning(index).cloned().unwrap_or(Value::Null);
                match controller {
                    7 | 10 | 11 => {
                        let ch = &mut channels[ch_index];
                        match controller {
                            7 => ch.volume = value,
                            10 => ch.pan = value,
                            _ => ch.expression = value,
                        }
                        let ch = *ch;
                        for a in voices.iter_mut().filter(|a| a.channel as usize == ch_index) {
                            let tone = tone_of(a).clone();
                            channel_gains(a, &ch, &tone);
                        }
                    }
                    6 | 98 | 99 => match meaning["nrpn"].as_str() {
                        Some("attribute") if meaning["reverb"] == json!(true) => {
                            counts.reverb_count += 1;
                            if counts.reverb_settings.len() < LISTED {
                                counts.reverb_settings.push(
                                    json!({"event": index, "offset": e.offset, "setting": meaning}),
                                );
                            }
                        }
                        Some("attribute") => {
                            return Err(refuse(
                                index,
                                seq,
                                format!(
                                    "NRPN VAB attribute {} changes tone settings, which this render does not model",
                                    meaning["attribute"]
                                ),
                            ));
                        }
                        Some("mark") => {
                            counts.mark_count += 1;
                            if counts.marks.len() < LISTED {
                                counts.marks.push(json!({"event": index, "offset": e.offset, "tick": e.tick, "value": value}));
                            }
                        }
                        Some("loop_end") if meaning["unmatched"] != json!(true) => {
                            let l = seq
                                .loops
                                .iter()
                                .position(|l| l.end_event == Some(index))
                                .expect("matched loop end");
                            if jumps_left[l] > 0 {
                                jumps_left[l] -= 1;
                                pending_jump = Some(seq.loops[l].start_event + 1);
                                counts.loop_jumps.push(json!({"loop": l, "at_event": index, "offset": e.offset,
                                    "output_sample": at, "to_event": seq.loops[l].start_event + 1}));
                            }
                        }
                        Some("data_entry") | Some("msb") | Some("lsb")
                            if meaning["known"] == json!(false) =>
                        {
                            return Err(refuse(
                                index,
                                seq,
                                format!(
                                    "uninterpreted NRPN use {meaning} (no public description); this render plays only described messages"
                                ),
                            ));
                        }
                        _ => {}
                    },
                    other => {
                        return Err(refuse(
                            index,
                            seq,
                            format!(
                                "controller {other} ({}) has no described SEQ playback meaning here; bank select would switch the VAB, which this render binds once",
                                meaning["controller"].as_str().unwrap_or("undescribed")
                            ),
                        ));
                    }
                }
            }
            Kind::PolyPressure { .. } | Kind::ChannelPressure { .. } => {
                return Err(refuse(
                    index,
                    seq,
                    "aftertouch is not described for SEQ playback",
                ));
            }
            Kind::End { .. } => break,
        }
        cursor += 1;
        if let Some(target) = pending_jump {
            let next = &seq.events[cursor];
            if next.delta != 0 || matches!(next.kind, Kind::End { .. }) {
                cursor = target;
                pending_jump = None;
            }
        }
    }
    let content = left.len();
    for a in voices.iter_mut() {
        a.voice.key_off();
        a.held = false;
    }
    let tail_end =
        (content + (options.tail_seconds * OUTPUT_RATE as f64) as usize).min(max_samples);
    while left.len() < tail_end && !voices.is_empty() {
        let next = (left.len() + 4410).min(tail_end);
        advance(&mut voices, &mut left, &mut right, next)?;
    }
    let peak = left
        .iter()
        .chain(right.iter())
        .fold(0f32, |m, v| m.max(v.abs()));
    let report = json!({
        "output_rate": OUTPUT_RATE,
        "channels": 2,
        "samples": left.len(),
        "seconds": left.len() as f64 / OUTPUT_RATE as f64,
        "content_samples": content,
        "end_reason": end_reason,
        "tail": {"seconds_requested": options.tail_seconds, "samples": left.len() - content, "voices_still_sounding": voices.len()},
        "peak": peak,
        "clips_if_integer": peak > 1.0,
        "events_processed": counts.events_processed,
        "notes": counts.notes,
        "voices_started": counts.voices,
        "peak_voices": counts.peak_voices,
        "voice_limit": SPU_VOICES,
        "voice_overflow": {"policy": options.overflow, "events": counts.overflow, "stolen": counts.stolen, "dropped": counts.dropped,
            "by_channel": counts.overflow_by_channel.iter().map(|(c, [s, d])| (c.to_string(), json!({"stolen": s, "dropped": d}))).collect::<serde_json::Map<_, _>>(),
            "first_tick": counts.overflow_ticks.map(|t| t.0), "last_tick": counts.overflow_ticks.map(|t| t.1),
            "listed": counts.overflow_list, "listed_count": counts.overflow_list.len().min(LISTED),
            "truncated": counts.overflow > LISTED, "list_limit": LISTED},
        "skipped_notes": {"count": counts.skipped_notes, "listed": counts.skipped_list, "truncated": counts.skipped_notes > LISTED, "list_limit": LISTED},
        "pitch_clamped": counts.pitch_clamped,
        "loop_jumps": counts.loop_jumps,
        "loop_repeats_requested": options.loop_repeats,
        "marks": counts.marks,
        "mark_count": counts.mark_count,
        "reverb_settings_not_applied": counts.reverb_settings,
        "reverb_setting_count": counts.reverb_count,
        "programs_used": counts.programs_used,
        "tones_used": counts.tones_used.iter().map(|(p, t)| json!({"program": p, "tone": t})).collect::<Vec<_>>(),
        "readings": READINGS,
        "voice_model": crate::spu_voice::MODEL,
        "voice_readings": crate::spu_voice::READINGS,
        "not_modelled": "the sound library's tick rate, voice priority and allocation, key and volume arithmetic, reverb (settings are reported), vibrato, portamento, tone attribute and uninterpreted NRPNs (refused), bank switching (refused), SPU mixing and DAC",
    });
    Ok(Rendered {
        left,
        right,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psx_seq::{Container, fixtures::seq_header, parse};

    /// Program 0 with one tone over every key, centre 60, playing a looped
    /// region with a fast attack and a full sustain.
    fn bank() -> Vab {
        use crate::vab::*;
        let mut region = vec![0u8; 16];
        region.extend([0x0A, 0x04]);
        region.extend([0x13u8; 14]);
        region.extend([0x1A, 0x03]);
        region.extend([0x31u8; 14]);
        let mut h = vec![0u8; TONE_TABLE + TONE_BLOCK_BYTES + VAG_TABLE_BYTES];
        h[..4].copy_from_slice(MAGIC);
        h[4..8].copy_from_slice(&7u32.to_le_bytes());
        h[0x12] = 1;
        h[0x14] = 1;
        h[0x16] = 1;
        h[0x18] = 127;
        h[0x19] = 64;
        h[PROGRAM_TABLE] = 1;
        h[PROGRAM_TABLE + 1] = 127;
        h[PROGRAM_TABLE + 4] = 64;
        let at = TONE_TABLE;
        h[at + 2] = 127;
        h[at + 3] = 64;
        h[at + 4] = 60;
        h[at + 7] = 127;
        h[at + 0x10..at + 0x12].copy_from_slice(&0x000Fu16.to_le_bytes());
        h[at + 0x12..at + 0x14].copy_from_slice(&0x1FC0u16.to_le_bytes());
        h[at + 0x16..at + 0x18].copy_from_slice(&1u16.to_le_bytes());
        let table = h.len() - VAG_TABLE_BYTES;
        h[table + 2..table + 4].copy_from_slice(&((region.len() / 8) as u16).to_le_bytes());
        Vab::parse(h, region).unwrap()
    }

    fn render_bytes(
        score: &[u8],
        overflow: Overflow,
        loop_repeats: u32,
    ) -> Result<Rendered, RenderError> {
        let mut bytes = seq_header(480, 500_000);
        bytes.extend(score);
        let file = parse(&bytes, Container::Seq).unwrap();
        render(
            &file.sequences[0],
            &bank(),
            &Options {
                loop_repeats,
                max_seconds: 30,
                tail_seconds: 0.5,
                overflow,
            },
        )
    }

    #[test]
    fn notes_start_voices_and_timing_follows_tempo() {
        let vab = bank();
        let program = vab.tone_programs[0] as u8;
        let tone = &tones(&vab)[program as usize][0];
        let key = tone.min;
        // Half a second at 120 BPM, 480 ticks per quarter: 480 ticks.
        let score = [
            0x00, 0xC0, program, 0x00, 0x90, key, 0x7F, 0x83, 0x60, 0x80, key, 0x00, 0x00, 0xFF,
            0x2F, 0x00,
        ];
        let r = render_bytes(&score, Overflow::Refuse, 0).unwrap();
        assert_eq!(r.report["notes"], 1);
        assert_eq!(r.report["content_samples"], 22050);
        assert!(r.left.iter().any(|&v| v != 0.0));
    }

    #[test]
    fn voice_limit_refuses_or_drops() {
        let vab = bank();
        let program = vab.tone_programs[0] as u8;
        let tone = &tones(&vab)[program as usize][0];
        let mut score = vec![0x00, 0xC0, program];
        for i in 0..25u8 {
            score.extend([0x00, 0x90, tone.min, 0x40 + i]);
        }
        score.extend([0x10, 0xFF, 0x2F, 0x00]);
        let layered = tones(&vab)[program as usize]
            .iter()
            .filter(|t| (t.min..=t.max).contains(&tone.min))
            .count();
        let err = render_bytes(&score, Overflow::Refuse, 0).err();
        if layered >= 1 {
            let err = err.expect("25 notes exceed 24 voices");
            assert!(err.message.contains("24 SPU voices"), "{}", err.message);
            let r = render_bytes(&score, Overflow::Drop, 0).unwrap();
            assert!(r.report["voice_overflow"]["dropped"].as_u64().unwrap() >= 1);
            assert_eq!(r.report["peak_voices"], 24);
        }
    }

    #[test]
    fn loops_jump_back_and_attribute_nrpn_refuses() {
        let vab = bank();
        let program = vab.tone_programs[0] as u8;
        let key = tones(&vab)[program as usize][0].min;
        let score = [
            0x00, 0xC0, program, 0x00, 0xB0, 99, 20, 0x00, 0x06, 127, // loop start, endless
            0x00, 0x90, key, 0x50, 0x83, 0x60, key, 0x00, // 480 ticks
            0x00, 0xB0, 99, 30, 0x00, 0xFF, 0x2F, 0x00,
        ];
        let once = render_bytes(&score, Overflow::Refuse, 0).unwrap();
        let twice = render_bytes(&score, Overflow::Refuse, 2).unwrap();
        assert_eq!(once.report["content_samples"], 22050);
        assert_eq!(twice.report["content_samples"], 3 * 22050);
        assert_eq!(twice.report["notes"], 3);
        let attr = [
            0x00, 0xB0, 99, 0, 0x00, 98, 4, 0x00, 6, 10, 0x00, 0xFF, 0x2F, 0x00,
        ];
        let err = render_bytes(&attr, Overflow::Refuse, 0).err().unwrap();
        assert_eq!(err.event.unwrap().1, 15 + 7);
        let bank_select = [0x00, 0xB0, 0, 1, 0x00, 0xFF, 0x2F, 0x00];
        assert_eq!(
            render_bytes(&bank_select, Overflow::Refuse, 0)
                .err()
                .unwrap()
                .event,
            Some((0, 15))
        );
    }
}
