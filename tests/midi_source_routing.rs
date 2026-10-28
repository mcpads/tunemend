//! Source-only playback routes ports to independent receivers and plays one
//! type 2 sequence on its own clock.
use midly::num::{u4, u7, u14, u15, u24, u28};
use midly::{
    Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, Timing, TrackEvent, TrackEventKind,
};
use serde_json::json;
use tunemend::{
    render::{RenderedAudio, render_midi_sequence, render_midi_source},
    review::RenderLayout,
    service::{RenderPreviewInput, Service, Status},
    soundfont::reference_soundfont,
};

type Kind = TrackEventKind<'static>;

fn event(delta: u32, kind: Kind) -> TrackEvent<'static> {
    TrackEvent {
        delta: u28::new(delta),
        kind,
    }
}
fn midi(channel: u8, message: MidiMessage) -> Kind {
    TrackEventKind::Midi {
        channel: u4::new(channel),
        message,
    }
}
fn on(channel: u8, key: u8, velocity: u8) -> Kind {
    midi(
        channel,
        MidiMessage::NoteOn {
            key: u7::new(key),
            vel: u7::new(velocity),
        },
    )
}
fn off(channel: u8, key: u8) -> Kind {
    midi(
        channel,
        MidiMessage::NoteOff {
            key: u7::new(key),
            vel: u7::new(64),
        },
    )
}
fn cc(channel: u8, controller: u8, value: u8) -> Kind {
    midi(
        channel,
        MidiMessage::Controller {
            controller: u7::new(controller),
            value: u7::new(value),
        },
    )
}
fn program(channel: u8, value: u8) -> Kind {
    midi(
        channel,
        MidiMessage::ProgramChange {
            program: u7::new(value),
        },
    )
}
fn port(value: u8) -> Kind {
    TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(value)))
}
fn tempo(microseconds: u32) -> Kind {
    TrackEventKind::Meta(MetaMessage::Tempo(u24::new(microseconds)))
}
fn end() -> Kind {
    TrackEventKind::Meta(MetaMessage::EndOfTrack)
}
fn smf(format: Format, timing: Timing, tracks: Vec<Vec<TrackEvent<'static>>>) -> Vec<u8> {
    let mut bytes = Vec::new();
    Smf {
        header: Header::new(format, timing),
        tracks,
    }
    .write_std(&mut bytes)
    .unwrap();
    bytes
}
fn ppq(value: u16) -> Timing {
    Timing::Metrical(u15::new(value))
}
const FULL: RenderLayout = RenderLayout::Full { loop_passes: 1 };

/// Port 0 mutes channel 1, changes its program and releases key 60 while
/// port 1 holds key 60 on the same channel with a pitch bend.
fn port_zero(with_port: bool) -> Vec<TrackEvent<'static>> {
    let mut track = vec![
        event(0, program(0, 40)),
        event(0, cc(0, 7, 0)),
        event(0, on(0, 60, 90)),
        event(240, off(0, 60)),
        event(0, cc(0, 64, 127)),
        event(480, end()),
    ];
    if with_port {
        track.insert(0, event(0, port(0)));
    }
    track
}
fn port_one(with_port: bool) -> Vec<TrackEvent<'static>> {
    let mut track = vec![
        event(120, on(0, 60, 100)),
        event(
            60,
            midi(
                0,
                MidiMessage::PitchBend {
                    bend: PitchBend(u14::new(12_000)),
                },
            ),
        ),
        event(300, off(0, 60)),
        event(240, end()),
    ];
    if with_port {
        track.insert(0, event(0, port(1)));
    }
    track
}

fn common_prefix(a: &RenderedAudio, parts: &[&RenderedAudio]) -> usize {
    parts
        .iter()
        .map(|p| p.left.len())
        .chain([a.left.len()])
        .min()
        .unwrap()
}

#[test]
fn the_same_channel_on_different_ports_plays_on_independent_receivers() {
    let font = reference_soundfont();
    let routed = smf(
        Format::Parallel,
        ppq(480),
        vec![port_zero(true), port_one(true)],
    );
    let alone_zero = smf(Format::Parallel, ppq(480), vec![port_zero(false)]);
    let alone_one = smf(Format::Parallel, ppq(480), vec![port_one(false)]);
    let merged = smf(
        Format::Parallel,
        ppq(480),
        vec![port_zero(false), port_one(false)],
    );
    let both = render_midi_sequence(&routed, None, &FULL, &font, 16000).unwrap();
    let project = tunemend::render::render_midi(&routed, &FULL, None, &font, 16000).unwrap();
    assert_eq!(project.left, both.audio.left);
    assert_eq!(project.right, both.audio.right);
    let zero = render_midi_source(&alone_zero, &FULL, &font, 16000).unwrap();
    let one = render_midi_source(&alone_one, &FULL, &font, 16000).unwrap();
    assert_eq!(both.audio.content_samples, zero.content_samples);
    assert_eq!(both.audio.content_samples, one.content_samples);
    // Each receiver is exactly what it renders alone; the sum is in port order.
    let n = common_prefix(&both.audio, &[&zero, &one]);
    for i in 0..n {
        assert_eq!(both.audio.left[i], zero.left[i] + one.left[i], "left {i}");
        assert_eq!(
            both.audio.right[i],
            zero.right[i] + one.right[i],
            "right {i}"
        );
    }
    // Port 0's mute, program, NoteOff and sustain never reach port 1.
    assert!(one.left.iter().any(|s| s.abs() > 0.01));
    // CC7 0 attenuates by 96 dB in the SoundFont model rather than silencing.
    assert!(zero.left.iter().all(|s| s.abs() < 1e-3));
    let merged = render_midi_source(&merged, &FULL, &font, 16000).unwrap();
    assert_ne!(merged.left[..n], both.audio.left[..n]);
    let ports: Vec<_> = both
        .receivers
        .iter()
        .map(|r| (r.port, r.tracks.clone()))
        .collect();
    assert_eq!(ports, vec![(0, vec![0]), (1, vec![1])]);
    assert_eq!(both.receivers[0].channel_events, 5);
    assert_eq!(both.receivers[1].channel_events, 3);

    // A type 0 track can change ports mid-stream; earlier events keep theirs.
    let switching = smf(
        Format::SingleTrack,
        ppq(480),
        vec![vec![
            event(0, on(0, 60, 100)),
            event(240, port(1)),
            event(0, on(0, 60, 100)),
            event(0, port(0)),
            event(240, off(0, 60)),
            event(0, port(1)),
            event(240, off(0, 60)),
            event(0, end()),
        ]],
    );
    let switched = render_midi_sequence(&switching, None, &FULL, &font, 16000).unwrap();
    let project = tunemend::render::render_midi(&switching, &FULL, None, &font, 16000).unwrap();
    assert_eq!(project.left, switched.audio.left);
    assert_eq!(project.right, switched.audio.right);
    assert_eq!(switched.receivers.len(), 2);
    assert_eq!(switched.receivers[0].channel_events, 2);

    // Analysis and the service agree with the renderer.
    let analysis = tunemend::midi::analyze_smf(&routed);
    assert_eq!(analysis["preview"]["available"], true);
    assert_eq!(analysis["preview"]["project_available"], true);
    assert_eq!(
        analysis["preview"]["receivers"],
        json!([{"port":0,"tracks":[0],"channel_events":5,"sysex_events":0},
               {"port":1,"tracks":[1],"channel_events":3,"sysex_events":0}])
    );
    let root = std::env::temp_dir().join(format!("tunemend-ports-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("ports.mid"), &routed).unwrap();
    let mut service = Service::new(&root);
    let loaded = service.call("load_midi", json!({"job_id":"load","path":"ports.mid"}));
    assert_eq!(
        loaded.result["editable_projection"]["preview"]["receivers"],
        analysis["preview"]["receivers"]
    );
    let rendered = service.call(
        "render_preview",
        json!({"job_id":"ports","source":loaded.result["source"],
        "destination":"ports","sample_rate":16000,"layout":{"kind":"full","loop_passes":1}}),
    );
    assert_eq!(rendered.status, Status::Ok, "{rendered:?}");
    assert_eq!(
        rendered.result["render"]["receivers"],
        analysis["preview"]["receivers"]
    );
    assert_eq!(
        std::fs::read(root.join("ports/preview.wav")).unwrap(),
        tunemend::render::wav_bytes_in(
            16000,
            &both.audio.left,
            &both.audio.right,
            tunemend::review::WavFormat::Float32
        )
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// Sequence 0 runs at 240 BPM; sequence 1 has no tempo event and plays at 120.
fn sequences() -> Vec<Vec<TrackEvent<'static>>> {
    vec![
        vec![
            event(0, tempo(250_000)),
            event(0, on(0, 60, 100)),
            event(480, off(0, 60)),
            event(0, end()),
        ],
        vec![
            event(0, on(0, 64, 100)),
            event(240, cc(0, 10, 20)),
            event(240, off(0, 64)),
            event(0, end()),
        ],
    ]
}

#[test]
fn a_selected_type_2_sequence_plays_alone_on_its_own_clock() {
    let font = reference_soundfont();
    let file = smf(Format::Sequential, ppq(480), sequences());
    for (index, expected) in [(0usize, 4000u64), (1, 8000)] {
        let selected = render_midi_sequence(&file, Some(index), &FULL, &font, 16000).unwrap();
        let alone = smf(
            Format::SingleTrack,
            ppq(480),
            vec![sequences().swap_remove(index)],
        );
        assert_eq!(selected.audio.content_samples, expected);
        assert_eq!(
            selected.audio,
            render_midi_source(&alone, &FULL, &font, 16000).unwrap()
        );
        assert_eq!(selected.sequence, Some(index));
        // Excerpt ticks count from the sequence's own start.
        let excerpt = render_midi_sequence(
            &file,
            Some(index),
            &RenderLayout::Excerpt {
                start: 240,
                end: 480,
            },
            &font,
            16000,
        )
        .unwrap();
        assert_eq!(excerpt.audio.content_samples, expected / 2);
        assert_eq!(
            excerpt.audio.left[..expected as usize / 2],
            selected.audio.left[expected as usize / 2..expected as usize]
        );
    }
    assert!(render_midi_sequence(&file, None, &FULL, &font, 16000).is_err());
    assert!(render_midi_sequence(&file, Some(2), &FULL, &font, 16000).is_err());
    let type_0 = smf(Format::SingleTrack, ppq(480), vec![sequences().remove(1)]);
    assert!(render_midi_sequence(&type_0, Some(0), &FULL, &font, 16000).is_err());

    // A blocker in another sequence does not affect the selected one; one in
    // the selected sequence refuses at its original location.
    let mut blocked = sequences();
    blocked[0].insert(1, event(0, cc(0, 32, 1)));
    let blocked = smf(Format::Sequential, ppq(480), blocked);
    assert!(render_midi_sequence(&blocked, Some(1), &FULL, &font, 16000).is_ok());
    match render_midi_sequence(&blocked, Some(0), &FULL, &font, 16000) {
        Err(tunemend::render::RenderError::MidiEvents(d)) => {
            assert_eq!(d.len(), 1);
            assert_eq!(d[0].location, "/tracks/0/events/1");
        }
        other => panic!("{other:?}"),
    }
    let analysis = tunemend::midi::analyze_smf(&blocked);
    let preview = &analysis["preview"];
    assert_eq!(preview["available"], false);
    assert_eq!(preview["sequence_count"], 2);
    assert_eq!(preview["sequences"][0]["available"], false);
    assert_eq!(
        preview["sequences"][0]["diagnostics"][0]["location"],
        "/tracks/0/events/1"
    );
    assert_eq!(preview["sequences"][1]["available"], true);

    let root = std::env::temp_dir().join(format!("tunemend-type2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("type2.mid"), &file).unwrap();
    std::fs::write(root.join("type0.mid"), &type_0).unwrap();
    let mut service = Service::new(&root);
    let loaded = service.call("load_midi", json!({"job_id":"load","path":"type2.mid"}));
    let source = &loaded.result["source"];
    let unselected = service.call(
        "render_preview",
        json!({"job_id":"none","source":source,
        "destination":"none","layout":{"kind":"full","loop_passes":1}}),
    );
    assert_eq!(unselected.status, Status::Blocked);
    assert_eq!(unselected.diagnostics[0].location, "/sequence");
    assert!(!root.join("none").exists());
    let selected = service.call(
        "render_preview",
        json!({"job_id":"one","source":source,"sequence":1,
        "destination":"one","sample_rate":16000,"layout":{"kind":"full","loop_passes":1}}),
    );
    assert_eq!(selected.status, Status::Ok, "{selected:?}");
    assert_eq!(selected.result["render"]["sequence"]["index"], 1);
    assert_eq!(selected.result["render"]["sequence"]["count"], 2);
    assert_eq!(selected.result["render"]["content_samples"], 8000);
    let loaded = service.call("load_midi", json!({"job_id":"load0","path":"type0.mid"}));
    let refused = service.call(
        "render_preview",
        json!({"job_id":"type0","source":loaded.result["source"],
        "sequence":0,"destination":"type0","layout":{"kind":"full","loop_passes":1}}),
    );
    assert_eq!(refused.status, Status::Blocked);
    assert_eq!(refused.diagnostics[0].location, "/sequence");
    std::fs::remove_dir_all(root).unwrap();

    // Requests without a sequence keep their canonical form and job hash.
    let request = json!({"job_id":"r","source":"s","destination":"d","layout":{"kind":"full","loop_passes":1}});
    let canonical =
        serde_json::to_value(serde_json::from_value::<RenderPreviewInput>(request).unwrap())
            .unwrap();
    assert_eq!(
        canonical,
        json!({"job_id":"r","project":null,"source":"s","destination":"d","revision":null,
            "layout":{"kind":"full","loop_passes":1},"voices":null,"sample_rate":null})
    );
}

/// Seconds-to-sample: the first sample at or after `ticks` with the given
/// tempo segments, as exact integers.
fn boundary(segments: &[(u64, u64)], ticks: u64, ppq: u64, rate: u64) -> u64 {
    // segments: (start tick, microseconds per quarter), ascending.
    let mut micros_x_ppq = 0u128; // microseconds * ppq
    for (i, &(start, tempo)) in segments.iter().enumerate() {
        let stop = segments.get(i + 1).map_or(ticks, |s| s.0.min(ticks));
        if start < stop {
            micros_x_ppq += u128::from((stop - start) * tempo);
        }
    }
    let numerator = micros_x_ppq * u128::from(rate);
    let denominator = u128::from(ppq) * 1_000_000;
    numerator.div_ceil(denominator) as u64
}

#[test]
fn late_excerpts_match_the_full_render_at_exact_sample_boundaries() {
    let font = reference_soundfont();
    let rate = 44_100;
    // Tempo changes whose boundaries fall between samples, notes held across
    // the excerpt start, and two ports.
    let conductor = vec![
        event(0, tempo(500_000)),
        event(203, tempo(333_333)),
        event(311, tempo(712_345)),
        event(800, end()),
    ];
    let lead = vec![
        event(0, port(0)),
        event(0, cc(0, 91, 40)),
        event(10, on(0, 67, 100)),
        event(400, on(0, 71, 80)),
        event(500, off(0, 67)),
        event(300, off(0, 71)),
        event(100, end()),
    ];
    let bass = vec![
        event(0, port(1)),
        event(5, on(0, 43, 110)),
        event(700, cc(0, 7, 80)),
        event(300, off(0, 43)),
        event(300, end()),
    ];
    let file = smf(Format::Parallel, ppq(96), vec![conductor, lead, bass]);
    let full = render_midi_sequence(&file, None, &FULL, &font, rate).unwrap();
    let segments = [(0, 500_000), (203, 333_333), (514, 712_345)];
    assert_eq!(
        full.audio.content_samples,
        boundary(&segments, 1314, 96, u64::from(rate))
    );
    for (start, stop) in [(203, 514), (517, 1200), (1000, 1314)] {
        let excerpt = render_midi_sequence(
            &file,
            None,
            &RenderLayout::Excerpt { start, end: stop },
            &font,
            rate,
        )
        .unwrap()
        .audio;
        let crop = boundary(&segments, start, 96, u64::from(rate)) as usize;
        let content = boundary(&segments, stop, 96, u64::from(rate)) as usize - crop;
        assert_eq!(excerpt.content_samples as usize, content, "{start}..{stop}");
        assert_eq!(
            excerpt.left[..content],
            full.audio.left[crop..crop + content]
        );
        assert_eq!(
            excerpt.right[..content],
            full.audio.right[crop..crop + content]
        );
    }

    // SMPTE 29.97 (30000/1001 frames per second), 40 ticks per frame.
    let smpte = smf(
        Format::SingleTrack,
        Timing::Timecode(midly::Fps::Fps29, 40),
        vec![vec![
            event(0, on(0, 60, 100)),
            event(77, on(0, 64, 100)),
            event(313, off(0, 60)),
            event(91, off(0, 64)),
            event(0, end()),
        ]],
    );
    let full = render_midi_source(&smpte, &FULL, &font, rate).unwrap();
    // ticks * 1001 / (30000 * 40) seconds.
    let at =
        |ticks: u64| (u128::from(ticks) * 1001 * u128::from(rate)).div_ceil(1_200_000) as usize;
    assert_eq!(full.content_samples as usize, at(481));
    let excerpt = render_midi_source(
        &smpte,
        &RenderLayout::Excerpt {
            start: 101,
            end: 400,
        },
        &font,
        rate,
    )
    .unwrap();
    let content = at(400) - at(101);
    assert_eq!(excerpt.content_samples as usize, content);
    assert_eq!(excerpt.left[..content], full.left[at(101)..at(400)]);
}

#[test]
fn refusals_keep_their_original_event_locations_per_receiver() {
    let font = reference_soundfont();
    let gm_on: Kind = TrackEventKind::SysEx(b"\x7e\x7f\x09\x01\xf7");
    // A GM reset on port 1 before port 1's own channel events is compatible
    // with that fresh receiver, even after port 0 has started.
    let late_reset = smf(
        Format::Parallel,
        ppq(480),
        vec![
            vec![
                event(0, on(0, 60, 100)),
                event(480, off(0, 60)),
                event(0, end()),
            ],
            vec![
                event(0, port(1)),
                event(120, gm_on),
                event(0, on(0, 64, 100)),
                event(240, off(0, 64)),
                event(0, end()),
            ],
        ],
    );
    let played = render_midi_sequence(&late_reset, None, &FULL, &font, 16000).unwrap();
    assert_eq!(played.receivers[1].sysex_events, 1);
    // The same reset after channel events on its own receiver refuses there.
    let mid_song = smf(
        Format::Parallel,
        ppq(480),
        vec![
            vec![
                event(0, on(0, 60, 100)),
                event(480, off(0, 60)),
                event(0, end()),
            ],
            vec![
                event(0, port(1)),
                event(0, on(0, 64, 100)),
                event(120, gm_on),
                event(240, off(0, 64)),
                event(0, end()),
            ],
        ],
    );
    let poly = smf(
        Format::Parallel,
        ppq(480),
        vec![
            vec![
                event(0, on(0, 60, 100)),
                event(480, off(0, 60)),
                event(0, end()),
            ],
            vec![
                event(0, port(1)),
                event(0, on(0, 64, 100)),
                event(
                    60,
                    midi(
                        0,
                        MidiMessage::Aftertouch {
                            key: u7::new(64),
                            vel: u7::new(50),
                        },
                    ),
                ),
                event(0, cc(0, 32, 1)),
                event(240, off(0, 64)),
                event(0, end()),
            ],
        ],
    );
    let device = |name: &'static [u8]| TrackEventKind::Meta(MetaMessage::DeviceName(name));
    let devices = smf(
        Format::Parallel,
        ppq(480),
        vec![
            vec![
                event(0, device(b"A")),
                event(0, on(0, 60, 1)),
                event(480, end()),
            ],
            vec![
                event(0, device(b"B")),
                event(0, on(0, 60, 1)),
                event(480, end()),
            ],
        ],
    );
    let many_ports = smf(
        Format::SingleTrack,
        ppq(480),
        (0..1)
            .map(|_| {
                let mut track = Vec::new();
                for p in 0..17 {
                    track.push(event(0, port(p)));
                    track.push(event(0, on(0, 60, 1)));
                }
                track.push(event(480, end()));
                track
            })
            .collect(),
    );
    for (file, locations) in [
        (&mid_song, vec!["/tracks/1/events/2"]),
        (&poly, vec!["/tracks/1/events/2", "/tracks/1/events/3"]),
        (&devices, vec!["/tracks/1/events/1"]),
        (&many_ports, vec!["/tracks/0/events/33"]),
    ] {
        let Err(tunemend::render::RenderError::MidiEvents(diagnostics)) =
            render_midi_sequence(file, None, &FULL, &font, 16000)
        else {
            panic!("expected located refusal");
        };
        let found: Vec<_> = diagnostics.iter().map(|d| d.location.as_str()).collect();
        assert_eq!(found, locations);
        let analysis = tunemend::midi::analyze_smf(file);
        assert_eq!(analysis["preview"]["available"], false);
        assert_eq!(
            analysis["preview"]["source_diagnostics"],
            json!(diagnostics)
        );
    }
}

#[test]
fn an_empty_multi_port_selection_and_excerpt_finish_as_silence() {
    let routed = smf(
        Format::Parallel,
        ppq(480),
        vec![
            vec![
                event(0, port(0)),
                event(480, on(0, 60, 100)),
                event(240, off(0, 60)),
                event(240, end()),
            ],
            vec![
                event(0, port(7)),
                event(480, on(0, 64, 100)),
                event(240, off(0, 64)),
                event(240, end()),
            ],
        ],
    );
    let font = reference_soundfont();
    for (layout, voices) in [
        (RenderLayout::Excerpt { start: 0, end: 240 }, None),
        (FULL, Some(Vec::<String>::new())),
    ] {
        let audio =
            tunemend::render::render_midi(&routed, &layout, voices.as_deref(), &font, 16000)
                .unwrap();
        assert!(audio.content_samples > 0);
        assert!(audio.left.iter().chain(&audio.right).all(|x| *x == 0.0));
        assert!(!audio.tail_truncated);
    }
}
