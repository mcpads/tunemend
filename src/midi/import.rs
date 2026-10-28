//! Reading a type 0 or 1 file strictly into an editable music project.

use std::collections::VecDeque;

use super::*;

struct OpenNote {
    start: u64,
    velocity: u8,
    location: String,
    event: SourceEvent,
    message: MidiMessage,
}

/// A track and receiver/channel key; each becomes one imported voice.
type SourceVoice = (usize, u16);

struct ReadNote {
    start: u64,
    duration: u64,
    pitch: u8,
    velocity: u8,
    release: Option<u8>,
    voice: SourceVoice,
    onset: SourceEvent,
    release_event: Option<SourceEvent>,
    declared: Option<DeclaredFrom>,
}

/// A never-released onset, or a zero-length pair with its release, waiting
/// for the selected [`DeclaredEnd`] once every track end is known.
struct Incomplete {
    channel: u16,
    key: u8,
    open: OpenNote,
    release: Option<(SourceEvent, MidiMessage, String)>,
}

#[derive(Default)]
struct ImportState {
    declared: Option<DeclaredEnd>,
    incomplete: Vec<Incomplete>,
    /// Every onset tick per track, channel and key, in order.
    onsets: HashMap<(usize, u16, u8), Vec<u64>>,
    track_ends: Vec<u64>,
    tempos: BTreeMap<u64, (u32, String)>,
    tempo_tracks: BTreeMap<u64, usize>,
    time_signatures: BTreeMap<u64, (u8, u8, String)>,
    key_signatures: BTreeMap<u64, (i8, bool, String)>,
    markers: Vec<(u64, usize, Marker)>,
    track_names: HashMap<usize, String>,
    /// Sounding onsets per track, channel and key, earliest first.
    open: HashMap<(usize, u16, u8), VecDeque<OpenNote>>,
    notes: Vec<ReadNote>,
    retained_notes: Vec<RetainedNoteEvent>,
    lanes: BTreeMap<(SourceVoice, ControlKind), Vec<(ControlPoint, String)>>,
    rpn: HashMap<u16, (u8, u8)>,
    nrpn: std::collections::HashSet<u16>,
    last_note_on: HashMap<SourceVoice, u64>,
    end: u64,
    preserve: bool,
    retained: Vec<Normalization>,
    /// How retained MIDI Port events route the projection.
    port_routing: String,
    multiple_receivers: bool,
    port: u8,
}

impl ImportState {
    /// Keeps a note event in the source without an editable note.
    #[allow(clippy::too_many_arguments)]
    fn retain_note(
        &mut self,
        event: SourceEvent,
        tick: u64,
        channel: u16,
        message: MidiMessage,
        location: String,
        reason: String,
        kind: RetainedKind,
    ) {
        self.retained_notes.push(RetainedNoteEvent {
            event,
            tick,
            channel,
            message,
            kind,
        });
        self.retained.push(Normalization {
            location,
            message: reason,
        });
    }
}

/// Reads a type 0 or type 1 file with metrical timing into a music project.
pub fn read_smf(bytes: &[u8]) -> Result<ImportedMidi, Vec<Diagnostic>> {
    read(bytes, false, None, false)
}

pub fn read_smf_preserving(bytes: &[u8]) -> Result<ImportedMidi, Vec<Diagnostic>> {
    read(bytes, true, None, false)
}

/// The preserving projection with an explicitly selected end for notes the
/// source leaves without a positive length; `None` retains them instead.
pub fn read_smf_declaring(
    bytes: &[u8],
    declared: Option<DeclaredEnd>,
) -> Result<ImportedMidi, Vec<Diagnostic>> {
    read(bytes, true, declared, false)
}

pub(super) fn read_smf_legacy_declaring(
    bytes: &[u8],
    declared: Option<DeclaredEnd>,
) -> Result<ImportedMidi, Vec<Diagnostic>> {
    read(bytes, true, declared, true)
}

fn read(
    bytes: &[u8],
    preserve: bool,
    declared: Option<DeclaredEnd>,
    legacy_declared: bool,
) -> Result<ImportedMidi, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let mut normalizations = Vec::new();
    let smf = match Smf::parse(bytes) {
        Ok(smf) => smf,
        Err(error) => {
            return Err(vec![track_count_mismatch(bytes).unwrap_or_else(|| {
                Diagnostic {
                    code: Code::UnsupportedMidi,
                    location: String::new(),
                    message: format!("the file is not a well-formed Standard MIDI File: {error}"),
                }
            })]);
        }
    };
    let mut report = Report::new("", &mut diagnostics);
    let ppq = match smf.header.timing {
        Timing::Metrical(ppq) if ppq.as_int() > 0 => ppq.as_int(),
        Timing::Metrical(_) => {
            report.push(Code::UnsupportedMidi, "header", "PPQ must be positive");
            0
        }
        Timing::Timecode(..) => {
            report.push(
                Code::UnsupportedMidi,
                "header",
                "SMPTE timecode files are not supported for editing",
            );
            0
        }
    };
    match smf.header.format {
        Format::Sequential => report.push(
            Code::UnsupportedMidi,
            "header",
            "type 2 files hold independent sequences and are not supported",
        ),
        Format::SingleTrack if !preserve => normalizations.push(Normalization {
            location: "/header".into(),
            message: "type 0 input will be exported as type 1".into(),
        }),
        Format::Parallel | Format::SingleTrack => {}
    }

    let mut state = ImportState {
        preserve,
        declared,
        multiple_receivers: multiple_receivers(&smf),
        port_routing: super::inspection::port_routing_note(&smf),
        ..ImportState::default()
    };
    for (track_index, track) in smf.tracks.iter().enumerate() {
        let mut tick = 0u64;
        state.port = 0;
        for (event_index, event) in track.iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            let mut report = report.nested(format!("tracks/{track_index}/events/{event_index}"));
            if matches!(event.kind, TrackEventKind::Meta(MetaMessage::EndOfTrack))
                && event_index + 1 < track.len()
            {
                report.push(Code::UnsupportedMidi, "", "events follow end-of-track; load_midi preserves them, but the editable projection cannot infer the intended playback boundary");
            }
            read_event(
                &mut state,
                &mut normalizations,
                (track_index, event_index),
                tick,
                event.kind,
                &mut report,
            );
        }
        state.end = state.end.max(tick);
        state.track_ends.push(tick);
    }
    let mut unreleased: Vec<(u16, u8, OpenNote)> = state
        .open
        .drain()
        .flat_map(|((_, channel, key), held)| held.into_iter().map(move |n| (channel, key, n)))
        .collect();
    unreleased.sort_by_key(|(_, _, note)| note.event);
    for (channel, key, note) in unreleased {
        if state.preserve && state.declared.is_some() {
            state.incomplete.push(Incomplete {
                channel,
                key,
                open: note,
                release: None,
            });
        } else if state.preserve {
            state.retain_note(
                note.event,
                note.start,
                channel,
                note.message,
                note.location,
                NEVER_RELEASED.into(),
                RetainedKind::NeverReleased,
            );
        } else {
            report.push(
                Code::UnpairedNoteEvent,
                note.location.trim_start_matches('/'),
                "the note is never released",
            );
        }
    }
    if let Some(rule) = state.declared.clone() {
        complete_notes(&mut state, &rule, &mut normalizations, legacy_declared);
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let mut retained = std::mem::take(&mut state.retained);
    let retained_note_events = std::mem::take(&mut state.retained_notes);
    let composition = build_composition(
        state,
        &retained_note_events,
        &mut retained,
        &mut normalizations,
        &mut diagnostics,
    );
    let (composition, assignments, note_bindings) = match composition {
        Some(built) => built,
        None => return Err(diagnostics),
    };
    if preserve {
        for item in &mut normalizations {
            item.message = format!(
                "editable projection only (original retained): {}",
                item.message
            );
        }
    }
    let timebase = Timebase::Music { ppq };
    check_composition_with(
        timebase,
        &composition,
        if preserve {
            StackedNotes::SourcePaired
        } else {
            StackedNotes::Reject
        },
        &mut Report::new("/composition", &mut diagnostics),
    );
    if diagnostics.is_empty() {
        Ok(ImportedMidi {
            timebase,
            composition,
            assignments,
            normalizations,
            retained,
            note_bindings,
            retained_note_events,
        })
    } else {
        Err(diagnostics)
    }
}

const NEVER_RELEASED: &str = "note never released; retained unchanged in source without an editable note, because no duration is inferred";

/// Gives never-released notes and zero-length pairs the selected end. The
/// source is not changed; the projection reports each declared end. A note
/// the rule does not apply to, whose rule gives no positive length, or whose
/// end would change first-in-first-out pairing of its key stays retained.
fn complete_notes(
    state: &mut ImportState,
    rule: &DeclaredEnd,
    normalizations: &mut Vec<Normalization>,
    legacy_declared: bool,
) {
    let mut incomplete = std::mem::take(&mut state.incomplete);
    incomplete.sort_by_key(|n| n.open.event);
    for Incomplete {
        channel,
        key,
        open,
        release,
    } in incomplete
    {
        let track = open.event.0;
        let track_end = state.track_ends[track];
        let end = match rule {
            DeclaredEnd::NextOnset { .. } => state.onsets[&(track, channel, key)]
                .iter()
                .copied()
                .find(|t| *t > open.start)
                .unwrap_or(track_end),
            DeclaredEnd::TrackEnd { .. } => track_end,
            DeclaredEnd::Fixed { ticks, .. } => open.start.saturating_add(*ticks).min(track_end),
        };
        let from = match release {
            Some(_) => DeclaredFrom::ZeroLength,
            None => DeclaredFrom::NeverReleased,
        };
        let what = match from {
            DeclaredFrom::ZeroLength => "zero-length note",
            DeclaredFrom::NeverReleased => "note never released",
        };
        let reason = if !rule.apply_to().contains(&from) {
            Some(match from {
                DeclaredFrom::ZeroLength => format!(
                    "zero-length note of key {key} (onset {}) retained unchanged in source; the selected declared end does not apply to zero-length pairs",
                    open.location
                ),
                DeclaredFrom::NeverReleased => format!(
                    "{NEVER_RELEASED}; the selected declared end does not apply to notes never released"
                ),
            })
        } else if end <= open.start {
            Some(format!(
                "{what} of key {key}: the selected end rule gives no positive length before the track end, so it is retained unchanged in source without an editable note"
            ))
        } else if legacy_declared {
            None
        } else {
            pairing_conflict(state, track, channel, key, open.event, open.start, end).map(|other| {
                format!(
                    "{what} of key {key}: the selected end at tick {end} would change first-in-first-out pairing with {other} of the same key, so it is retained unchanged in source without an editable note"
                )
            })
        };
        let kind = match from {
            DeclaredFrom::ZeroLength => RetainedKind::ZeroLength,
            DeclaredFrom::NeverReleased => RetainedKind::NeverReleased,
        };
        if let Some(reason) = reason {
            if let Some((event, message, location)) = release {
                state.retain_note(
                    open.event,
                    open.start,
                    channel,
                    open.message,
                    open.location.clone(),
                    reason.clone(),
                    kind,
                );
                state.retain_note(event, open.start, channel, message, location, reason, kind);
            } else {
                state.retain_note(
                    open.event,
                    open.start,
                    channel,
                    open.message,
                    open.location,
                    reason,
                    kind,
                );
            }
            continue;
        }
        normalizations.push(Normalization {
            location: open.location,
            message: format!("{what} of key {key} ends at tick {end} by the selected {} end; the source keeps its original events until an edit changes this note or an export writes declared ends", rule_name(rule)),
        });
        state.notes.push(ReadNote {
            start: open.start,
            duration: end - open.start,
            pitch: key,
            velocity: open.velocity,
            release: None,
            voice: (track, channel),
            onset: open.event,
            release_event: release.map(|(event, _, _)| event),
            declared: Some(from),
        });
    }
}

fn rule_name(rule: &DeclaredEnd) -> String {
    match rule {
        DeclaredEnd::NextOnset { .. } => "next_onset".into(),
        DeclaredEnd::TrackEnd { .. } => "track_end".into(),
        DeclaredEnd::Fixed { ticks, .. } => format!("fixed {ticks}-tick"),
    }
}

/// A written release at `end` keeps first-in-first-out pairing only when
/// every note of the key that starts earlier in the track ends no later,
/// every later one ends no earlier, and no release without a sounding note
/// falls inside the note.
fn pairing_conflict(
    state: &ImportState,
    track: usize,
    channel: u16,
    key: u8,
    onset: SourceEvent,
    start: u64,
    end: u64,
) -> Option<String> {
    let location = |event: SourceEvent| format!("/tracks/{}/events/{}", event.0, event.1);
    for note in state
        .notes
        .iter()
        .filter(|n| n.voice == (track, channel) && n.pitch == key)
    {
        let note_end = note.start + note.duration;
        if (note.onset < onset && note_end > end) || (note.onset > onset && note_end < end) {
            return Some(format!("the note at {}", location(note.onset)));
        }
    }
    state
        .retained_notes
        .iter()
        .find(|e| {
            e.kind == RetainedKind::ReleaseWithoutOnset
                && e.event.0 == track
                && e.channel == channel
                && matches!(e.message, MidiMessage::NoteOn { key: k, .. } | MidiMessage::NoteOff { key: k, .. } if k.as_int() == key)
                && e.event > onset
                && start <= e.tick
                && e.tick < end
        })
        .map(|e| format!("the release without a sounding note at {}", location(e.event)))
}

/// Locates a header whose declared track count differs from its track chunks.
fn track_count_mismatch(bytes: &[u8]) -> Option<Diagnostic> {
    let layout = super::source::source_layout(bytes).ok()?;
    let header = bytes.get(layout.smf_offset + 10..layout.smf_offset + 12)?;
    let declared = u16::from_be_bytes([header[0], header[1]]) as usize;
    let found = layout.chunks.iter().filter(|c| &c.id == b"MTrk").count();
    (declared != found).then(|| Diagnostic {
        code: Code::UnsupportedMidi,
        location: "/header".into(),
        message: format!("the header declares {declared} tracks but the file holds {found} track chunks; load_midi retains every byte, but the editable projection cannot decide which tracks form the song"),
    })
}

fn text(bytes: &[u8], normalizations: &mut Vec<Normalization>, location: &str) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => {
            normalizations.push(Normalization {
                location: location.to_owned(),
                message: "text is not UTF-8 and was decoded with replacement characters; the original file keeps the bytes".into(),
            });
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

fn read_event(
    state: &mut ImportState,
    normalizations: &mut Vec<Normalization>,
    event: SourceEvent,
    tick: u64,
    kind: TrackEventKind,
    report: &mut Report,
) {
    let track = event.0;
    if let TrackEventKind::Meta(MetaMessage::MidiPort(port)) = kind {
        state.port = port.as_int();
    }
    let location = report.location("");
    match kind {
        TrackEventKind::Meta(meta) => match meta {
            MetaMessage::TrackName(name) => {
                let name = text(name, normalizations, &location);
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    state.track_names.entry(track)
                {
                    entry.insert(name);
                } else {
                    normalizations.push(Normalization {
                        location,
                        message: format!("second track name `{name}` was not used"),
                    });
                }
            }
            MetaMessage::Tempo(tempo) => {
                let tempo = tempo.as_int();
                match state.tempos.get(&tick) {
                    Some((existing, previous)) if *existing != tempo && state.preserve
                        && state.tempo_tracks.get(&tick) == Some(&track) => {
                        state.retained.push(Normalization {
                            location: previous.clone(),
                            message: format!("ordered tempos at tick {tick} retained in source; the editable projection uses the final value {tempo}"),
                        });
                        state.tempos.insert(tick, (tempo, location));
                    }
                    Some((existing, _)) if *existing != tempo => report.push(
                        Code::UnsupportedMidi,
                        "",
                        format!("two different tempos at tick {tick}; the file does not say which applies"),
                    ),
                    Some(_) => normalizations.push(Normalization { location, message: "repeated tempo was merged".into() }),
                    None => {
                        state.tempo_tracks.insert(tick, track);
                        state.tempos.insert(tick, (tempo, location));
                    }
                }
            }
            MetaMessage::TimeSignature(numerator, power, clocks, thirty_seconds) => {
                if power > 7 {
                    report.push(
                        Code::InvalidTimeSignature,
                        "",
                        "time signature denominator is too large",
                    );
                    return;
                }
                if (clocks, thirty_seconds) != (24, 8) {
                    normalizations.push(Normalization {
                        location: location.clone(),
                        message: format!(
                            "the editable meter omits metronome fields {clocks}/{thirty_seconds}; normalized export uses 24/8"
                        ),
                    });
                }
                match state.time_signatures.get(&tick) {
                    Some((n, p, _)) if (*n, *p) != (numerator, 1 << power) => report.push(
                        Code::UnsupportedMidi,
                        "",
                        format!("two different time signatures at tick {tick}"),
                    ),
                    Some(_) => {}
                    None => {
                        state
                            .time_signatures
                            .insert(tick, (numerator, 1 << power, location));
                    }
                }
            }
            MetaMessage::KeySignature(sharps, minor) => match state.key_signatures.get(&tick) {
                Some((s, m, _)) if (*s, *m) != (sharps, minor) => report.push(
                    Code::UnsupportedMidi,
                    "",
                    format!("two different key signatures at tick {tick}"),
                ),
                Some(_) => {}
                None => {
                    state.key_signatures.insert(tick, (sharps, minor, location));
                }
            },
            MetaMessage::Marker(bytes)
            | MetaMessage::Text(bytes)
            | MetaMessage::CuePoint(bytes)
            | MetaMessage::Lyric(bytes)
            | MetaMessage::Copyright(bytes)
            | MetaMessage::InstrumentName(bytes)
            | MetaMessage::ProgramName(bytes)
            | MetaMessage::DeviceName(bytes) => {
                let kind = match meta {
                    MetaMessage::Marker(_) => MarkerKind::Marker,
                    MetaMessage::CuePoint(_) => MarkerKind::CuePoint,
                    MetaMessage::Lyric(_) => MarkerKind::Lyric,
                    MetaMessage::Copyright(_) => MarkerKind::Copyright,
                    MetaMessage::Text(_) => MarkerKind::Text,
                    _ => {
                        normalizations.push(Normalization {
                            location: location.clone(),
                            message: "instrument, program or device name is kept as a text marker"
                                .into(),
                        });
                        MarkerKind::Text
                    }
                };
                let text = text(bytes, normalizations, &location);
                if track > 0 {
                    normalizations.push(Normalization {
                        location: location.clone(),
                        message: "text from a voice track is kept on the shared timeline".into(),
                    });
                }
                state.markers.push((
                    tick,
                    track,
                    Marker {
                        at: tick,
                        kind,
                        text,
                    },
                ));
            }
            MetaMessage::EndOfTrack => {}
            MetaMessage::TrackNumber(_) => normalizations.push(Normalization {
                location,
                message: "sequence number is not used by a type 0 or 1 file and was dropped".into(),
            }),
            MetaMessage::MidiPort(_) if state.preserve => {
                state.retained.push(Normalization {
                    location,
                    message: state.port_routing.clone(),
                });
            }
            MetaMessage::MidiChannel(_) if state.preserve => {
                state.retained.push(Normalization {
                    location,
                    message: "channel prefix retained unchanged; it associates later metadata and SysEx with a channel and does not route events".into(),
                });
            }
            MetaMessage::MidiChannel(_) | MetaMessage::MidiPort(_) => report.push(
                Code::UnsupportedMidi,
                "",
                "channel prefix and port assignments are not supported",
            ),
            MetaMessage::SmpteOffset(_) if state.preserve => {
                state.retained.push(Normalization {
                    location,
                    message: "external SMPTE synchronization offset retained; the editable timeline starts at local tick zero".into(),
                });
            }
            MetaMessage::SmpteOffset(_) => {
                report.push(Code::UnsupportedMidi, "", "SMPTE offsets are not supported")
            }
            MetaMessage::SequencerSpecific(_) | MetaMessage::Unknown(..) if state.preserve => state.retained.push(Normalization {
                location, message: "opaque metadata retained unchanged in source".into(),
            }),
            MetaMessage::SequencerSpecific(_) | MetaMessage::Unknown(..) => report.push(
                Code::UnsupportedMidi,
                "",
                "sequencer-specific and unknown meta events are not supported",
            ),
        },
        TrackEventKind::SysEx(_) | TrackEventKind::Escape(_) if state.preserve => state.retained.push(Normalization {
            location, message: "system exclusive data retained unchanged in source; preview support is checked separately".into(),
        }),
        TrackEventKind::SysEx(_) | TrackEventKind::Escape(_) => report.push(
            Code::UnsupportedMidi,
            "",
            "system exclusive data is not supported for editing",
        ),
        TrackEventKind::Midi { channel, message } => read_channel_message(
            state,
            event,
            receiver_channel(state.multiple_receivers, state.port, channel.as_int()),
            tick,
            message,
            report,
            location,
        ),
    }
}

fn read_channel_message(
    state: &mut ImportState,
    event: SourceEvent,
    channel: u16,
    tick: u64,
    message: MidiMessage,
    report: &mut Report,
    location: String,
) {
    let track = event.0;
    let voice = (track, channel);
    let control = |state: &mut ImportState, kind: ControlKind, value: i32, report: &mut Report| {
        if !state.preserve && state.last_note_on.get(&voice) == Some(&tick) {
            report.push(
                Code::UnsupportedMidi,
                "",
                format!("a control change follows a note-on at tick {tick} on MIDI channel {}; the order would change", channel % 16 + 1),
            );
            return;
        }
        let lane = state.lanes.entry((voice, kind)).or_default();
        match lane.last_mut() {
            Some((point, _)) if point.at == tick && point.value == value => {}
            Some((point, previous)) if point.at == tick && state.preserve => {
                state.retained.push(Normalization {
                    location: previous.clone(),
                    message: format!("ordered {kind:?} values at tick {tick} retained in source; the editable projection shows the final value {value}"),
                });
                point.value = value;
                *previous = location.clone();
            }
            Some((point, _)) if point.at == tick => report.push(
                Code::ConflictingMidiControl,
                "",
                format!(
                    "two different {kind:?} values at tick {tick} on MIDI channel {}",
                    channel % 16 + 1
                ),
            ),
            _ => lane.push((ControlPoint { at: tick, value }, location.clone())),
        }
    };
    match message {
        MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
            let key = key.as_int();
            let held = state.open.entry((track, channel, key)).or_default();
            if let Some(open) = held.front() {
                // Each onset stays a separate note; releases pair first in,
                // first out. Only the strict normalized reader refuses.
                let message = format!(
                    "key {key} starts again on MIDI channel {} while the note from {} still sounds",
                    channel % 16 + 1,
                    open.location
                );
                if state.preserve {
                    state.retained.push(Normalization {
                        location: location.clone(),
                        message: format!("{message}; both onsets are separate notes and releases pair first in, first out"),
                    });
                } else {
                    report.push(Code::OverlappingNotes, "", message);
                }
            }
            state
                .onsets
                .entry((track, channel, key))
                .or_default()
                .push(tick);
            held.push_back(OpenNote {
                start: tick,
                velocity: vel.as_int(),
                location,
                event,
                message,
            });
            state.last_note_on.insert(voice, tick);
        }
        MidiMessage::NoteOn { key, .. } | MidiMessage::NoteOff { key, .. } => {
            let key = key.as_int();
            let release = match message {
                MidiMessage::NoteOff { vel, .. } if vel.as_int() != DEFAULT_RELEASE_VELOCITY => {
                    Some(vel.as_int())
                }
                _ => None,
            };
            let open = state
                .open
                .get_mut(&(track, channel, key))
                .and_then(VecDeque::pop_front);
            match open {
                None => {
                    let other = state
                        .open
                        .iter()
                        .find(|((t, c, k), held)| (*c, *k) == (channel, key) && *t != track && !held.is_empty())
                        .map(|((t, _, _), _)| *t);
                    let problem = match other {
                        Some(other) => format!("key {key} started in track {other} but is released in track {track}"),
                        None => format!("key {key} is released on MIDI channel {} without sounding", channel % 16 + 1),
                    };
                    if state.preserve {
                        state.retain_note(event, tick, channel, message, location, format!("{problem}; the release is retained unchanged in source without an editable note"), RetainedKind::ReleaseWithoutOnset);
                    } else {
                        report.push(Code::UnpairedNoteEvent, "", problem);
                    }
                }
                Some(open) if open.start == tick => {
                    if state.preserve && state.declared.is_some() {
                        state.incomplete.push(Incomplete {
                            channel,
                            key,
                            open,
                            release: Some((event, message, location)),
                        });
                    } else if state.preserve {
                        let reason = format!("zero-length note of key {key} (onset {}) retained unchanged in source; editable notes have positive durations", open.location);
                        state.retain_note(open.event, tick, channel, open.message, open.location, reason.clone(), RetainedKind::ZeroLength);
                        state.retain_note(event, tick, channel, message, location, reason, RetainedKind::ZeroLength);
                    } else {
                        report.push(
                            Code::NonPositiveDuration,
                            "",
                            format!("key {key} is released at the tick it starts"),
                        );
                    }
                }
                Some(open) => state.notes.push(ReadNote {
                    start: open.start,
                    duration: tick - open.start,
                    pitch: key,
                    velocity: open.velocity,
                    release,
                    voice,
                    onset: open.event,
                    release_event: Some(event),
                    declared: None,
                }),
            }
        }
        MidiMessage::ProgramChange { program } => control(
            state,
            ControlKind::Program,
            i32::from(program.as_int()),
            report,
        ),
        MidiMessage::PitchBend { bend } => control(
            state,
            ControlKind::PitchBend,
            i32::from(bend.as_int()),
            report,
        ),
        MidiMessage::Controller { controller, value } => {
            let (controller, value) = (controller.as_int(), value.as_int());
            match controller {
                CC_MODULATION => control(state, ControlKind::Modulation, i32::from(value), report),
                CC_REVERB => control(state, ControlKind::Reverb, i32::from(value), report),
                CC_CHORUS => control(state, ControlKind::Chorus, i32::from(value), report),
                CC_VOLUME => control(state, ControlKind::Volume, i32::from(value), report),
                CC_PAN => control(state, ControlKind::Pan, i32::from(value), report),
                CC_EXPRESSION => control(state, ControlKind::Expression, i32::from(value), report),
                CC_RPN_MSB => {
                    state.nrpn.remove(&channel);
                    state.rpn.entry(channel).or_insert((RPN_NULL, RPN_NULL)).0 = value;
                },
                CC_RPN_LSB => {
                    state.nrpn.remove(&channel);
                    state.rpn.entry(channel).or_insert((RPN_NULL, RPN_NULL)).1 = value;
                },
                98 | 99 if state.preserve => {
                    state.nrpn.insert(channel);
                    state.rpn.remove(&channel);
                    state.retained.push(Normalization { location, message: format!("CC{controller} selects an opaque NRPN; retained unchanged in source") });
                },
                CC_DATA_ENTRY | CC_DATA_ENTRY_LSB if state.preserve && state.nrpn.contains(&channel) => {
                    state.retained.push(Normalization { location, message: format!("NRPN data entry CC{controller} retained unchanged; no editable parameter meaning is inferred") });
                },
                CC_DATA_ENTRY if state.rpn.get(&channel) == Some(&(0, 0)) => {
                    control(state, ControlKind::PitchBendRange, i32::from(value), report)
                }
                CC_DATA_ENTRY_LSB if state.rpn.get(&channel) == Some(&(0, 0)) && value == 0 => {}
                CC_DATA_ENTRY_LSB if state.rpn.get(&channel) == Some(&(0, 0)) => {
                    report.push(Code::UnsupportedMidi, "", "pitch bend ranges with cents are not supported")
                }
                CC_DATA_ENTRY | CC_DATA_ENTRY_LSB => {
                    report.push(Code::UnsupportedMidi, "", "data entry for parameters other than RPN 0 is not supported")
                }
                other if state.preserve => state.retained.push(Normalization {
                    location, message: format!("CC{other} retained unchanged in source; not exposed as an editable control lane"),
                }),
                other => report.push(
                    Code::UnsupportedMidiController,
                    "",
                    format!("controller {other} is not supported for editing; it may change how notes sound"),
                ),
            }
        }
        MidiMessage::Aftertouch { .. } | MidiMessage::ChannelAftertouch { .. }
            if state.preserve =>
        {
            state.retained.push(Normalization {
                location,
                message:
                    "aftertouch retained unchanged in source; not exposed as an editable control lane"
                        .into(),
            })
        }
        MidiMessage::Aftertouch { .. } | MidiMessage::ChannelAftertouch { .. } => report.push(
            Code::UnsupportedMidi,
            "",
            "aftertouch is not supported for editing",
        ),
    }
}

type Built = (Composition, Vec<VoiceAssignment>, Vec<NoteBinding>);

fn build_composition(
    mut state: ImportState,
    retained_notes: &[RetainedNoteEvent],
    retained: &mut Vec<Normalization>,
    normalizations: &mut Vec<Normalization>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Built> {
    let mut report = Report::new("", diagnostics);
    if state.end == 0 || state.notes.is_empty() {
        report.push(Code::InvalidRange, "", "the file has no notes to edit");
        return None;
    }

    let mut voice_keys: Vec<(usize, u16)> = state
        .notes
        .iter()
        .map(|n| n.voice)
        .chain(state.lanes.keys().map(|(v, _)| *v))
        .chain(retained_notes.iter().map(|e| (e.event.0, e.channel)))
        .collect();
    voice_keys.sort();
    voice_keys.dedup();
    let voice_id = |(track, channel)| source_voice(track, channel);

    let mut tracks_per_channel: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    for (track, channel) in &voice_keys {
        tracks_per_channel.entry(*channel).or_default().push(*track);
    }
    for (channel, tracks) in tracks_per_channel.iter().filter(|(_, t)| t.len() > 1) {
        if let Some(((voice, kind), points)) =
            state.lanes.iter().find(|((v, _), _)| v.1 == *channel)
        {
            let message = format!(
                "MIDI channel {} is used by tracks {tracks:?}; its {kind:?} changes from track {} would apply to all of them",
                channel % 16 + 1,
                voice.0
            );
            if state.preserve {
                // Each control event stays in its own track and lane; the
                // receiver state it sets is checked against every voice.
                retained.push(Normalization {
                    location: points[0].1.clone(),
                    message: format!("{message}. Lanes stay with the track that holds their events; a lane change must be in scope for, and is protected by, every voice on the channel"),
                });
            } else {
                report.push(
                    Code::SharedChannelControl,
                    points[0].1.trim_start_matches('/'),
                    message,
                );
            }
        }
    }
    if !diagnostics.is_empty() {
        return None;
    }

    // End-boundary controller cleanup affects the original render's release
    // tail, but is outside the editable half-open timeline. Preserve it in SMF.
    if state.preserve {
        for points in state.lanes.values_mut() {
            points.retain(|(point, location)| {
                if point.at >= state.end {
                    normalizations.push(Normalization {
                        location: location.clone(),
                        message:
                            "terminal control retained in source outside the editable timeline"
                                .into(),
                    });
                    false
                } else {
                    true
                }
            });
        }
        state.lanes.retain(|_, points| !points.is_empty());
        for (tick, (_, location)) in &state.tempos {
            if *tick >= state.end {
                normalizations.push(Normalization {
                    location: location.clone(),
                    message: "terminal tempo retained in source outside the editable timeline"
                        .into(),
                });
            }
        }
    }

    let mut tempo_map: Vec<TempoChange> = state
        .tempos
        .iter()
        .filter(|(tick, _)| !state.preserve || **tick < state.end)
        .map(|(tick, (tempo, _))| TempoChange {
            tick: *tick,
            microseconds_per_quarter: *tempo,
        })
        .collect();
    if tempo_map.first().is_none_or(|t| t.tick != 0) {
        tempo_map.insert(
            0,
            TempoChange {
                tick: 0,
                microseconds_per_quarter: DEFAULT_MICROSECONDS_PER_QUARTER,
            },
        );
        normalizations.push(Normalization {
            location: "/tracks/0".into(),
            message: format!("no tempo at tick 0; assumed the MIDI default {DEFAULT_MICROSECONDS_PER_QUARTER} µs per quarter note"),
        });
    }

    let mut lanes: Vec<ControlLane> = state
        .lanes
        .iter()
        .map(|((voice, kind), points)| ControlLane {
            voice: voice_id(*voice),
            kind: *kind,
            points: points.iter().map(|(p, _)| *p).collect(),
        })
        .collect();
    let bend_voices: Vec<(String, u64)> = lanes
        .iter()
        .filter(|l| l.kind == ControlKind::PitchBend)
        .map(|l| (l.voice.clone(), l.points[0].at))
        .collect();
    for (voice, first_bend) in bend_voices {
        let range = lanes
            .iter_mut()
            .find(|l| l.voice == voice && l.kind == ControlKind::PitchBendRange);
        let needs_default = range.as_ref().is_none_or(|l| l.points[0].at > first_bend);
        if needs_default {
            let point = ControlPoint {
                at: 0,
                value: DEFAULT_PITCH_BEND_RANGE,
            };
            match range {
                Some(lane) => lane.points.insert(0, point),
                None => lanes.push(ControlLane {
                    voice: voice.clone(),
                    kind: ControlKind::PitchBendRange,
                    points: vec![point],
                }),
            }
            normalizations.push(Normalization {
                location: "/tracks".into(),
                message: format!("`{voice}` bends before setting a range; assumed ±{DEFAULT_PITCH_BEND_RANGE} semitones from tick 0"),
            });
        }
    }

    if state.preserve {
        report_shared_keys(&state.notes, retained);
    }
    // Source onset order breaks ties between stacked onsets at one tick.
    state
        .notes
        .sort_by_key(|n| (n.start, voice_id(n.voice), n.pitch, n.onset));
    let mut notes = Vec::with_capacity(state.notes.len());
    let mut bindings = Vec::with_capacity(state.notes.len());
    for (index, note) in state.notes.iter().enumerate() {
        let id = format!("n{}", index + 1);
        bindings.push(NoteBinding {
            id: id.clone(),
            onset: note.onset,
            release: note.release_event,
            declared: note.declared,
        });
        notes.push(Note {
            id,
            voice: voice_id(note.voice),
            start: note.start,
            duration: note.duration,
            pitch: note.pitch,
            velocity: note.velocity,
            release_velocity: note.release,
        });
    }

    let mut markers = state.markers;
    markers.sort_by_key(|(tick, track, _)| (*tick, *track));
    let loop_markers: Vec<&Marker> = markers
        .iter()
        .map(|(_, _, m)| m)
        .filter(|m| m.text == LOOP_START_MARKER || m.text == LOOP_END_MARKER)
        .collect();
    if !loop_markers.is_empty() {
        normalizations.push(Normalization {
            location: "/tracks".into(),
            message:
                "loop markers were kept as markers; declare the loop explicitly if it should repeat"
                    .into(),
        });
    }

    let assignments = voice_keys
        .iter()
        .map(|&(track, channel)| VoiceAssignment {
            voice: voice_id((track, channel)),
            track: state
                .track_names
                .get(&track)
                .cloned()
                .unwrap_or_else(|| format!("Track {track}")),
            channel: (channel % 16) as u8,
            port: (channel >= 16).then_some((channel / 16) as u8),
        })
        .collect();
    for (track, name) in &state.track_names {
        if name != "Conductor" && !voice_keys.iter().any(|(t, _)| t == track) {
            normalizations.push(Normalization {
                location: format!("/tracks/{track}"),
                message: format!("track name `{name}` belongs to a track without notes and is written as `Conductor`"),
            });
        }
    }

    let composition = Composition {
        tempo_map,
        time_signatures: state
            .time_signatures
            .iter()
            .map(|(tick, (numerator, denominator, _))| TimeSignature {
                tick: *tick,
                numerator: *numerator,
                denominator: *denominator,
            })
            .collect(),
        key_signatures: state
            .key_signatures
            .iter()
            .map(|(tick, (sharps, minor, _))| KeySignature {
                tick: *tick,
                sharps: *sharps,
                minor: *minor,
            })
            .collect(),
        markers: markers.into_iter().map(|(_, _, m)| m).collect(),
        voices: voice_keys
            .iter()
            .map(|v| Voice {
                id: voice_id(*v),
                role: None,
            })
            .collect(),
        notes,
        control_lanes: lanes,
        mix: Default::default(),
        sections: vec![],
        playback: Playback {
            end: state.end,
            loop_start: None,
        },
    };
    Some((composition, assignments, bindings))
}

/// Tracks are read one after another, so notes of one channel and key that
/// overlap in time from different tracks are found after pairing.
fn report_shared_keys(notes: &[ReadNote], retained: &mut Vec<Normalization>) {
    let mut by_key: BTreeMap<(u16, u8), Vec<&ReadNote>> = BTreeMap::new();
    for note in notes {
        by_key
            .entry((note.voice.1, note.pitch))
            .or_default()
            .push(note);
    }
    for ((channel, key), mut notes) in by_key {
        notes.sort_by_key(|n| (n.start, n.onset));
        let mut sounding: Vec<&ReadNote> = Vec::new();
        for note in notes {
            sounding.retain(|held| held.start + held.duration > note.start);
            if let Some(other) = sounding.iter().find(|held| held.voice.0 != note.voice.0) {
                retained.push(Normalization {
                    location: format!("/tracks/{}/events/{}", note.onset.0, note.onset.1),
                    message: format!("key {key} on MIDI channel {} also sounds from track {}; each track keeps its own note, but one receiver pairs releases of this key first in, first out across tracks", channel % 16 + 1, other.voice.0),
                });
            }
            sounding.push(note);
        }
    }
}
