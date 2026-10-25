//! Target plans retain complete source data and verify actual score bytes.
use serde_json::{Value, json};
use std::path::PathBuf;
use tunemend::service::{Service, Status};

fn fixture(name: &str) -> (PathBuf, Service, Value) {
    let root = std::env::temp_dir().join(format!("tunemend-score-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("encoder.py"),
        include_str!("../fixtures/score-encoder-fixture.py"),
    )
    .unwrap();
    let mut template = vec![0, 0x3a, 0x5e, 1];
    template.extend(vec![12; 0x15e]);
    template.extend([255, 255]);
    std::fs::write(root.join("template.bin"), template).unwrap();
    std::fs::write(root.join("bank.bin"), [0, 0x45, 2, 0, 7, 8, 255, 255]).unwrap();
    // Tempo, opaque metadata, pressure, two notes and an explicit end.
    let track = [
        0, 255, 81, 3, 7, 161, 32, 0, 255, 112, 2, 1, 2, 0, 0xd0, 44, 0, 0x90, 60, 100, 12, 0x80,
        60, 50, 12, 0x90, 64, 90, 12, 0x80, 64, 40, 0, 255, 47, 0,
    ];
    let mut midi = b"MThd\0\0\0\x06\0\0\0\x01\0\x30MTrk".to_vec();
    midi.extend((track.len() as u32).to_be_bytes());
    midi.extend(track);
    std::fs::write(root.join("input.mid"), midi).unwrap();
    let mut service = Service::new(&root);
    let loaded = service.call("load_midi", json!({"job_id":"load", "path":"input.mid"}));
    assert_eq!(loaded.status, Status::Ok, "{loaded:?}");
    let request = json!({"job_id":"prepare", "source":loaded.result["source"],
        "target":{"encoder":"encoder.py", "template":"template.bin", "bank":"bank.bin",
            "python": if cfg!(windows) {"python"} else {"python3"}},
        "mapping":{"ppqn":12,"whole_bars":false,"tracks":[{"name":"tone", "channels":[0], "volume":25}]}});
    (root, service, request)
}

#[test]
fn target_plan_accounts_for_all_events_and_exports_an_immutable_candidate() {
    let (root, mut service, request) = fixture("plan");
    let result = service.call("prepare_target", request.clone());
    assert_eq!(result.status, Status::Ok, "{result:?}");
    assert!(
        result.result["changes"]
            .get("channel_aftertouch_not_encoded")
            .is_some()
    );
    assert_eq!(result.result["verification"]["score_readback"], true);
    assert_eq!(
        service.call("prepare_target", request).result,
        result.result
    );
    let plan = result.result["plan"].clone();
    let events = service.call(
        "inspect_target",
        json!({"plan":plan,"view":"events","limit":500}),
    );
    assert_eq!(events.result["items"].as_array().unwrap().len(), 8);
    let refused = service.call(
        "export_target",
        json!({"job_id":"refuse","plan":plan,"accepted_changes":[],"destination":"out/no"}),
    );
    assert_eq!(refused.status, Status::Blocked);
    assert!(!root.join("out/no").exists());
    let changes: Vec<_> = result.result["changes"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    // The prepared result survives changing/removing external dependencies.
    std::fs::remove_file(root.join("encoder.py")).unwrap();
    std::fs::remove_file(root.join("template.bin")).unwrap();
    drop(service);
    let mut service = Service::new(&root);
    let export = service.call(
        "export_target",
        json!({"job_id":"export","plan":plan,"accepted_changes":changes,"destination":"out/yes"}),
    );
    assert_eq!(export.status, Status::Ok, "{export:?}");
    assert_eq!(
        std::fs::read(root.join("input.mid")).unwrap(),
        std::fs::read(root.join("out/yes/source.mid")).unwrap()
    );
    let score = std::fs::read(root.join("out/yes/score.bin")).unwrap();
    assert_eq!(
        &score[4 + 0x16d..],
        &[49, 222, 3, 0, 222, 3, 53, 222, 3, 128, 109, 59, 255, 255]
    );
    let overwrite = service.call("export_target", json!({"job_id":"overwrite","plan":plan,"accepted_changes":changes,"destination":"out/yes"}));
    assert_eq!(overwrite.status, Status::Blocked);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn corrupt_score_and_unsupported_mapping_never_produce_a_plan() {
    let (root, mut service, mut request) = fixture("refusal");
    let original = std::fs::read_to_string(root.join("encoder.py")).unwrap();
    std::fs::write(
        root.join("encoder.py"),
        original.replace("CORRUPT = False", "CORRUPT = True"),
    )
    .unwrap();
    let result = service.call("prepare_target", request.clone());
    assert_eq!(result.status, Status::Blocked, "{result:?}");
    assert!(result.summary.contains("does not match mapped events"));
    assert!(result.result.get("plan").is_none());
    std::fs::write(root.join("encoder.py"), original).unwrap();
    request["job_id"] = json!("bad-grid");
    request["mapping"]["ppqn"] = json!(13);
    let result = service.call("prepare_target", request);
    assert_eq!(result.status, Status::Blocked);
    assert!(result.summary.contains("divide source PPQ"));
    assert_eq!(service.call("export_midi_source", json!({"job_id":"original","source":fixture_source(&root),"destination":"original.mid"})).status, Status::Ok);
    let _ = std::fs::remove_dir_all(root);
}

fn fixture_source(root: &std::path::Path) -> String {
    tunemend::store::sha256_hex(&std::fs::read(root.join("input.mid")).unwrap())
}

#[test]
fn newly_created_project_can_prepare_a_target_without_loading_a_source() {
    let (root, _, mut request) = fixture("created");
    let imported =
        tunemend::midi::read_smf_preserving(&std::fs::read(root.join("input.mid")).unwrap())
            .unwrap();
    let fresh = root.join("fresh");
    std::fs::create_dir(&fresh).unwrap();
    let mut service = Service::new(&fresh);
    let created = service.call("create_project", json!({"job_id":"create", "project":"original", "timebase":imported.timebase, "composition":imported.composition}));
    assert_eq!(created.status, Status::Ok, "{created:?}");
    request.as_object_mut().unwrap().remove("source");
    request["project"] = json!("original");
    for key in ["encoder", "template", "bank"] {
        request["target"][key] = json!(root.join(request["target"][key].as_str().unwrap()));
    }
    let result = service.call("prepare_target", request);
    assert_eq!(result.status, Status::Ok, "{result:?}");
    assert_eq!(
        service
            .call("inspect_midi", json!({"view":"sources"}))
            .result["sources"],
        json!([])
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn simultaneous_notes_require_a_choice_and_each_discard_is_accounted() {
    let (root, mut service, mut request) = fixture("chord");
    let source = std::fs::read(root.join("input.mid")).unwrap();
    let mut midi = midly::Smf::parse(&source).unwrap();
    let on = midly::TrackEvent {
        delta: 0.into(),
        kind: midly::TrackEventKind::Midi {
            channel: 0.into(),
            message: midly::MidiMessage::NoteOn {
                key: 65.into(),
                vel: 90.into(),
            },
        },
    };
    let off = midly::TrackEvent {
        delta: 0.into(),
        kind: midly::TrackEventKind::Midi {
            channel: 0.into(),
            message: midly::MidiMessage::NoteOff {
                key: 65.into(),
                vel: 50.into(),
            },
        },
    };
    midi.tracks[0].insert(4, on);
    midi.tracks[0].insert(6, off);
    let mut bytes = Vec::new();
    midi.write_std(&mut bytes).unwrap();
    std::fs::write(root.join("chord.mid"), bytes).unwrap();
    let source = service.call("load_midi", json!({"job_id":"chord", "path":"chord.mid"}));
    request["source"] = source.result["source"].clone();
    let blocked = service.call("prepare_target", request.clone());
    assert_eq!(blocked.status, Status::Blocked);
    assert!(blocked.summary.contains("same_tick=last"), "{blocked:?}");
    request["job_id"] = json!("select");
    request["mapping"]["tracks"][0]["same_tick"] = json!("last");
    let selected = service.call("prepare_target", request);
    assert_eq!(selected.status, Status::Ok, "{selected:?}");
    assert_eq!(
        selected.result["changes"]["note_removed_by_monophonic_track"],
        1
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn intro_is_played_once_and_a_loop_back_to_the_intro_is_rejected() {
    let (root, mut service, mut request) = fixture("intro");
    let source = std::fs::read(root.join("input.mid")).unwrap();
    let mut midi = midly::Smf::parse(&source).unwrap();
    midi.tracks[0][3].delta = 12.into();
    let mut bytes = Vec::new();
    midi.write_std(&mut bytes).unwrap();
    std::fs::write(root.join("intro.mid"), bytes).unwrap();
    let loaded = service.call("load_midi", json!({"job_id":"intro", "path":"intro.mid"}));
    request["source"] = loaded.result["source"].clone();
    request["mapping"]["intro_once"] = json!(true);
    let plan = service.call("prepare_target", request.clone());
    assert_eq!(plan.status, Status::Ok, "{plan:?}");
    assert_eq!(plan.result["changes"]["intro_played_once"], 1);
    let tracks = service.call(
        "inspect_target",
        json!({"plan":plan.result["plan"], "view":"tracks"}),
    );
    assert_eq!(tracks.result["items"][0]["loop_start"], 0x3b70);
    assert_eq!(tracks.result["items"][0]["intro_ticks"], 3);
    let events = service.call(
        "inspect_target",
        json!({"plan":plan.result["plan"], "view":"score_events", "track":"tone", "limit":1}),
    );
    assert_eq!(events.result["items"], json!([[0, 3, 0]]));
    assert_eq!(events.result["next_cursor"], 1);
    let encoder = std::fs::read_to_string(root.join("encoder.py")).unwrap();
    std::fs::write(
        root.join("encoder.py"),
        encoder.replace("CORRUPT_LOOP = False", "CORRUPT_LOOP = True"),
    )
    .unwrap();
    request["job_id"] = json!("bad-loop");
    let bad = service.call("prepare_target", request);
    assert_eq!(bad.status, Status::Blocked);
    assert!(bad.summary.contains("loop return"), "{bad:?}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn single_port_source_maps_without_discarding_routing_metadata_silently() {
    let (root, mut service, request) = fixture("single-port");
    let baseline = service.call("prepare_target", request.clone());
    assert_eq!(baseline.status, Status::Ok, "{baseline:?}");
    let original = std::fs::read(root.join("input.mid")).unwrap();
    // Port 9 and channel prefix 7 do not change the channel 0 note messages.
    let prefix = [0, 255, 33, 1, 9, 0, 255, 32, 1, 7];
    let mut routed = original[..18].to_vec();
    routed.extend(((original.len() - 22 + prefix.len()) as u32).to_be_bytes());
    routed.extend(prefix);
    routed.extend(&original[22..]);
    std::fs::write(root.join("routed.mid"), &routed).unwrap();
    let loaded = service.call(
        "load_midi",
        json!({"job_id":"routing-load", "path":"routed.mid"}),
    );
    assert_eq!(loaded.status, Status::Ok);
    let mut mapped = request;
    mapped["job_id"] = json!("routing-prepare");
    mapped["source"] = loaded.result["source"].clone();
    let result = service.call("prepare_target", mapped.clone());
    assert_eq!(result.status, Status::Ok, "{result:?}");
    assert_eq!(result.result["verification"]["source_events_accounted"], 10);
    assert_eq!(result.result["changes"]["metadata_not_encoded"], 3);
    for (name, prepared) in [("plain", baseline), ("routed", result)] {
        let changes: Vec<_> = prepared.result["changes"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        let exported = service.call("export_target", json!({"job_id":name, "plan":prepared.result["plan"], "accepted_changes":changes, "destination":format!("out/{name}")}));
        assert_eq!(exported.status, Status::Ok, "{exported:?}");
    }
    assert_eq!(
        std::fs::read(root.join("out/plain/score.bin")).unwrap(),
        std::fs::read(root.join("out/routed/score.bin")).unwrap()
    );
    assert_eq!(
        std::fs::read(root.join("out/routed/source.mid")).unwrap(),
        routed
    );
    // The other receiver must not be merged just because note channels match.
    let mut mixed = routed.clone();
    let at = mixed.len() - 4;
    mixed.splice(at..at, [0, 255, 33, 1, 10, 0, 0xb0, 7, 100]);
    let track_size = (mixed.len() - 22) as u32;
    mixed[18..22].copy_from_slice(&track_size.to_be_bytes());
    std::fs::write(root.join("mixed.mid"), mixed).unwrap();
    let loaded = service.call(
        "load_midi",
        json!({"job_id":"mixed-load", "path":"mixed.mid"}),
    );
    mapped["job_id"] = json!("mixed-prepare");
    mapped["source"] = loaded.result["source"].clone();
    let blocked = service.call("prepare_target", mapped);
    assert_eq!(blocked.status, Status::Blocked);
    assert!(blocked.summary.contains("multiple MIDI output ports"));
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}
