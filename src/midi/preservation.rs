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
    project_source_bytes(source, &source.bytes)
}

fn project_source_bytes(
    source: &MidiSource,
    bytes: &[u8],
) -> Result<ImportedMidi, Vec<Diagnostic>> {
    if source.legacy_declared_ends == Some(true) {
        super::import::read_smf_legacy_declaring(bytes, source.declared_ends.clone())
    } else {
        read_smf_declaring(bytes, source.declared_ends.clone())
    }
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
    write_preserving_declared(source, timebase, composition, plan, false)
}

/// [`write_preserving`] that, with `write_declared`, also writes the release
/// of every declared-end note at its projected end, so playback and other
/// players stop it there. Without it only changed declared notes are written.
pub fn write_preserving_declared(
    source: &MidiSource,
    timebase: Timebase,
    composition: &Composition,
    plan: &MidiExport,
    write_declared: bool,
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
    let declared = write_declared && imported.note_bindings.iter().any(|b| b.declared.is_some());
    if &comparable == old && !declared {
        return Ok(source.bytes.clone());
    }
    let mut diagnostics = Vec::new();
    check_composition_with(
        timebase,
        composition,
        StackedNotes::SourcePaired,
        &mut Report::new("/composition", &mut diagnostics),
    );
    check_stacked_notes(
        old,
        composition,
        &mut Report::new("/composition", &mut diagnostics),
    );
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let smf = Smf::parse(&source.bytes).map_err(|e| refuse("/midi_source", &e.to_string()))?;
    let multiple = multiple_receivers(&smf);
    // Notes bind to their source events by identity, never by time or pitch,
    // so stacked onsets of one key keep their own events.
    let old_notes: HashMap<&str, &Note> = old.notes.iter().map(|n| (n.id.as_str(), n)).collect();
    let new_notes: HashMap<_, _> = composition
        .notes
        .iter()
        .map(|n| (n.id.as_str(), n))
        .collect();
    let mut roles: HashMap<SourceEvent, (bool, &str)> = HashMap::new();
    // Declared-end notes that changed and have no source release to move.
    let mut written_releases = Vec::new();
    for binding in &imported.note_bindings {
        let id = binding.id.as_str();
        // An unchanged declared end is projection only: its events stay as
        // they are, including a zero-length release.
        if binding.declared.is_some()
            && !write_declared
            && new_notes.get(id) == Some(&old_notes[id])
        {
            continue;
        }
        roles.insert(binding.onset, (true, id));
        match binding.release {
            Some(release) => {
                roles.insert(release, (false, id));
            }
            None => written_releases.push((binding.onset.0, id)),
        }
    }
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
        let mut port = 0;
        let mut last_control = BTreeMap::new();
        let mut last_tempo = BTreeMap::new();
        for (index, event) in track.iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            if let TrackEventKind::Meta(MetaMessage::MidiPort(value)) = event.kind {
                port = value.as_int();
            }
            if matches!(event.kind, TrackEventKind::Meta(MetaMessage::Tempo(_))) {
                last_tempo.insert(tick, index);
            }
            if let TrackEventKind::Midi { channel, message } = event.kind
                && let Some((kind, _)) = direct_control(message)
            {
                last_control.insert(
                    (
                        receiver_channel(multiple, port, channel.as_int()),
                        kind,
                        tick,
                    ),
                    index,
                );
            }
        }
        let mut events = Vec::new();
        tick = 0;
        port = 0;
        for (index, event) in track.iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            let mut at = tick;
            let mut order = index as i64;
            let mut kind = event.kind;
            if let TrackEventKind::Meta(MetaMessage::MidiPort(value)) = kind {
                port = value.as_int();
            }
            match kind {
                TrackEventKind::Meta(MetaMessage::Tempo(_))
                    if tempo_changed && tick < old.playback.end =>
                {
                    let Some(point) = composition.tempo_map.iter().find(|p| p.tick == tick) else {
                        continue;
                    };
                    if last_tempo[&tick] == index {
                        kind = TrackEventKind::Meta(MetaMessage::Tempo(u24::new(
                            point.microseconds_per_quarter,
                        )));
                    }
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
                let receiver = receiver_channel(multiple, port, channel.as_int());
                let voice = source_voice(track_index, receiver);
                let role = roles.get(&(track_index, index)).copied();
                match message {
                    // Retained note events without an editable note stay as they are.
                    MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. } if role.is_none() => {}
                    MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. } => {
                        let (onset, id) = role.expect("bound note event");
                        let original = old_notes[id];
                        let Some(note) = new_notes.get(id) else {
                            continue;
                        };
                        if note.voice != original.voice {
                            return Err(refuse(
                                "/notes",
                                "moving an imported note to another voice is not yet implemented losslessly",
                            ));
                        }
                        if onset {
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
                        } else {
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
                    }
                    _ => {
                        if tick < old.playback.end
                            && let Some((control, _)) = direct_control(message)
                            && let Some(next) = changed_lanes.get(&(voice, control))
                        {
                            let point =
                                next.and_then(|lane| lane.points.iter().find(|p| p.at == tick));
                            let Some(point) = point else {
                                continue;
                            };
                            if last_control[&(receiver, control, tick)] == index {
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
            if note.voice
                != source_voice(
                    track_index,
                    receiver_channel(multiple, assignment.port.unwrap_or(0), assignment.channel),
                )
            {
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
        for (_, id) in written_releases.iter().filter(|(t, _)| *t == track_index) {
            let Some(note) = new_notes.get(id) else {
                continue;
            };
            let Some(assignment) = imported.assignments.iter().find(|a| a.voice == note.voice)
            else {
                continue;
            };
            events.push((
                note.start + note.duration,
                i64::MIN,
                TrackEventKind::Midi {
                    channel: u4::new(assignment.channel),
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
            if *voice
                != source_voice(
                    track_index,
                    receiver_channel(multiple, assignment.port.unwrap_or(0), assignment.channel),
                )
            {
                continue;
            }
            if let Some(lane) = lane {
                for point in &lane.points {
                    if !last_control.contains_key(&(
                        receiver_channel(
                            multiple,
                            assignment.port.unwrap_or(0),
                            assignment.channel,
                        ),
                        *control,
                        point.at,
                    )) {
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
    let readback = project_source_bytes(source, &bytes)?;
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
            if source.declared_ends.is_some() {
                "the edited MIDI does not read back to the requested composition; no output was accepted. A written or moved declared-end release must not change first-in-first-out pairing, for example by ending after a later note of the same key is released"
            } else {
                "the edited MIDI does not read back to the requested composition; no output was accepted"
            },
        ));
    }
    // Retained note events keep their tracks, ticks and messages. Readback must
    // not pair them with edited notes or leave edited events unpaired.
    let retained = |events: &[RetainedNoteEvent]| {
        let mut events: Vec<_> = events
            .iter()
            .map(|e| (e.event.0, e.tick, e.channel, e.message))
            .collect();
        events.sort_by_key(|(track, tick, channel, message)| {
            (*track, *tick, *channel, format!("{message:?}"))
        });
        events
    };
    if retained(&readback.retained_note_events) != retained(&imported.retained_note_events) {
        return Err(refuse(
            "/composition/notes",
            "the edit changes which source note events pair into notes; retained note events must stay unpaired and edited notes paired",
        ));
    }
    Ok(bytes)
}

/// Same-pitch notes of one voice may overlap only where the source already
/// stacked them; a new overlap would pair ambiguously.
fn check_stacked_notes(old: &Composition, composition: &Composition, report: &mut Report) {
    let unordered = |(a, b): &(String, String)| {
        if a < b {
            (a.clone(), b.clone())
        } else {
            (b.clone(), a.clone())
        }
    };
    let source_pairs: std::collections::BTreeSet<_> =
        overlapping_pairs(old).iter().map(unordered).collect();
    for pair @ (first, second) in &overlapping_pairs(composition) {
        if !source_pairs.contains(&unordered(pair)) {
            let index = composition.notes.iter().position(|n| &n.id == second);
            report.push(
                Code::OverlappingNotes,
                format!("notes/{}", index.unwrap_or_default()),
                format!("note `{second}` would start while `{first}` of the same pitch still sounds in the same voice; only stacks present in the source MIDI are kept"),
            );
        }
    }
}

/// Note ID pairs `(earlier, later)` of one voice and pitch that overlap.
fn overlapping_pairs(composition: &Composition) -> std::collections::BTreeSet<(String, String)> {
    let mut groups: BTreeMap<(&str, u8), Vec<&Note>> = BTreeMap::new();
    for note in &composition.notes {
        groups
            .entry((note.voice.as_str(), note.pitch))
            .or_default()
            .push(note);
    }
    let mut pairs = std::collections::BTreeSet::new();
    for notes in groups.values_mut() {
        notes.sort_by_key(|n| (n.start, n.id.as_str()));
        for (i, earlier) in notes.iter().enumerate() {
            let end = earlier.end().unwrap_or(u64::MAX);
            for later in notes[i + 1..].iter().take_while(|n| n.start < end) {
                pairs.insert((earlier.id.clone(), later.id.clone()));
            }
        }
    }
    pairs
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

/// Channel state is shared by every voice that sends on a channel: a control
/// event affects all of their notes, and a release can end another voice's
/// sounding note of that key. Changes there must be in scope for, and are
/// protected by, every voice they reach, not only the voice holding the event.
pub fn check_source_bounds(
    source: &MidiSource,
    base: &Composition,
    result: &Composition,
    request: &crate::edit::EditRequest,
) -> Result<(), Vec<Diagnostic>> {
    let imported = source_projection(source)?;
    let mut channel_voices: BTreeMap<(Option<u8>, u8), Vec<&str>> = BTreeMap::new();
    for assignment in &imported.assignments {
        channel_voices
            .entry((assignment.port, assignment.channel))
            .or_default()
            .push(assignment.voice.as_str());
    }
    let sharing = |voice: &str| -> Vec<&str> {
        imported
            .assignments
            .iter()
            .find(|a| a.voice == voice)
            .and_then(|a| channel_voices.get(&(a.port, a.channel)))
            .map(|voices| voices.iter().copied().filter(|v| *v != voice).collect())
            .unwrap_or_default()
    };
    let mut diagnostics = Vec::new();
    let mut report = Report::new("", &mut diagnostics);
    let lane = |c: &Composition, voice: &str, kind: ControlKind| -> Vec<ControlPoint> {
        c.control_lanes
            .iter()
            .find(|l| l.voice == voice && l.kind == kind)
            .map(|l| l.points.clone())
            .unwrap_or_default()
    };
    let mut lanes: Vec<(&str, ControlKind)> = base
        .control_lanes
        .iter()
        .chain(&result.control_lanes)
        .map(|l| (l.voice.as_str(), l.kind))
        .collect();
    lanes.sort();
    lanes.dedup();
    for (voice, kind) in lanes {
        let others = sharing(voice);
        if others.is_empty() {
            continue;
        }
        let (before, after) = (lane(base, voice, kind), lane(result, voice, kind));
        // A step value holds until the next point of the lane.
        let end = base.playback.end.max(result.playback.end);
        let spans: Vec<(u64, u64)> = before
            .iter()
            .filter(|p| !after.contains(p))
            .chain(after.iter().filter(|p| !before.contains(p)))
            .map(|point| {
                let next = before
                    .iter()
                    .chain(&after)
                    .map(|p| p.at)
                    .filter(|t| *t > point.at)
                    .min()
                    .unwrap_or(end);
                (point.at, next.max(point.at + 1))
            })
            .collect();
        if spans.is_empty() {
            continue;
        }
        let versions: Vec<(&str, Option<(u64, u64)>)> = others
            .iter()
            .flat_map(|other| spans.iter().map(move |span| (*other, Some(*span))))
            .collect();
        crate::edit::check_change(
            request,
            &versions,
            &mut report,
            format!(
                "{kind:?} control of `{voice}`, which reaches every voice on its MIDI channel,"
            ),
        );
    }
    let notes = |c: &Composition| -> HashMap<String, Note> {
        c.notes.iter().map(|n| (n.id.clone(), n.clone())).collect()
    };
    let (before, after) = (notes(base), notes(result));
    let mut ids: Vec<&String> = before.keys().chain(after.keys()).collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        let (old, new) = (before.get(id), after.get(id));
        if old == new {
            continue;
        }
        let mut versions = Vec::new();
        for changed in old.into_iter().chain(new) {
            let span = (changed.start, changed.end().unwrap_or(u64::MAX));
            for other in sharing(&changed.voice) {
                let reached = base.notes.iter().chain(&result.notes).any(|n| {
                    n.voice == other
                        && n.pitch == changed.pitch
                        && n.start < span.1
                        && span.0 < n.end().unwrap_or(u64::MAX)
                });
                if reached {
                    versions.push((other, Some(span)));
                }
            }
        }
        if !versions.is_empty() {
            crate::edit::check_change(
                request,
                &versions,
                &mut report,
                format!(
                    "note `{id}`, which shares its MIDI channel and key with another voice's sounding note,"
                ),
            );
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
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

/// Notes whose end comes from the source's declared end rule.
pub fn declared_notes(imported: &ImportedMidi) -> Vec<Value> {
    imported
        .note_bindings
        .iter()
        .filter_map(|binding| {
            let from = binding.declared?;
            let note = imported
                .composition
                .notes
                .iter()
                .find(|n| n.id == binding.id)?;
            Some(
                json!({"note": note.id, "from": from, "voice": note.voice, "start": note.start,
                "declared_end": note.start + note.duration,
                "onset": format!("/tracks/{}/events/{}", binding.onset.0, binding.onset.1)}),
            )
        })
        .collect()
}

/// What an edit writes for declared-end notes it changes: a new release, a
/// moved zero-length release, or removal of the source events.
pub fn declared_end_changes(
    source: &MidiSource,
    composition: &Composition,
    write_declared: bool,
) -> Result<Vec<Value>, Vec<Diagnostic>> {
    let imported = source_projection(source)?;
    let mut changes = Vec::new();
    for binding in imported
        .note_bindings
        .iter()
        .filter(|b| b.declared.is_some())
    {
        let old = imported
            .composition
            .notes
            .iter()
            .find(|n| n.id == binding.id);
        let new = composition.notes.iter().find(|n| n.id == binding.id);
        if old == new && !write_declared {
            continue;
        }
        let onset = format!("/tracks/{}/events/{}", binding.onset.0, binding.onset.1);
        changes.push(match (new, binding.release) {
            (None, _) => json!({"note": binding.id, "onset": onset, "change": "removed its source onset and any release"}),
            (Some(note), None) => json!({"note": note.id, "onset": onset, "release_written_at": note.start + note.duration,
                "change": "wrote a release the source did not have"}),
            (Some(note), Some(release)) => json!({"note": note.id, "onset": onset, "release_written_at": note.start + note.duration,
                "change": format!("moved the zero-length release /tracks/{}/events/{}", release.0, release.1)}),
        });
    }
    Ok(changes)
}

/// A retained never-released note: it has no note id and sounds from its
/// onset until the transport releases held notes at the content end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldRetainedNote {
    pub source_event: String,
    pub voice: String,
    pub pitch: u8,
    pub start: u64,
}

pub fn held_retained_notes(source: &MidiSource) -> Result<Vec<HeldRetainedNote>, Vec<Diagnostic>> {
    let imported = source_projection(source)?;
    Ok(imported
        .retained_note_events
        .iter()
        .filter(|e| e.kind == RetainedKind::NeverReleased)
        .filter_map(|e| match e.message {
            MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => Some(HeldRetainedNote {
                source_event: format!("/tracks/{}/events/{}", e.event.0, e.event.1),
                voice: source_voice(e.event.0, e.channel),
                pitch: key.as_int(),
                start: e.tick,
            }),
            _ => None,
        })
        .collect())
}
