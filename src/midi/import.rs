//! Reading a type 0 or 1 file strictly into an editable music project.

use super::*;

struct OpenNote {
    track: usize,
    start: u64,
    velocity: u8,
    location: String,
}

/// A track and channel pair; each becomes one imported voice.
type SourceVoice = (usize, u8);

struct ReadNote {
    start: u64,
    duration: u64,
    pitch: u8,
    velocity: u8,
    release: Option<u8>,
    voice: SourceVoice,
}

#[derive(Default)]
struct ImportState {
    tempos: BTreeMap<u64, (u32, String)>,
    tempo_tracks: BTreeMap<u64, usize>,
    time_signatures: BTreeMap<u64, (u8, u8, String)>,
    key_signatures: BTreeMap<u64, (i8, bool, String)>,
    markers: Vec<(u64, usize, Marker)>,
    track_names: HashMap<usize, String>,
    open: HashMap<(u8, u8), OpenNote>,
    notes: Vec<ReadNote>,
    lanes: BTreeMap<(SourceVoice, ControlKind), Vec<(ControlPoint, String)>>,
    rpn: HashMap<u8, (u8, u8)>,
    last_note_on: HashMap<SourceVoice, u64>,
    end: u64,
    preserve: bool,
    retained: Vec<Normalization>,
}

/// Reads a type 0 or type 1 file with metrical timing into a music project.
pub fn read_smf(bytes: &[u8]) -> Result<ImportedMidi, Vec<Diagnostic>> {
    read(bytes, false)
}

pub fn read_smf_preserving(bytes: &[u8]) -> Result<ImportedMidi, Vec<Diagnostic>> {
    read(bytes, true)
}

fn read(bytes: &[u8], preserve: bool) -> Result<ImportedMidi, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let mut normalizations = Vec::new();
    let smf = match Smf::parse(bytes) {
        Ok(smf) => smf,
        Err(error) => {
            return Err(vec![Diagnostic {
                code: Code::UnsupportedMidi,
                location: String::new(),
                message: format!("the file is not a well-formed Standard MIDI File: {error}"),
            }]);
        }
    };
    if preserve {
        diagnostics.extend(super::inspection::routing_blockers(&smf));
    }
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
        ..ImportState::default()
    };
    for (track_index, track) in smf.tracks.iter().enumerate() {
        let mut tick = 0u64;
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
                track_index,
                tick,
                event.kind,
                &mut report,
            );
        }
        state.end = state.end.max(tick);
    }
    for note in state.open.values() {
        report.push(
            Code::UnpairedNoteEvent,
            note.location.trim_start_matches('/'),
            "the note is never released",
        );
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let retained = std::mem::take(&mut state.retained);
    let composition = build_composition(state, &mut normalizations, &mut diagnostics);
    let (composition, assignments) = match composition {
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
    check_composition(
        timebase,
        &composition,
        &mut Report::new("/composition", &mut diagnostics),
    );
    if diagnostics.is_empty() {
        Ok(ImportedMidi {
            timebase,
            composition,
            assignments,
            normalizations,
            retained,
        })
    } else {
        Err(diagnostics)
    }
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
    track: usize,
    tick: u64,
    kind: TrackEventKind,
    report: &mut Report,
) {
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
            MetaMessage::MidiChannel(_) | MetaMessage::MidiPort(_) if state.preserve => {
                state.retained.push(Normalization {
                    location,
                    message: "routing metadata retained unchanged; the projection uses the file's single output port".into(),
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
            track,
            channel.as_int(),
            tick,
            message,
            report,
            location,
        ),
    }
}

fn read_channel_message(
    state: &mut ImportState,
    track: usize,
    channel: u8,
    tick: u64,
    message: MidiMessage,
    report: &mut Report,
    location: String,
) {
    let voice = (track, channel);
    let control = |state: &mut ImportState, kind: ControlKind, value: i32, report: &mut Report| {
        if !state.preserve && state.last_note_on.get(&voice) == Some(&tick) {
            report.push(
                Code::UnsupportedMidi,
                "",
                format!("a control change follows a note-on at tick {tick} on channel {}; the order would change", channel + 1),
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
                    "two different {kind:?} values at tick {tick} on channel {}",
                    channel + 1
                ),
            ),
            _ => lane.push((ControlPoint { at: tick, value }, location.clone())),
        }
    };
    match message {
        MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
            let key = key.as_int();
            if let Some(open) = state.open.get(&(channel, key)) {
                report.push(
                    Code::OverlappingNotes,
                    "",
                    format!(
                        "key {key} starts again on channel {} while the note from {} still sounds",
                        channel + 1,
                        open.location
                    ),
                );
                return;
            }
            state.open.insert(
                (channel, key),
                OpenNote {
                    track,
                    start: tick,
                    velocity: vel.as_int(),
                    location,
                },
            );
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
            match state.open.remove(&(channel, key)) {
                None => report.push(
                    Code::UnpairedNoteEvent,
                    "",
                    format!(
                        "key {key} is released on channel {} without sounding",
                        channel + 1
                    ),
                ),
                Some(open) if open.track != track => report.push(
                    Code::UnpairedNoteEvent,
                    "",
                    format!(
                        "key {key} started in track {} but is released in track {track}",
                        open.track
                    ),
                ),
                Some(open) if open.start == tick => report.push(
                    Code::NonPositiveDuration,
                    "",
                    format!("key {key} is released at the tick it starts"),
                ),
                Some(open) => state.notes.push(ReadNote {
                    start: open.start,
                    duration: tick - open.start,
                    pitch: key,
                    velocity: open.velocity,
                    release,
                    voice,
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
                CC_RPN_MSB => state.rpn.entry(channel).or_insert((RPN_NULL, RPN_NULL)).0 = value,
                CC_RPN_LSB => state.rpn.entry(channel).or_insert((RPN_NULL, RPN_NULL)).1 = value,
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

fn build_composition(
    mut state: ImportState,
    normalizations: &mut Vec<Normalization>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(Composition, Vec<VoiceAssignment>)> {
    let mut report = Report::new("", diagnostics);
    if state.end == 0 || state.notes.is_empty() {
        report.push(Code::InvalidRange, "", "the file has no notes to edit");
        return None;
    }

    let mut voice_keys: Vec<(usize, u8)> = state
        .notes
        .iter()
        .map(|n| n.voice)
        .chain(state.lanes.keys().map(|(v, _)| *v))
        .collect();
    voice_keys.sort();
    voice_keys.dedup();
    let voice_id = |(track, channel): (usize, u8)| format!("track{track}-ch{}", channel + 1);

    let mut tracks_per_channel: BTreeMap<u8, Vec<usize>> = BTreeMap::new();
    for (track, channel) in &voice_keys {
        tracks_per_channel.entry(*channel).or_default().push(*track);
    }
    for (channel, tracks) in tracks_per_channel.iter().filter(|(_, t)| t.len() > 1) {
        if let Some(((voice, kind), points)) =
            state.lanes.iter().find(|((v, _), _)| v.1 == *channel)
        {
            report.push(
                Code::SharedChannelControl,
                points[0].1.trim_start_matches('/'),
                format!(
                    "channel {} is used by tracks {tracks:?}; its {kind:?} changes from track {} would apply to all of them",
                    channel + 1,
                    voice.0
                ),
            );
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
    }

    let mut tempo_map: Vec<TempoChange> = state
        .tempos
        .iter()
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

    let mut notes: Vec<Note> = state
        .notes
        .iter()
        .map(|note| Note {
            id: String::new(),
            voice: voice_id(note.voice),
            start: note.start,
            duration: note.duration,
            pitch: note.pitch,
            velocity: note.velocity,
            release_velocity: note.release,
        })
        .collect();
    notes.sort_by(|a, b| (a.start, &a.voice, a.pitch).cmp(&(b.start, &b.voice, b.pitch)));
    for (index, note) in notes.iter_mut().enumerate() {
        note.id = format!("n{}", index + 1);
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
            channel,
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
    Some((composition, assignments))
}
