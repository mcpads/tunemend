mod common;

use midly::num::{u4, u7, u15, u24, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use tunemend::DiagnosticCode as Code;
use tunemend::composition::{ControlKind, MarkerKind};
use tunemend::export::MidiTiming;
use tunemend::midi::{LOOP_END_MARKER, LOOP_START_MARKER, read_smf, write_smf};
use tunemend::performance::{ChannelEvent, channel_events};

#[test]
fn music_round_trips_with_the_same_channel_messages() {
    let bundle = common::village();
    let (composition, plan) = (&bundle.revisions[1].composition, &bundle.midi_exports[0]);
    let bytes = write_smf(bundle.project.timebase, composition, plan).unwrap();
    let imported = read_smf(&bytes).unwrap();

    assert_eq!(imported.timebase, bundle.project.timebase);
    assert_eq!(
        channel_events(&imported.composition, &imported.assignments),
        channel_events(composition, &plan.assignments)
    );
    assert_eq!(imported.composition.tempo_map, composition.tempo_map);
    assert_eq!(
        imported.composition.time_signatures,
        composition.time_signatures
    );
    assert_eq!(imported.composition.playback.end, composition.playback.end);
    let tracks: Vec<(&str, u8)> = imported
        .assignments
        .iter()
        .map(|a| (a.track.as_str(), a.channel))
        .collect();
    assert_eq!(tracks, [("Melody", 0), ("Accompaniment", 1), ("Bass", 2)]);

    // The loop is written as markers and is not inferred back.
    assert_eq!(imported.composition.playback.loop_start, None);
    let markers: Vec<(u64, &str)> = imported
        .composition
        .markers
        .iter()
        .map(|m| (m.at, m.text.as_str()))
        .collect();
    assert_eq!(
        markers,
        [(3840, LOOP_START_MARKER), (15360, LOOP_END_MARKER)]
    );
    assert!(
        imported
            .normalizations
            .iter()
            .any(|n| n.message.contains("declare the loop explicitly"))
    );
}

#[test]
fn effects_are_written_on_their_explicit_grid() {
    let bundle = common::chime();
    let (composition, plan) = (&bundle.revisions[0].composition, &bundle.midi_exports[0]);
    let imported =
        read_smf(&write_smf(bundle.project.timebase, composition, plan).unwrap()).unwrap();

    let in_milliseconds: Vec<ChannelEvent> = channel_events(composition, &plan.assignments)
        .into_iter()
        .map(|e| ChannelEvent {
            at: e.at / 1000,
            ..e
        })
        .collect();
    assert_eq!(
        channel_events(&imported.composition, &imported.assignments),
        in_milliseconds
    );
    assert_eq!(imported.composition.playback.end, 500);
    assert_eq!(
        imported.composition.tempo_map[0].microseconds_per_quarter,
        1_000_000
    );

    let mut coarse = plan.clone();
    coarse.timing = Some(MidiTiming {
        ppq: 60,
        microseconds_per_quarter: 1_000_000,
    });
    let blocked = write_smf(bundle.project.timebase, composition, &coarse).unwrap_err();
    assert!(
        blocked
            .iter()
            .all(|d| d.code == Code::EffectTimeNotRepresentable),
        "{blocked:#?}"
    );
}

type Events = Vec<(u32, TrackEventKind<'static>)>;

fn file(format: Format, timing: Timing, tracks: Vec<Events>) -> Vec<u8> {
    let tracks = tracks
        .into_iter()
        .map(|events| {
            events
                .into_iter()
                .map(|(delta, kind)| TrackEvent {
                    delta: u28::new(delta),
                    kind,
                })
                .chain(std::iter::once(TrackEvent {
                    delta: u28::new(0),
                    kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
                }))
                .collect()
        })
        .collect();
    let mut bytes = Vec::new();
    Smf {
        header: Header::new(format, timing),
        tracks,
    }
    .write_std(&mut bytes)
    .unwrap();
    bytes
}

fn metrical(tracks: Vec<Events>) -> Vec<u8> {
    file(Format::Parallel, Timing::Metrical(u15::new(480)), tracks)
}

fn midi(channel: u8, message: MidiMessage) -> TrackEventKind<'static> {
    TrackEventKind::Midi {
        channel: u4::new(channel),
        message,
    }
}

fn on(key: u8) -> MidiMessage {
    MidiMessage::NoteOn {
        key: u7::new(key),
        vel: u7::new(100),
    }
}

fn off(key: u8) -> MidiMessage {
    MidiMessage::NoteOff {
        key: u7::new(key),
        vel: u7::new(64),
    }
}

fn cc(controller: u8, value: u8) -> MidiMessage {
    MidiMessage::Controller {
        controller: u7::new(controller),
        value: u7::new(value),
    }
}

fn note(key: u8) -> Events {
    vec![(0, midi(0, on(key))), (480, midi(0, off(key)))]
}

#[test]
fn unsupported_or_ambiguous_files_are_blocked_where_the_problem_is() {
    let tempo = |t: u32| TrackEventKind::Meta(MetaMessage::Tempo(u24::new(t)));
    let cases: Vec<(&str, Vec<u8>, Code, &str)> = vec![
        (
            "type 2",
            file(
                Format::Sequential,
                Timing::Metrical(u15::new(480)),
                vec![note(60)],
            ),
            Code::UnsupportedMidi,
            "/header",
        ),
        (
            "SMPTE timing",
            file(
                Format::Parallel,
                Timing::Timecode(midly::Fps::Fps25, 40),
                vec![note(60)],
            ),
            Code::UnsupportedMidi,
            "/header",
        ),
        (
            "same key restarts while sounding",
            metrical(vec![vec![
                (0, midi(0, on(60))),
                (10, midi(0, on(60))),
                (10, midi(0, off(60))),
            ]]),
            Code::OverlappingNotes,
            "/tracks/0/events/1",
        ),
        (
            "release without a note",
            metrical(vec![vec![(0, midi(0, off(60)))]]),
            Code::UnpairedNoteEvent,
            "/tracks/0/events/0",
        ),
        (
            "note never released",
            metrical(vec![vec![(0, midi(0, on(60)))]]),
            Code::UnpairedNoteEvent,
            "/tracks/0/events/0",
        ),
        (
            "sustain pedal",
            metrical(vec![[vec![(0, midi(0, cc(64, 127)))], note(60)].concat()]),
            Code::UnsupportedMidiController,
            "/tracks/0/events/0",
        ),
        (
            "system exclusive",
            metrical(vec![
                [
                    vec![(0, TrackEventKind::SysEx(&[0x7E, 0x7F, 0xF7]))],
                    note(60),
                ]
                .concat(),
            ]),
            Code::UnsupportedMidi,
            "/tracks/0/events/0",
        ),
        (
            "controller after a note-on at the same tick",
            metrical(vec![vec![
                (0, midi(0, on(60))),
                (0, midi(0, cc(7, 90))),
                (480, midi(0, off(60))),
            ]]),
            Code::UnsupportedMidi,
            "/tracks/0/events/1",
        ),
        (
            "two tempos at one tick",
            metrical(vec![
                [vec![(0, tempo(500_000)), (0, tempo(400_000))], note(60)].concat(),
            ]),
            Code::UnsupportedMidi,
            "/tracks/0/events/1",
        ),
        (
            "bend range with cents",
            metrical(vec![
                [
                    vec![
                        (0, midi(0, cc(101, 0))),
                        (0, midi(0, cc(100, 0))),
                        (0, midi(0, cc(6, 2))),
                        (0, midi(0, cc(38, 50))),
                    ],
                    note(60),
                ]
                .concat(),
            ]),
            Code::UnsupportedMidi,
            "/tracks/0/events/3",
        ),
        (
            "zero-length note",
            metrical(vec![vec![(0, midi(0, on(60))), (0, midi(0, off(60)))]]),
            Code::NonPositiveDuration,
            "/tracks/0/events/1",
        ),
        (
            "channel controlled from two tracks",
            metrical(vec![
                [vec![(0, midi(0, cc(7, 90)))], note(60)].concat(),
                note(64),
            ]),
            Code::SharedChannelControl,
            "/tracks/0/events/0",
        ),
        (
            "aftertouch",
            metrical(vec![
                [
                    note(60),
                    vec![(
                        0,
                        midi(0, MidiMessage::ChannelAftertouch { vel: u7::new(10) }),
                    )],
                ]
                .concat(),
            ]),
            Code::UnsupportedMidi,
            "/tracks/0/events/2",
        ),
    ];
    for (name, bytes, code, location) in cases {
        let diagnostics = read_smf(&bytes).expect_err(name);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == code && d.location == location),
            "{name}: expected {code:?} at {location}, got {diagnostics:#?}",
        );
    }
}

#[test]
fn assumptions_are_reported_instead_of_hidden() {
    let bend = MidiMessage::PitchBend {
        bend: midly::PitchBend::from_int(1000),
    };
    let single = file(
        Format::SingleTrack,
        Timing::Metrical(u15::new(96)),
        vec![
            [
                vec![
                    (0, TrackEventKind::Meta(MetaMessage::Marker(b"verse"))),
                    (0, midi(0, bend)),
                ],
                note(60),
            ]
            .concat(),
        ],
    );
    let imported = read_smf(&single).unwrap();
    let messages: Vec<&str> = imported
        .normalizations
        .iter()
        .map(|n| n.message.as_str())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("type 0")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("assumed the MIDI default")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("assumed ±2 semitones")),
        "{messages:?}"
    );
    let range = imported
        .composition
        .control_lanes
        .iter()
        .find(|l| l.kind == ControlKind::PitchBendRange)
        .unwrap();
    assert_eq!(range.points[0].value, 2);
    assert_eq!(imported.composition.markers[0].kind, MarkerKind::Marker);
    assert_eq!(
        imported.composition.tempo_map[0].microseconds_per_quarter,
        500_000
    );
}
