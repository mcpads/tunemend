//! Loading does not depend on note pairing, a composition model or a target.
use midly::num::{u4, u7, u15, u24, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use serde_json::{Value, json};
use tunemend::midi::{compare_midi, source_events, source_layout, source_summary};
use tunemend::service::{Service, Status};

fn midi(message: MidiMessage) -> TrackEventKind<'static> {
    TrackEventKind::Midi {
        channel: u4::new(4),
        message,
    }
}
fn file(format: Format, timing: Timing, events: Vec<TrackEventKind<'static>>) -> Vec<u8> {
    let mut tracks = vec![
        events
            .into_iter()
            .map(|kind| TrackEvent {
                delta: u28::new(0),
                kind,
            })
            .collect(),
    ];
    if format == Format::Sequential {
        tracks.push(vec![TrackEvent {
            delta: u28::new(5),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(750_000))),
        }]);
    }
    let mut bytes = Vec::new();
    Smf {
        header: Header::new(format, timing),
        tracks,
    }
    .write_std(&mut bytes)
    .unwrap();
    bytes
}
fn difficult() -> Vec<u8> {
    let events = vec![
        TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(3))),
        TrackEventKind::Meta(MetaMessage::MidiChannel(u4::new(4))),
        TrackEventKind::Meta(MetaMessage::Text(b"\xff\xfeoriginal")),
        TrackEventKind::Meta(MetaMessage::Unknown(0x70, b"unmodeled\x00\xff")),
        TrackEventKind::Meta(MetaMessage::SequencerSpecific(b"\x01\x02")),
        TrackEventKind::SysEx(b"\x7d\x01\x02\xf7"),
        TrackEventKind::Escape(b"\xf8"),
        midi(MidiMessage::Controller {
            controller: u7::new(64),
            value: u7::new(127),
        }),
        midi(MidiMessage::Controller {
            controller: u7::new(99),
            value: u7::new(1),
        }),
        midi(MidiMessage::Controller {
            controller: u7::new(98),
            value: u7::new(2),
        }),
        midi(MidiMessage::Controller {
            controller: u7::new(6),
            value: u7::new(35),
        }),
        midi(MidiMessage::ChannelAftertouch { vel: u7::new(9) }),
        midi(MidiMessage::Aftertouch {
            key: u7::new(60),
            vel: u7::new(45),
        }),
        midi(MidiMessage::NoteOn {
            key: u7::new(60),
            vel: u7::new(100),
        }),
        midi(MidiMessage::NoteOn {
            key: u7::new(60),
            vel: u7::new(90),
        }),
        TrackEventKind::Meta(MetaMessage::EndOfTrack),
    ];
    let bytes = file(
        Format::Sequential,
        Timing::Timecode(midly::Fps::Fps29, 80),
        events,
    );
    extended(&bytes)
}
fn extended(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes[..14].to_vec();
    out[4..8].copy_from_slice(&8u32.to_be_bytes());
    out.extend([0x12, 0x34]);
    out.extend(b"EXTR");
    out.extend(3u32.to_be_bytes());
    out.extend([9, 8, 7]);
    out.extend(&bytes[14..]);
    out
}
fn riff(smf: &[u8]) -> Vec<u8> {
    let mut body = b"RMIDJUNK\x03\0\0\0abc\x7fdata".to_vec();
    body.extend((smf.len() as u32).to_le_bytes());
    body.extend(smf);
    if smf.len() % 2 == 1 {
        body.push(0x55);
    }
    body.extend(b"JUNK\x02\0\0\0xy");
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out.extend(b"retained-trailer");
    out
}
fn ok(service: &mut Service, tool: &str, input: Value) -> Value {
    let result = service.call(tool, input);
    assert_eq!(result.status, Status::Ok, "{result:?}");
    result.result
}

#[test]
fn all_source_events_and_bytes_load_without_an_editable_projection() {
    let root = std::env::temp_dir().join(format!("tunemend-loading-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let bytes = riff(&difficult());
    std::fs::write(root.join("source.rmi"), &bytes).unwrap();
    let mut service = Service::new(&root);
    assert!(
        ok(&mut service, "inspect_midi", json!({"view": "sources"}))["sources"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!root.join(".tunemend").exists());
    let result = ok(
        &mut service,
        "load_midi",
        json!({"job_id": "load", "path": "source.rmi"}),
    );
    let source = result["source"].clone();
    assert_eq!(result["format"], 2);
    assert_eq!(result["container"], "rmid");
    assert_eq!(
        result["timing"]["frames_per_second"],
        json!({"numerator": 30000, "denominator": 1001})
    );
    assert_eq!(result["editable_projection"]["available"], false);
    assert_eq!(result["events"], 17);
    let mut events = Vec::new();
    let mut cursor = 0;
    loop {
        let page = ok(
            &mut service,
            "inspect_midi",
            json!({"source": source, "view": "events", "offset": cursor, "limit": 3}),
        );
        events.extend(page["events"].as_array().unwrap().clone());
        let Some(next) = page["next_cursor"].as_u64() else {
            break;
        };
        cursor = next;
    }
    assert_eq!(events.len(), 17);
    assert_eq!(events[13]["message"]["velocity"], 100);
    assert_eq!(events[14]["message"]["velocity"], 90);
    assert_eq!(events[13]["tick"], events[14]["tick"]);
    assert_eq!(events[15]["message"]["kind"], "end_of_track");
    assert_eq!(events[16]["tick"], 5);
    let mut reconstructed = Vec::new();
    cursor = 0;
    loop {
        let page = ok(
            &mut service,
            "inspect_midi",
            json!({"source": source, "view": "bytes", "offset": cursor, "limit": 31}),
        );
        reconstructed.extend(
            page["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b.as_u64().unwrap() as u8),
        );
        let Some(next) = page["next_cursor"].as_u64() else {
            break;
        };
        cursor = next;
    }
    assert_eq!(reconstructed, bytes);
    let projection = service.call(
        "import_midi",
        json!({"job_id": "projection", "project": "optional", "source": source}),
    );
    assert_eq!(projection.status, Status::Blocked);
    assert!(
        ok(&mut service, "inspect_project", json!({"view": "projects"}))["projects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    drop(service);
    std::fs::remove_file(root.join("source.rmi")).unwrap();
    let mut resumed = Service::new(&root);
    let retried = resumed.call("load_midi", json!({"job_id": "load", "path": "source.rmi"}));
    assert_eq!(retried.status, Status::Ok);
    assert!(retried.replayed);
    let exported = ok(
        &mut resumed,
        "export_midi_source",
        json!({"job_id": "export", "source": source, "destination": "out/exact.rmi"}),
    );
    assert_eq!(exported["readback_checked"], true);
    assert_eq!(std::fs::read(root.join("out/exact.rmi")).unwrap(), bytes);
    assert_eq!(
        ok(&mut resumed, "inspect_midi", json!({"view": "sources"}))["sources"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(resumed);
    // A preexisting content-addressed object cannot silently substitute bytes.
    std::fs::write(
        root.join(".tunemend/objects")
            .join(source.as_str().unwrap()),
        b"corrupt",
    )
    .unwrap();
    std::fs::write(root.join("source.rmi"), &bytes).unwrap();
    let mut corrupted = Service::new(&root);
    let result = corrupted.call(
        "load_midi",
        json!({"job_id": "reload-corrupt", "path": "source.rmi"}),
    );
    assert_eq!(result.status, Status::Error);
    let result = corrupted.call("inspect_midi", json!({"source": source}));
    assert_eq!(result.status, Status::Blocked);
    drop(corrupted);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn loader_accepts_empty_and_ambiguous_music_but_refuses_broken_containers() {
    for format in [Format::SingleTrack, Format::Parallel, Format::Sequential] {
        for timing in [
            Timing::Metrical(u15::new(480)),
            Timing::Timecode(midly::Fps::Fps25, 40),
        ] {
            let bytes = file(
                format,
                timing,
                vec![TrackEventKind::Meta(MetaMessage::EndOfTrack)],
            );
            let summary = source_summary(&bytes).unwrap();
            assert_eq!(summary["tracks"][0]["events"], 1);
            let page = source_events(&bytes, Some(0), 0, 10).unwrap();
            assert_eq!(page["events"][0]["message"]["kind"], "end_of_track");
            let mut broken = bytes.clone();
            broken.pop();
            assert!(source_summary(&broken).is_err());
        }
    }
    assert!(source_summary(b"not a MIDI").is_err());
    let plain = file(
        Format::SingleTrack,
        Timing::Metrical(u15::new(480)),
        vec![TrackEventKind::Meta(MetaMessage::EndOfTrack)],
    );
    let wrapped = extended(&plain);
    let mut relocated = wrapped[..16].to_vec();
    relocated.extend(&wrapped[27..]);
    relocated.extend(&wrapped[16..27]);
    let comparison = compare_midi(&wrapped, &relocated);
    assert_eq!(comparison["events_identical"], true);
    assert_eq!(comparison["container_identical"], false);
}

#[test]
fn source_edit_keeps_extended_metadata_and_unknown_smf_and_riff_chunks() {
    use tunemend::{
        export::MidiExport,
        midi::{MidiSource, read_smf_preserving, write_preserving},
    };
    let bytes = file(
        Format::SingleTrack,
        Timing::Metrical(u15::new(480)),
        vec![
            TrackEventKind::Meta(MetaMessage::Tempo(u24::new(500_000))),
            midi(MidiMessage::NoteOn {
                key: u7::new(60),
                vel: u7::new(100),
            }),
            // This fixture changes the release delta below to give a nonzero note.
            midi(MidiMessage::NoteOff {
                key: u7::new(60),
                vel: u7::new(64),
            }),
            TrackEventKind::Meta(MetaMessage::EndOfTrack),
        ],
    );
    let mut parsed = Smf::parse(&bytes).unwrap();
    parsed.tracks[0][2].delta = u28::new(120);
    let mut bytes = Vec::new();
    parsed.write_std(&mut bytes).unwrap();
    // Accepted metadata extension beyond the three-byte tempo value.
    let track_length = u32::from_be_bytes(bytes[18..22].try_into().unwrap());
    bytes[18..22].copy_from_slice(&(track_length + 2).to_be_bytes());
    bytes[25] = 5;
    bytes.splice(29..29, [0x12, 0x34]);
    let bytes = riff(&extended(&bytes));
    let mut altered = bytes.clone();
    let at = altered
        .windows(8)
        .position(|w| w == [0xff, 0x51, 5, 7, 0xa1, 0x20, 0x12, 0x34])
        .unwrap();
    altered[at + 7] = 0x35;
    assert_eq!(
        compare_midi(&bytes, &altered)["aligned_event_differences"],
        1
    );
    assert_eq!(compare_midi(&bytes, &altered)["events_identical"], false);
    let imported = read_smf_preserving(&bytes).unwrap();
    let mut edited = imported.composition.clone();
    edited.notes[0].velocity = 99;
    let plan = MidiExport {
        id: "test".into(),
        revision: "r1".into(),
        timing: None,
        assignments: imported.assignments,
        sections_as_markers: false,
    };
    let source = MidiSource {
        sha256: tunemend::store::sha256_hex(&bytes),
        bytes: bytes.clone(),
    };
    let output = write_preserving(&source, imported.timebase, &edited, &plan).unwrap();
    let comparison = compare_midi(&bytes, &output);
    assert_eq!(comparison["aligned_event_differences"], 1);
    assert_eq!(comparison["container_identical"], true);
    assert!(
        output
            .windows(7)
            .any(|w| w == [0xff, 0x51, 5, 7, 0xa1, 0x20, 0x12])
    );
    edited.tempo_map[0].microseconds_per_quarter = 600_000;
    let output = write_preserving(&source, imported.timebase, &edited, &plan).unwrap();
    assert!(
        output
            .windows(8)
            .any(|w| w == [0xff, 0x51, 5, 9, 0x27, 0xc0, 0x12, 0x34])
    );
    assert_eq!(compare_midi(&bytes, &output)["container_identical"], true);
    let layout = source_layout(&output).unwrap();
    assert!(layout.chunks.iter().any(|c| &c.id == b"EXTR"));
}

#[test]
fn opaque_containers_load_directly_and_keep_parent_and_child_identity() {
    let root = std::env::temp_dir().join(format!("tunemend-container-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let first = difficult();
    let second = file(
        Format::SingleTrack,
        Timing::Metrical(u15::new(96)),
        vec![TrackEventKind::Meta(MetaMessage::EndOfTrack)],
    );
    let mut bytes = b"unknown archive index MThd\x00\x00\x00\x00".to_vec();
    let first_offset = bytes.len();
    bytes.extend(&first);
    bytes.extend(b"opaque between streams");
    let second_offset = bytes.len();
    bytes.extend(&second);
    bytes.extend(b"opaque trailer");
    std::fs::write(root.join("archive.bin"), &bytes).unwrap();
    let mut service = Service::new(&root);
    let loaded = ok(
        &mut service,
        "load_midi",
        json!({"job_id": "load", "path": "archive.bin"}),
    );
    let children = loaded["children"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0]["offset"], first_offset);
    assert_eq!(children[0]["length"], first.len());
    assert_eq!(children[1]["offset"], second_offset);
    assert_eq!(children[1]["length"], second.len());
    let summary = ok(
        &mut service,
        "inspect_midi",
        json!({"source": children[0]["source"]}),
    );
    assert_eq!(summary["format"], 2);
    for (index, child) in children.iter().enumerate() {
        ok(
            &mut service,
            "export_midi_source",
            json!({"job_id": format!("child-{index}"), "source": child["source"], "destination": format!("out/{index}.mid")}),
        );
    }
    assert_eq!(std::fs::read(root.join("out/0.mid")).unwrap(), first);
    assert_eq!(std::fs::read(root.join("out/1.mid")).unwrap(), second);
    ok(
        &mut service,
        "export_midi_source",
        json!({"job_id": "parent", "source": loaded["source"], "destination": "out/archive.bin"}),
    );
    assert_eq!(std::fs::read(root.join("out/archive.bin")).unwrap(), bytes);
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}
