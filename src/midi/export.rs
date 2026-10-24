//! Writing a checked revision as a type 1 Standard MIDI File.

use super::*;

/// Writes a type 1 file. The composition and the export plan are checked
/// first; nothing is written when they have diagnostics.
pub fn write_smf(
    timebase: Timebase,
    composition: &Composition,
    plan: &MidiExport,
) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    check_composition(
        timebase,
        composition,
        &mut Report::new("/composition", &mut diagnostics),
    );
    if diagnostics.is_empty() {
        check_midi_export(
            timebase,
            composition,
            plan,
            &mut Report::new("/export", &mut diagnostics),
        );
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let (ppq, file_tick): (u16, Box<dyn Fn(u64) -> u64>) = match (timebase, plan.timing) {
        (Timebase::Music { ppq }, _) => (ppq, Box::new(|tick| tick)),
        (Timebase::Effect, Some(timing)) => (
            timing.ppq,
            Box::new(move |microseconds| {
                timing.ticks_for_microseconds(microseconds).to_integer() as u64
            }),
        ),
        (Timebase::Effect, None) => unreachable!("checked by check_midi_export"),
    };
    let end = file_tick(composition.playback.end);

    let mut conductor: Vec<(u64, TrackEventKind)> = vec![(
        0,
        TrackEventKind::Meta(MetaMessage::TrackName(b"Conductor")),
    )];
    match (timebase, plan.timing) {
        (Timebase::Effect, Some(timing)) => conductor.push((
            0,
            TrackEventKind::Meta(MetaMessage::Tempo(u24::new(
                timing.microseconds_per_quarter,
            ))),
        )),
        _ => conductor.extend(composition.tempo_map.iter().map(|t| {
            (
                t.tick,
                TrackEventKind::Meta(MetaMessage::Tempo(u24::new(t.microseconds_per_quarter))),
            )
        })),
    }
    for signature in &composition.time_signatures {
        let power = signature.denominator.trailing_zeros() as u8;
        conductor.push((
            signature.tick,
            TrackEventKind::Meta(MetaMessage::TimeSignature(
                signature.numerator,
                power,
                24,
                8,
            )),
        ));
    }
    for key in &composition.key_signatures {
        conductor.push((
            key.tick,
            TrackEventKind::Meta(MetaMessage::KeySignature(key.sharps, key.minor)),
        ));
    }
    let writes_loop = composition.playback.loop_start.is_some();
    for marker in composition
        .markers
        .iter()
        .filter(|m| !(writes_loop && (m.text == LOOP_START_MARKER || m.text == LOOP_END_MARKER)))
    {
        let text = marker.text.as_bytes();
        let meta = match marker.kind {
            MarkerKind::Marker => MetaMessage::Marker(text),
            MarkerKind::Text => MetaMessage::Text(text),
            MarkerKind::CuePoint => MetaMessage::CuePoint(text),
            MarkerKind::Lyric => MetaMessage::Lyric(text),
            MarkerKind::Copyright => MetaMessage::Copyright(text),
        };
        conductor.push((file_tick(marker.at), TrackEventKind::Meta(meta)));
    }
    if let Some(loop_start) = composition.playback.loop_start {
        conductor.push((
            file_tick(loop_start),
            TrackEventKind::Meta(MetaMessage::Marker(LOOP_START_MARKER.as_bytes())),
        ));
        conductor.push((
            end,
            TrackEventKind::Meta(MetaMessage::Marker(LOOP_END_MARKER.as_bytes())),
        ));
    }
    conductor.sort_by_key(|(tick, _)| *tick);

    let mut track_names: Vec<&str> = Vec::new();
    for assignment in &plan.assignments {
        if !track_names.contains(&assignment.track.as_str()) {
            track_names.push(&assignment.track);
        }
    }
    let channel_track: HashMap<u8, usize> = plan
        .assignments
        .iter()
        .map(|a| {
            (
                a.channel,
                track_names
                    .iter()
                    .position(|t| *t == a.track)
                    .expect("listed above"),
            )
        })
        .collect();
    let mut voice_tracks: Vec<Vec<(u64, TrackEventKind)>> = track_names
        .iter()
        .map(|name| {
            vec![(
                0,
                TrackEventKind::Meta(MetaMessage::TrackName(name.as_bytes())),
            )]
        })
        .collect();
    for event in channel_events(composition, &plan.assignments) {
        let message = match event.message {
            ChannelMessage::NoteOff { key, velocity } => MidiMessage::NoteOff {
                key: u7::new(key),
                vel: u7::new(velocity),
            },
            ChannelMessage::NoteOn { key, velocity } => MidiMessage::NoteOn {
                key: u7::new(key),
                vel: u7::new(velocity),
            },
            ChannelMessage::Controller { controller, value } => MidiMessage::Controller {
                controller: u7::new(controller),
                value: u7::new(value),
            },
            ChannelMessage::Program(program) => MidiMessage::ProgramChange {
                program: u7::new(program),
            },
            ChannelMessage::PitchBend(bend) => MidiMessage::PitchBend {
                bend: PitchBend::from_int(bend),
            },
        };
        voice_tracks[channel_track[&event.channel]].push((
            file_tick(event.at),
            TrackEventKind::Midi {
                channel: u4::new(event.channel),
                message,
            },
        ));
    }

    let mut tracks = Vec::with_capacity(voice_tracks.len() + 1);
    for (index, events) in std::iter::once(conductor).chain(voice_tracks).enumerate() {
        tracks.push(encode_track(events, end).map_err(|message| {
            vec![Diagnostic {
                code: Code::ValueOutOfRange,
                location: format!("/tracks/{index}"),
                message,
            }]
        })?);
    }
    let smf = Smf {
        header: Header::new(Format::Parallel, Timing::Metrical(u15::new(ppq))),
        tracks,
    };
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).map_err(|error| {
        vec![Diagnostic {
            code: Code::ValueOutOfRange,
            location: String::new(),
            message: error.to_string(),
        }]
    })?;
    Ok(bytes)
}

fn encode_track(events: Vec<(u64, TrackEventKind)>, end: u64) -> Result<Vec<TrackEvent>, String> {
    let mut track = Vec::with_capacity(events.len() + 1);
    let mut previous = 0;
    for (tick, kind) in events.into_iter().chain(std::iter::once((
        end,
        TrackEventKind::Meta(MetaMessage::EndOfTrack),
    ))) {
        let delta = u32::try_from(tick - previous)
            .ok()
            .and_then(u28::try_from)
            .ok_or_else(|| {
                format!(
                    "a gap of {} ticks does not fit a MIDI delta time",
                    tick - previous
                )
            })?;
        track.push(TrackEvent { delta, kind });
        previous = tick;
    }
    Ok(track)
}
