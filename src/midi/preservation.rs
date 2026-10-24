//! Surgical MIDI edits retain every event outside the explicitly edited fields.

use super::*;
use serde_json::{Value, json};

fn refuse(location: &str, message: &str) -> Vec<Diagnostic> {
    vec![Diagnostic {
        code: Code::UnsupportedMidi,
        location: location.into(),
        message: message.into(),
    }]
}

fn source_projection(source: &MidiSource) -> Result<ImportedMidi, Vec<Diagnostic>> {
    if crate::store::sha256_hex(&source.bytes) != source.sha256 {
        return Err(refuse(
            "/midi_source",
            "the original MIDI bytes do not match their recorded hash",
        ));
    }
    read_smf_preserving(&source.bytes)
}

pub fn check_source_edit(
    source: &MidiSource,
    timebase: Timebase,
    composition: &Composition,
) -> Result<(), Vec<Diagnostic>> {
    let imported = source_projection(source)?;
    let old = &imported.composition;
    if timebase != imported.timebase
        || composition.voices.iter().map(|v| &v.id).collect::<Vec<_>>()
            != old.voices.iter().map(|v| &v.id).collect::<Vec<_>>()
    {
        return Err(refuse(
            "/composition/voices",
            "source-backed MIDI retains its track/channel identities and PPQ; edit notes within existing voices. Voice/channel remapping is not implemented losslessly",
        ));
    }
    if composition.markers != old.markers || composition.playback != old.playback {
        return Err(refuse(
            "/composition/playback",
            "source-backed MIDI retains its original markers and end/loop boundaries; changing them is not yet implemented losslessly",
        ));
    }
    let ranges = |c: &Composition| {
        c.control_lanes
            .iter()
            .filter(|lane| lane.kind == ControlKind::PitchBendRange)
            .cloned()
            .collect::<Vec<_>>()
    };
    if ranges(composition) != ranges(old) {
        return Err(refuse(
            "/composition/control_lanes",
            "changing imported RPN pitch-bend ranges is not yet implemented losslessly; existing RPN events are retained",
        ));
    }
    Ok(())
}

fn direct_control(message: MidiMessage) -> Option<(ControlKind, i32)> {
    match message {
        MidiMessage::ProgramChange { program } => {
            Some((ControlKind::Program, program.as_int().into()))
        }
        MidiMessage::PitchBend { bend } => Some((ControlKind::PitchBend, bend.as_int().into())),
        MidiMessage::Controller { controller, value } => {
            let kind = match controller.as_int() {
                CC_MODULATION => ControlKind::Modulation,
                CC_VOLUME => ControlKind::Volume,
                CC_PAN => ControlKind::Pan,
                CC_EXPRESSION => ControlKind::Expression,
                CC_REVERB => ControlKind::Reverb,
                CC_CHORUS => ControlKind::Chorus,
                _ => return None,
            };
            Some((kind, value.as_int().into()))
        }
        _ => None,
    }
}

fn control_message(kind: ControlKind, value: i32) -> MidiMessage {
    match kind {
        ControlKind::Program => MidiMessage::ProgramChange {
            program: u7::new(value as u8),
        },
        ControlKind::PitchBend => MidiMessage::PitchBend {
            bend: PitchBend::from_int(value as i16),
        },
        _ => MidiMessage::Controller {
            controller: u7::new(match kind {
                ControlKind::Modulation => CC_MODULATION,
                ControlKind::Volume => CC_VOLUME,
                ControlKind::Pan => CC_PAN,
                ControlKind::Expression => CC_EXPRESSION,
                ControlKind::Reverb => CC_REVERB,
                ControlKind::Chorus => CC_CHORUS,
                _ => unreachable!("range lanes are not rewritten"),
            }),
            value: u7::new(value as u8),
        },
    }
}

/// No edits: exact source bytes. Edited notes/controls/tempo/meter/key: retain
/// unrelated event bytes, absolute ticks, original tracks and same-tick order.
pub fn write_preserving(
    source: &MidiSource,
    timebase: Timebase,
    composition: &Composition,
    plan: &MidiExport,
) -> Result<Vec<u8>, Vec<Diagnostic>> {
    check_source_edit(source, timebase, composition)?;
    let imported = source_projection(source)?;
    if plan.assignments != imported.assignments || plan.timing.is_some() {
        return Err(refuse(
            "/assignments",
            "lossless source export uses the imported track/channel assignments and timing",
        ));
    }
    let old = &imported.composition;
    // Roles, sections and the render mixer are project metadata, not SMF events.
    let mut comparable = composition.clone();
    comparable.voices = old.voices.clone();
    comparable.sections = old.sections.clone();
    comparable.mix = old.mix.clone();
    if &comparable == old {
        return Ok(source.bytes.clone());
    }
    let mut diagnostics = Vec::new();
    check_composition(
        timebase,
        composition,
        &mut Report::new("/composition", &mut diagnostics),
    );
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let smf = Smf::parse(&source.bytes).map_err(|e| refuse("/midi_source", &e.to_string()))?;
    let old_notes: HashMap<_, _> = old
        .notes
        .iter()
        .map(|n| ((n.voice.clone(), n.start, n.pitch), n))
        .collect();
    let new_notes: HashMap<_, _> = composition
        .notes
        .iter()
        .map(|n| (n.id.as_str(), n))
        .collect();
    let old_ids: std::collections::HashSet<_> = old.notes.iter().map(|n| n.id.as_str()).collect();
    let mut changed_lanes = BTreeMap::new();
    for lane in old.control_lanes.iter().chain(&composition.control_lanes) {
        if lane.kind == ControlKind::PitchBendRange {
            continue;
        }
        let key = (lane.voice.clone(), lane.kind);
        let previous = old
            .control_lanes
            .iter()
            .find(|l| l.voice == key.0 && l.kind == key.1);
        let next = composition
            .control_lanes
            .iter()
            .find(|l| l.voice == key.0 && l.kind == key.1);
        if previous != next {
            changed_lanes.insert(key, next);
        }
    }
    let tempo_changed = old.tempo_map != composition.tempo_map;
    let meter_changed = old.time_signatures != composition.time_signatures;
    let key_changed = old.key_signatures != composition.key_signatures;
    let mut tracks = Vec::new();
    for (track_index, track) in smf.tracks.iter().enumerate() {
        let mut tick = 0u64;
        let mut last_control = BTreeMap::new();
        for (index, event) in track.iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            if let TrackEventKind::Midi { channel, message } = event.kind
                && let Some((kind, _)) = direct_control(message)
            {
                last_control.insert((channel.as_int(), kind, tick), index);
            }
        }
        let mut events = Vec::new();
        let mut held = HashMap::new();
        tick = 0;
        for (index, event) in track.iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            let mut at = tick;
            let mut order = index as i64;
            let mut kind = event.kind;
            match kind {
                TrackEventKind::Meta(MetaMessage::Tempo(_)) if tempo_changed => {
                    let Some(point) = composition.tempo_map.iter().find(|p| p.tick == tick) else {
                        continue;
                    };
                    kind = TrackEventKind::Meta(MetaMessage::Tempo(u24::new(
                        point.microseconds_per_quarter,
                    )));
                }
                TrackEventKind::Meta(MetaMessage::TimeSignature(_, _, clocks, thirty_seconds))
                    if meter_changed =>
                {
                    let Some(point) = composition.time_signatures.iter().find(|p| p.tick == tick)
                    else {
                        continue;
                    };
                    kind = TrackEventKind::Meta(MetaMessage::TimeSignature(
                        point.numerator,
                        point.denominator.trailing_zeros() as u8,
                        clocks,
                        thirty_seconds,
                    ));
                }
                TrackEventKind::Meta(MetaMessage::KeySignature(..)) if key_changed => {
                    let Some(point) = composition.key_signatures.iter().find(|p| p.tick == tick)
                    else {
                        continue;
                    };
                    kind =
                        TrackEventKind::Meta(MetaMessage::KeySignature(point.sharps, point.minor));
                }
                _ => {}
            }
            if let TrackEventKind::Midi { channel, message } = kind {
                let voice = format!("track{track_index}-ch{}", channel.as_int() + 1);
                match message {
                    MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                        let original =
                            old_notes.get(&(voice, tick, key.as_int())).ok_or_else(|| {
                                refuse("/notes", "source note binding could not be recovered")
                            })?;
                        held.insert((channel.as_int(), key.as_int()), original.id.as_str());
                        let Some(note) = new_notes.get(original.id.as_str()) else {
                            continue;
                        };
                        if note.voice != original.voice {
                            return Err(refuse(
                                "/notes",
                                "moving an imported note to another voice is not yet implemented losslessly",
                            ));
                        }
                        at = note.start;
                        if at != tick {
                            order = i64::MAX - 1;
                        }
                        kind = TrackEventKind::Midi {
                            channel,
                            message: MidiMessage::NoteOn {
                                key: u7::new(note.pitch),
                                vel: u7::new(note.velocity),
                            },
                        };
                    }
                    MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                        let id =
                            held.remove(&(channel.as_int(), key.as_int()))
                                .ok_or_else(|| {
                                    refuse("/notes", "source note release could not be bound")
                                })?;
                        let Some(note) = new_notes.get(id) else {
                            continue;
                        };
                        at = note.start + note.duration;
                        if at != tick {
                            order = i64::MIN;
                        }
                        let off = match (message, note.release_velocity) {
                            (MidiMessage::NoteOn { .. }, None) => MidiMessage::NoteOn {
                                key: u7::new(note.pitch),
                                vel: u7::new(0),
                            },
                            _ => MidiMessage::NoteOff {
                                key: u7::new(note.pitch),
                                vel: u7::new(
                                    note.release_velocity.unwrap_or(DEFAULT_RELEASE_VELOCITY),
                                ),
                            },
                        };
                        kind = TrackEventKind::Midi {
                            channel,
                            message: off,
                        };
                    }
                    _ => {
                        if let Some((control, _)) = direct_control(message)
                            && let Some(next) = changed_lanes.get(&(voice, control))
                        {
                            let point =
                                next.and_then(|lane| lane.points.iter().find(|p| p.at == tick));
                            let Some(point) = point else {
                                continue;
                            };
                            if last_control[&(channel.as_int(), control, tick)] == index {
                                kind = TrackEventKind::Midi {
                                    channel,
                                    message: control_message(control, point.value),
                                };
                            }
                        }
                    }
                }
            }
            events.push((at, order, kind));
        }
        for note in composition
            .notes
            .iter()
            .filter(|n| !old_ids.contains(n.id.as_str()))
        {
            let Some(assignment) = imported.assignments.iter().find(|a| a.voice == note.voice)
            else {
                continue;
            };
            if note.voice != format!("track{track_index}-ch{}", assignment.channel + 1) {
                continue;
            }
            let channel = u4::new(assignment.channel);
            events.push((
                note.start,
                i64::MAX - 1,
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::NoteOn {
                        key: u7::new(note.pitch),
                        vel: u7::new(note.velocity),
                    },
                },
            ));
            events.push((
                note.start + note.duration,
                i64::MIN,
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::NoteOff {
                        key: u7::new(note.pitch),
                        vel: u7::new(note.release_velocity.unwrap_or(DEFAULT_RELEASE_VELOCITY)),
                    },
                },
            ));
        }
        for ((voice, control), lane) in &changed_lanes {
            let Some(assignment) = imported.assignments.iter().find(|a| &a.voice == voice) else {
                continue;
            };
            if *voice != format!("track{track_index}-ch{}", assignment.channel + 1) {
                continue;
            }
            if let Some(lane) = lane {
                for point in &lane.points {
                    if !last_control.contains_key(&(assignment.channel, *control, point.at)) {
                        events.push((
                            point.at,
                            i64::MIN + 1,
                            TrackEventKind::Midi {
                                channel: u4::new(assignment.channel),
                                message: control_message(*control, point.value),
                            },
                        ));
                    }
                }
            }
        }
        if track_index == 0 {
            if tempo_changed {
                events.extend(
                    composition
                        .tempo_map
                        .iter()
                        .filter(|t| !has_meta_at(&smf, t.tick, 0))
                        .map(|t| {
                            (
                                t.tick,
                                i64::MIN + 1,
                                TrackEventKind::Meta(MetaMessage::Tempo(u24::new(
                                    t.microseconds_per_quarter,
                                ))),
                            )
                        }),
                );
            }
            if meter_changed {
                events.extend(
                    composition
                        .time_signatures
                        .iter()
                        .filter(|t| !has_meta_at(&smf, t.tick, 1))
                        .map(|t| {
                            (
                                t.tick,
                                i64::MIN + 1,
                                TrackEventKind::Meta(MetaMessage::TimeSignature(
                                    t.numerator,
                                    t.denominator.trailing_zeros() as u8,
                                    24,
                                    8,
                                )),
                            )
                        }),
                );
            }
            if key_changed {
                events.extend(
                    composition
                        .key_signatures
                        .iter()
                        .filter(|t| !has_meta_at(&smf, t.tick, 2))
                        .map(|t| {
                            (
                                t.tick,
                                i64::MIN + 1,
                                TrackEventKind::Meta(MetaMessage::KeySignature(t.sharps, t.minor)),
                            )
                        }),
                );
            }
        }
        let last_event = events
            .iter()
            .filter(|(_, _, kind)| !matches!(kind, TrackEventKind::Meta(MetaMessage::EndOfTrack)))
            .map(|(at, _, _)| *at)
            .max()
            .unwrap_or(0);
        for (at, _, kind) in &mut events {
            if matches!(kind, TrackEventKind::Meta(MetaMessage::EndOfTrack)) {
                *at = (*at).max(last_event);
            }
        }
        events.sort_by_key(|(at, order, _)| (*at, *order));
        let mut previous = 0;
        let mut output = Vec::new();
        for (at, _, kind) in events {
            if at - previous > 0x0fff_ffff {
                return Err(refuse("/notes", "the edited delta time does not fit MIDI"));
            }
            output.push(TrackEvent {
                delta: u28::new((at - previous) as u32),
                kind,
            });
            previous = at;
        }
        tracks.push(output);
    }
    let bytes = super::source::write_source_tracks(&source.bytes, &tracks)?;
    // Detect newly ambiguous note pairing, e.g. a pitch edit creating overlap.
    let readback = read_smf_preserving(&bytes)?;
    let normalize = |c: &Composition| {
        let mut c = c.clone();
        for note in &mut c.notes {
            note.id.clear();
            // The projection treats note-on velocity zero as an unspecified release.
            note.release_velocity = Some(note.release_velocity.unwrap_or(DEFAULT_RELEASE_VELOCITY));
        }
        c.notes.sort_by_key(|n| {
            (
                n.start,
                n.voice.clone(),
                n.pitch,
                n.duration,
                n.velocity,
                n.release_velocity,
            )
        });
        c.voices = old.voices.clone();
        c.sections.clear();
        c.mix = Default::default();
        c.control_lanes.sort_by_key(|l| (l.voice.clone(), l.kind));
        c
    };
    if normalize(&readback.composition) != normalize(composition) {
        return Err(refuse(
            "/composition",
            "the edited MIDI does not read back to the requested composition; no output was accepted",
        ));
    }
    Ok(bytes)
}

pub fn compare_midi(reference: &[u8], candidate: &[u8]) -> Value {
    let parsed = (
        midly::SmfBytemap::parse(reference),
        midly::SmfBytemap::parse(candidate),
    );
    let (Ok(reference_smf), Ok(candidate_smf)) = parsed else {
        return json!({"comparable": false, "byte_identical": reference == candidate,
            "events_identical": false, "reason": "both inputs must be well-formed Standard MIDI Files"});
    };
    let mut differences = Vec::new();
    if reference_smf.header != candidate_smf.header {
        differences.push(json!({"location": "/header", "reference": format!("{:?}", reference_smf.header), "candidate": format!("{:?}", candidate_smf.header)}));
    }
    if reference_smf.tracks.len() != candidate_smf.tracks.len() {
        differences.push(json!({"location": "/tracks", "reference": reference_smf.tracks.len(), "candidate": candidate_smf.tracks.len()}));
    }
    let mut event_differences = 0;
    for (track, (left, right)) in reference_smf
        .tracks
        .iter()
        .zip(&candidate_smf.tracks)
        .enumerate()
    {
        if left.len() != right.len() {
            differences.push(json!({"location": format!("/tracks/{track}"), "reference_events": left.len(), "candidate_events": right.len()}));
        }
        let (mut left_tick, mut right_tick) = (0u64, 0u64);
        for (index, ((raw_a, a), (raw_b, b))) in left.iter().zip(right).enumerate() {
            left_tick += u64::from(a.delta.as_int());
            right_tick += u64::from(b.delta.as_int());
            if left_tick != right_tick
                || a.kind != b.kind
                || super::source::meta_payload(raw_a) != super::source::meta_payload(raw_b)
            {
                event_differences += 1;
                if differences.len() < 20 {
                    differences.push(json!({"location": format!("/tracks/{track}/events/{index}"), "reference_tick": left_tick, "candidate_tick": right_tick, "reference": format!("{:?}", a.kind), "candidate": format!("{:?}", b.kind)}));
                }
            }
        }
    }
    json!({"comparable": true, "byte_identical": reference == candidate,
        "events_identical": reference_smf.header == candidate_smf.header && reference_smf.tracks.len() == candidate_smf.tracks.len() && reference_smf.tracks.iter().zip(&candidate_smf.tracks).all(|(a,b)| a.len() == b.len()) && event_differences == 0,
        "container_identical": match (super::source::container_data(reference), super::source::container_data(candidate)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }, "aligned_event_differences": event_differences,
        "differences": differences, "reference_sha256": crate::store::sha256_hex(reference),
        "candidate_sha256": crate::store::sha256_hex(candidate),
        "scope": "MIDI headers, tracks, every event class, exact ticks and event order. This does not compare audio or prove target playback."})
}

fn has_meta_at(smf: &Smf<'_>, at: u64, class: u8) -> bool {
    smf.tracks.iter().any(|track| {
        let mut tick = 0;
        track.iter().any(|event| {
            tick += u64::from(event.delta.as_int());
            tick == at
                && matches!(
                    (class, event.kind),
                    (0, TrackEventKind::Meta(MetaMessage::Tempo(_)))
                        | (1, TrackEventKind::Meta(MetaMessage::TimeSignature(..)))
                        | (2, TrackEventKind::Meta(MetaMessage::KeySignature(..)))
                )
        })
    })
}

pub fn check_source_revision(
    source: &MidiSource,
    timebase: Timebase,
    composition: &Composition,
) -> Result<(), Vec<Diagnostic>> {
    let imported = source_projection(source)?;
    let plan = MidiExport {
        id: "source-check".into(),
        revision: "source-check".into(),
        timing: None,
        assignments: imported.assignments,
        sections_as_markers: false,
    };
    write_preserving(source, timebase, composition, &plan).map(|_| ())
}
