use midly::num::{u4, u7, u15, u24, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use serde_json::json;
use tunemend::{
    render::render_midi_source,
    review::RenderLayout,
    service::{Service, Status},
    soundfont::reference_soundfont,
};

fn event(delta: u32, kind: TrackEventKind<'static>) -> TrackEvent<'static> {
    TrackEvent {
        delta: u28::new(delta),
        kind,
    }
}
fn on(velocity: u8) -> TrackEventKind<'static> {
    TrackEventKind::Midi {
        channel: u4::new(0),
        message: MidiMessage::NoteOn {
            key: u7::new(60),
            vel: u7::new(velocity),
        },
    }
}
fn off() -> TrackEventKind<'static> {
    TrackEventKind::Midi {
        channel: u4::new(0),
        message: MidiMessage::NoteOff {
            key: u7::new(60),
            vel: u7::new(64),
        },
    }
}
fn encode(smf: &Smf<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    bytes
}

#[test]
fn overlapping_shared_channel_events_play_from_the_source_stream() {
    let control = TrackEventKind::Midi {
        channel: u4::new(0),
        message: MidiMessage::Controller {
            controller: u7::new(7),
            value: u7::new(60),
        },
    };
    let smf = Smf {
        header: Header::new(Format::Parallel, Timing::Metrical(u15::new(480))),
        tracks: vec![
            vec![
                event(0, on(100)),
                event(120, off()),
                event(120, TrackEventKind::Meta(MetaMessage::EndOfTrack)),
            ],
            vec![event(60, on(70)), event(120, off())],
            vec![event(90, control)],
        ],
    };
    let bytes = encode(&smf);
    // The editable projection keeps per-track notes and the shared control in
    // its own track; playback still sends the source stream to one receiver.
    let projection = tunemend::midi::read_smf_preserving(&bytes).unwrap();
    assert_eq!(projection.composition.notes.len(), 2);
    let analysis = tunemend::midi::analyze_smf(&bytes);
    assert_eq!(analysis["preview"]["available"], true);
    assert_eq!(analysis["preview"]["project_available"], true);
    let font = reference_soundfont();
    let audio =
        render_midi_source(&bytes, &RenderLayout::Full { loop_passes: 1 }, &font, 16000).unwrap();
    let mut payload = b"RMIDdata".to_vec();
    payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(&bytes);
    if !bytes.len().is_multiple_of(2) {
        payload.push(0);
    }
    let mut rmid = b"RIFF".to_vec();
    rmid.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    rmid.extend_from_slice(&payload);
    assert_eq!(
        audio,
        render_midi_source(&rmid, &RenderLayout::Full { loop_passes: 1 }, &font, 16000).unwrap()
    );
    // Independent receiver calls at known sample times, including both onsets
    // and the shared channel's controller, establish the event-stream contract.
    let mut synth = tunemend::synth::Synthesizer::new(
        std::sync::Arc::new(tunemend::synth::SoundFont::parse(&font).unwrap()),
        16000,
        false,
    )
    .unwrap();
    let mut left = vec![0.0; 4000];
    let mut right = vec![0.0; 4000];
    synth.note_on(0, 60, 100);
    synth.render(&mut left[..1000], &mut right[..1000]);
    synth.note_on(0, 60, 70);
    synth.render(&mut left[1000..1500], &mut right[1000..1500]);
    synth.control_change(0, 7, 60);
    synth.render(&mut left[1500..2000], &mut right[1500..2000]);
    synth.note_off(0, 60);
    synth.render(&mut left[2000..3000], &mut right[2000..3000]);
    synth.note_off(0, 60);
    synth.render(&mut left[3000..], &mut right[3000..]);
    assert_eq!(audio.content_samples, 4000);
    assert_eq!(audio.left[..4000], left);
    assert_eq!(audio.right[..4000], right);
    let excerpt = render_midi_source(
        &bytes,
        &RenderLayout::Excerpt {
            start: 90,
            end: 180,
        },
        &font,
        16000,
    )
    .unwrap();
    assert_eq!(excerpt.left[..1500], audio.left[1500..3000]);
    let root = std::env::temp_dir().join(format!("tunemend-source-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("source.mid"), &bytes).unwrap();
    let mut service = Service::new(&root);
    let loaded = service.call("load_midi", json!({"job_id":"load","path":"source.mid"}));
    assert_eq!(loaded.status, Status::Ok, "{loaded:?}");
    let request = json!({"job_id":"render","source":loaded.result["source"],"destination":"listen","sample_rate":16000,"layout":{"kind":"full","loop_passes":1}});
    let rendered = service.call("render_preview", request.clone());
    assert_eq!(rendered.status, Status::Ok, "{rendered:?}");
    assert_eq!(
        std::fs::read(root.join("listen/original.bin")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::read(root.join("listen/preview.wav")).unwrap(),
        tunemend::render::wav_bytes_in(
            16000,
            &audio.left,
            &audio.right,
            tunemend::review::WavFormat::Float32
        )
    );
    drop(service);
    let mut service = Service::new(&root);
    assert_eq!(service.call("render_preview", request).status, Status::Ok);
    let conflict = service.call("render_preview",json!({"job_id":"conflict","project":"x","source":loaded.result["source"],"destination":"bad","layout":{"kind":"full","loop_passes":1}}));
    assert_ne!(conflict.status, Status::Ok);
    assert!(!root.join("bad").exists());
    let mut unsupported = smf.clone();
    unsupported.tracks[0].insert(0, event(0, TrackEventKind::SysEx(b"\x7d\x01\xf7")));
    std::fs::write(root.join("unsupported.mid"), encode(&unsupported)).unwrap();
    let loaded = service.call(
        "load_midi",
        json!({"job_id":"unsupported-load","path":"unsupported.mid"}),
    );
    let refused = service.call("render_preview",json!({"job_id":"unsupported-preview","source":loaded.result["source"],"destination":"refused","layout":{"kind":"full","loop_passes":1}}));
    assert_eq!(refused.status, Status::Blocked);
    assert_eq!(refused.diagnostics[0].location, "/tracks/0/events/0");
    assert!(!root.join("refused").exists());
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn smpte_source_clock_ignores_tempo_and_unreleased_notes_get_a_transport_end() {
    let font = reference_soundfont();
    for (fps, expected) in [
        (midly::Fps::Fps24, 20_000),
        (midly::Fps::Fps25, 19_200),
        (midly::Fps::Fps29, 16_016),
        (midly::Fps::Fps30, 16_000),
    ] {
        let mut smf = Smf {
            header: Header::new(Format::SingleTrack, Timing::Timecode(fps, 80)),
            tracks: vec![vec![
                event(0, on(100)),
                event(
                    100,
                    TrackEventKind::Meta(MetaMessage::Tempo(u24::new(100_000))),
                ),
                event(2300, TrackEventKind::Meta(MetaMessage::EndOfTrack)),
            ]],
        };
        let bytes = encode(&smf);
        let audio =
            render_midi_source(&bytes, &RenderLayout::Full { loop_passes: 1 }, &font, 16000)
                .unwrap();
        assert_eq!(audio.content_samples, expected);
        assert!(audio.left.iter().any(|s| s.abs() > 0.01));
        assert!(
            !audio.tail_truncated,
            "transport should release the held note"
        );
        smf.tracks[0][1].kind = TrackEventKind::Meta(MetaMessage::Tempo(u24::new(900_000)));
        assert_eq!(
            audio,
            render_midi_source(
                &encode(&smf),
                &RenderLayout::Full { loop_passes: 1 },
                &font,
                16000
            )
            .unwrap()
        );
        smf.tracks[0].insert(1, event(0, TrackEventKind::SysEx(b"\x7d\x01\xf7")));
        assert!(
            render_midi_source(
                &encode(&smf),
                &RenderLayout::Full { loop_passes: 1 },
                &font,
                16000
            )
            .is_err()
        );
    }
}
