//! Source preservation is tested with authored MIDI, independent of consumers.
use midly::num::{u4, u7, u15, u24, u28};
use midly::{
    Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, Timing, TrackEvent, TrackEventKind,
};
use serde_json::json;
use tunemend::composition::ControlKind;
use tunemend::export::MidiExport;
use tunemend::midi::{
    MidiSource, analyze_smf, compare_midi, read_smf_preserving, write_preserving,
};
use tunemend::service::{Service, Status};

fn cc(number: u8, value: u8) -> TrackEventKind<'static> {
    midi(MidiMessage::Controller {
        controller: u7::new(number),
        value: u7::new(value),
    })
}
fn midi(message: MidiMessage) -> TrackEventKind<'static> {
    TrackEventKind::Midi {
        channel: u4::new(2),
        message,
    }
}
fn source(opaque: bool) -> Vec<u8> {
    let mut events = vec![
        (
            0,
            TrackEventKind::Meta(MetaMessage::TrackName(b"Original\xff")),
        ),
        (
            0,
            TrackEventKind::Meta(MetaMessage::TimeSignature(4, 2, 36, 12)),
        ),
        (
            0,
            TrackEventKind::Meta(MetaMessage::Tempo(u24::new(500_000))),
        ),
        (0, cc(1, 0)),
        (0, cc(91, 50)),
        (0, cc(93, 30)),
        (
            0,
            midi(MidiMessage::PitchBend {
                bend: PitchBend::from_int(-100),
            }),
        ),
        (
            0,
            midi(MidiMessage::NoteOn {
                key: u7::new(60),
                vel: u7::new(100),
            }),
        ),
        (
            0,
            midi(MidiMessage::PitchBend {
                bend: PitchBend::from_int(400),
            }),
        ),
        (
            120,
            midi(MidiMessage::NoteOn {
                key: u7::new(60),
                vel: u7::new(0),
            }),
        ),
        (120, TrackEventKind::Meta(MetaMessage::EndOfTrack)),
    ];
    if opaque {
        events.insert(
            3,
            (
                0,
                TrackEventKind::Meta(MetaMessage::SequencerSpecific(b"opaque\x00\xff")),
            ),
        );
        events.insert(4, (0, TrackEventKind::SysEx(b"\x7d\x01\xf7")));
        events.insert(5, (0, cc(64, 127)));
        events.insert(
            6,
            (0, midi(MidiMessage::ChannelAftertouch { vel: u7::new(12) })),
        );
    }
    let smf = Smf {
        header: Header::new(Format::SingleTrack, Timing::Metrical(u15::new(480))),
        tracks: vec![
            events
                .into_iter()
                .map(|(delta, kind)| TrackEvent {
                    delta: u28::new(delta),
                    kind,
                })
                .collect(),
        ],
    };
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    bytes
}
fn original(bytes: &[u8]) -> MidiSource {
    MidiSource {
        sha256: tunemend::store::sha256_hex(bytes),
        bytes: bytes.to_vec(),
        declared_ends: None,
        legacy_declared_ends: None,
    }
}
fn plan(bytes: &[u8]) -> MidiExport {
    MidiExport {
        id: "delivery".into(),
        revision: "r1".into(),
        timing: None,
        assignments: read_smf_preserving(bytes).unwrap().assignments,
        sections_as_markers: false,
    }
}
fn events(bytes: &[u8]) -> Vec<(u64, TrackEventKind<'_>)> {
    let smf = Smf::parse(bytes).unwrap();
    let mut tick = 0;
    smf.tracks[0]
        .iter()
        .map(|event| {
            tick += u64::from(event.delta.as_int());
            (tick, event.kind)
        })
        .collect()
}

#[test]
fn unchanged_roundtrip_is_byte_identical_even_with_opaque_and_ordered_events() {
    let bytes = source(true);
    let imported = read_smf_preserving(&bytes).unwrap();
    let output = write_preserving(
        &original(&bytes),
        imported.timebase,
        &imported.composition,
        &plan(&bytes),
    )
    .unwrap();
    assert_eq!(output, bytes);
    let inspection = analyze_smf(&bytes);
    assert_eq!(inspection["can_import"], true);
    // The sustain pedal and channel pressure play; the SysEx still blocks preview.
    assert_eq!(inspection["preview"]["available"], false);
    assert_eq!(inspection["preview"]["blocker_count"], 1);
    let mut corrupt = original(&bytes);
    corrupt.bytes[0] = 0;
    assert!(
        write_preserving(
            &corrupt,
            imported.timebase,
            &imported.composition,
            &plan(&bytes)
        )
        .is_err()
    );
}

#[test]
fn note_and_control_edits_preserve_every_unrelated_event_and_same_tick_order() {
    let bytes = source(true);
    let imported = read_smf_preserving(&bytes).unwrap();
    let mut edited = imported.composition.clone();
    edited.notes[0].pitch = 62;
    edited.notes[0].velocity = 84;
    edited
        .control_lanes
        .iter_mut()
        .find(|l| l.kind == ControlKind::Reverb)
        .unwrap()
        .points[0]
        .value = 75;
    let output =
        write_preserving(&original(&bytes), imported.timebase, &edited, &plan(&bytes)).unwrap();
    let expected: Vec<_> = events(&bytes)
        .into_iter()
        .map(|(tick, kind)| {
            let kind = match kind {
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::NoteOn { vel, .. },
                } => TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::NoteOn {
                        key: u7::new(62),
                        vel: if vel.as_int() == 0 { vel } else { u7::new(84) },
                    },
                },
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::Controller { controller, .. },
                } if controller.as_int() == 91 => TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::Controller {
                        controller,
                        value: u7::new(75),
                    },
                },
                other => other,
            };
            (tick, kind)
        })
        .collect();
    assert_eq!(events(&output), expected);
    assert_eq!(compare_midi(&bytes, &output)["events_identical"], false);
    // Meter editing keeps the original metronome fields too.
    edited.time_signatures[0].numerator = 3;
    let output =
        write_preserving(&original(&bytes), imported.timebase, &edited, &plan(&bytes)).unwrap();
    assert!(
        events(&output).iter().any(
            |(_, kind)| *kind == TrackEventKind::Meta(MetaMessage::TimeSignature(3, 2, 36, 12))
        )
    );
}

#[test]
fn comparison_detects_non_note_loss_and_reordering() {
    let bytes = source(true);
    let mut parsed = Smf::parse(&bytes).unwrap();
    parsed.tracks[0].retain(|event| !matches!(event.kind, TrackEventKind::SysEx(_)));
    let mut dropped = Vec::new();
    parsed.write_std(&mut dropped).unwrap();
    assert_eq!(compare_midi(&bytes, &dropped)["events_identical"], false);
    let mut parsed = Smf::parse(&bytes).unwrap();
    parsed.tracks[0].swap(0, 1);
    let mut reordered = Vec::new();
    parsed.write_std(&mut reordered).unwrap();
    assert_eq!(compare_midi(&bytes, &reordered)["events_identical"], false);
    assert_eq!(compare_midi(&bytes, &bytes)["events_identical"], true);
    assert_eq!(compare_midi(b"invalid", &bytes)["comparable"], false);
}

#[test]
fn reference_preview_uses_source_order_and_effects_and_excerpts_use_preroll() {
    use tunemend::{render::render_midi, review::RenderLayout, soundfont::reference_soundfont};
    let bytes = source(false);
    let full = RenderLayout::Full { loop_passes: 1 };
    let font = reference_soundfont();
    let a = render_midi(&bytes, &full, None, &font, 16000).unwrap();
    assert!(a.left.iter().any(|s| s.abs() > 0.001));
    let excerpt = render_midi(
        &bytes,
        &RenderLayout::Excerpt {
            start: 30,
            end: 240,
        },
        None,
        &font,
        16000,
    )
    .unwrap();
    assert_eq!(excerpt.left[..500], a.left[500..1000]);
    // Program changes latch at note-on, unlike a bend that can update the
    // sounding voice before the first sample. This makes order observable.
    let mut parsed = Smf::parse(&bytes).unwrap();
    parsed.tracks[0].insert(
        8,
        TrackEvent {
            delta: u28::new(0),
            kind: midi(MidiMessage::ProgramChange {
                program: u7::new(80),
            }),
        },
    );
    let mut after = Vec::new();
    parsed.write_std(&mut after).unwrap();
    let after = render_midi(&after, &full, None, &font, 16000).unwrap();
    parsed.tracks[0].swap(7, 8);
    let mut before = Vec::new();
    parsed.write_std(&mut before).unwrap();
    let before = render_midi(&before, &full, None, &font, 16000).unwrap();
    assert!(
        after.left != before.left,
        "same-tick program order must reach the synthesizer"
    );
    let imported = read_smf_preserving(&bytes).unwrap();
    let mut dry = imported.composition.clone();
    for lane in &mut dry.control_lanes {
        if matches!(lane.kind, ControlKind::Reverb | ControlKind::Chorus) {
            lane.points[0].value = 0;
        }
    }
    let dry = write_preserving(&original(&bytes), imported.timebase, &dry, &plan(&bytes)).unwrap();
    let c = render_midi(&dry, &full, None, &font, 16000).unwrap();
    assert!(a.left != c.left, "effect sends must affect PCM");
    assert!(render_midi(&source(true), &full, None, &font, 16000).is_err());
}

#[test]
fn service_edits_are_atomic_and_source_survives_bundle_resume() {
    let root = std::env::temp_dir().join(format!("tunemend-preservation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let bytes = source(true);
    std::fs::write(root.join("original.mid"), &bytes).unwrap();
    let mut service = Service::new(&root);
    let inspect = service.call("analyze_midi", json!({"path": "original.mid"}));
    assert_eq!(inspect.status, Status::Ok);
    assert!(!root.join(".tunemend").exists());
    let stale = service.call("import_midi", json!({"job_id": "stale", "project": "song", "path": "original.mid", "expected_sha256": "wrong"}));
    assert_eq!(stale.status, Status::Blocked);
    let imported = service.call("import_midi", json!({"job_id": "import", "project": "song", "path": "original.mid", "expected_sha256": inspect.result["source_sha256"]}));
    assert_eq!(imported.status, Status::Ok, "{imported:?}");
    let blocked = service.call("edit_project", json!({"project": "song", "request": {"job_id": "move", "base_revision": "r1", "operations": [{"op": "add_voice", "voice": {"id": "extra", "role": "harmony"}}]}}));
    assert_eq!(blocked.status, Status::Blocked);
    let head = service.call(
        "inspect_project",
        json!({"project": "song", "view": "summary"}),
    );
    assert_eq!(head.result["head_revision"], "r1");
    let edited = service.call("edit_project", json!({"project": "song", "request": {"job_id": "edit", "base_revision": "r1", "operations": [{"op": "update_notes", "notes": [{"id": "n1", "pitch": 62}]}]}}));
    assert_eq!(edited.status, Status::Ok, "{edited:?}");
    // DAW-style operations take the same source-backed edit and readback path.
    let shaped = service.call("edit_project", json!({"project": "song", "request": {"job_id": "shape", "base_revision": "r2", "operations": [{"op": "scale_velocities", "ids": ["n1"], "percent": 50}, {"op": "humanize_notes", "ids": ["n1"], "seed": 3, "velocity": 4}]}}));
    assert_eq!(shaped.status, Status::Ok, "{shaped:?}");
    let exported = service.call(
        "export_project",
        json!({"job_id": "export", "project": "song", "destination": "out/delivery"}),
    );
    assert_eq!(exported.status, Status::Ok, "{exported:?}");
    let target = service.call("convert_for_target", json!({"job_id": "target", "project": "song", "converter": "missing", "destination": "out/chip", "mapping": {"frames_per_second": 60, "voices": [], "patches": {}}}));
    assert_eq!(target.status, Status::Blocked, "{target:?}");
    assert_eq!(target.diagnostics[0].location, "/midi_source");
    assert!(!root.join("out/chip").exists());
    let mut resumed = Service::new(root.join("resumed"));
    let bundle = root.join("out/delivery/song.project.json");
    let restored = resumed.call(
        "import_project",
        json!({"job_id": "resume", "path": bundle}),
    );
    assert_eq!(restored.status, Status::Ok, "{restored:?}");
    let again = resumed.call(
        "export_project",
        json!({"job_id": "again", "project": "song", "destination": "out/again"}),
    );
    assert_eq!(again.status, Status::Ok, "{again:?}");
    assert_eq!(
        std::fs::read(root.join("out/delivery/song.mid")).unwrap(),
        std::fs::read(root.join("resumed/out/again/song.mid")).unwrap()
    );
    let mut tampered: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle).unwrap()).unwrap();
    tampered["project"]["midi_source"]["sha256"] = json!("wrong");
    let tampered: tunemend::bundle::ProjectBundle = serde_json::from_value(tampered).unwrap();
    assert!(!tunemend::bundle::check_project_bundle(&tampered).is_empty());
    drop(resumed);
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn earlier_imports_recover_original_objects_and_refuse_missing_evidence() {
    let root = std::env::temp_dir().join(format!("tunemend-earlier-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let bytes = source(false);
    std::fs::write(root.join("original.mid"), &bytes).unwrap();
    {
        let mut service = Service::new(&root);
        assert_eq!(
            service
                .call(
                    "import_midi",
                    json!({"job_id": "import", "project": "song", "path": "original.mid"})
                )
                .status,
            Status::Ok
        );
    }
    let db = rusqlite::Connection::open(root.join(".tunemend/store.sqlite3")).unwrap();
    db.execute(
        "UPDATE projects SET info = json_remove(info, '$.midi_source')",
        [],
    )
    .unwrap();
    {
        let mut service = Service::new(&root);
        let exported = service.call(
            "export_project",
            json!({"job_id": "export", "project": "song", "destination": "out/old"}),
        );
        assert_eq!(exported.status, Status::Ok, "{exported:?}");
        assert_eq!(std::fs::read(root.join("out/old/song.mid")).unwrap(), bytes);
    }
    db.execute(
        "UPDATE projects SET settings = json_set(settings, '$.source.sha256', ?1)",
        ["0".repeat(64)],
    )
    .unwrap();
    let mut service = Service::new(&root);
    let failed = service.call(
        "export_project",
        json!({"job_id": "missing", "project": "song", "destination": "out/missing"}),
    );
    assert_eq!(failed.status, Status::Error);
    assert!(!root.join("out/missing").exists());
    drop(service);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn newly_composed_target_mapping_with_losses_does_not_run_converter() {
    let root = std::env::temp_dir().join(format!("tunemend-target-loss-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Exists, but is deliberately not executable: reaching execution fails.
    std::fs::write(root.join("converter"), "not an executable").unwrap();
    let imported = read_smf_preserving(&source(false)).unwrap();
    let mut service = Service::new(&root);
    let created = service.call("create_project", json!({"job_id": "create", "project": "new", "timebase": imported.timebase, "composition": imported.composition}));
    assert_eq!(created.status, Status::Ok);
    let response = service.call("convert_for_target", json!({"job_id": "convert", "project": "new", "converter": "converter", "destination": "out/target", "mapping": {
        "frames_per_second": 60, "quantize": true, "patches": {"voice": {"chip": "ym2612"}},
        "voices": [{"voice": "track0-ch3", "chip": "ym2612", "patch": "voice", "pan": 192}]
    }}));
    assert_eq!(response.status, Status::Blocked, "{response:?}");
    assert_eq!(response.diagnostics[0].location, "/mapping");
    assert!(!response.result["losses"].as_array().unwrap().is_empty());
    assert!(!root.join("out/target").exists());
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn single_port_metadata_and_initial_gm_resets_preserve_edits_and_preview() {
    use tunemend::{render::render_midi, review::RenderLayout, soundfont::reference_soundfont};
    let base = source(false);
    let mut smf = Smf::parse(&base).unwrap();
    let prefix = [
        TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(9))),
        TrackEventKind::Meta(MetaMessage::MidiChannel(u4::new(2))),
        TrackEventKind::Meta(MetaMessage::SmpteOffset(
            midly::SmpteTime::new(1, 2, 3, 4, 0, midly::Fps::Fps25).unwrap(),
        )),
        TrackEventKind::SysEx(b"\x7e\x7f\x09\x01\xf7"),
        TrackEventKind::SysEx(b"\x7e\x7f\x09\x03\xf7"),
    ];
    smf.tracks[0].splice(
        0..0,
        prefix.into_iter().map(|kind| TrackEvent {
            delta: u28::new(0),
            kind,
        }),
    );
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    let mut imported = read_smf_preserving(&bytes).unwrap();
    assert_eq!(
        write_preserving(
            &original(&bytes),
            imported.timebase,
            &imported.composition,
            &plan(&bytes)
        )
        .unwrap(),
        bytes
    );
    let layout = RenderLayout::Full { loop_passes: 1 };
    let font = reference_soundfont();
    let expected = render_midi(&base, &layout, None, &font, 16000).unwrap();
    let actual = render_midi(&bytes, &layout, None, &font, 16000).unwrap();
    assert_eq!(actual.left, expected.left);
    assert_eq!(actual.right, expected.right);
    imported.composition.notes[0].velocity = 73;
    let edited = write_preserving(
        &original(&bytes),
        imported.timebase,
        &imported.composition,
        &plan(&bytes),
    )
    .unwrap();
    assert_eq!(
        events(&edited)[..prefix.len()],
        events(&bytes)[..prefix.len()]
    );
    assert_eq!(
        read_smf_preserving(&edited).unwrap().composition.notes[0].velocity,
        73
    );
    // A second receiver may reuse the channel; it must not be merged into one.
    let mut multi = Smf::parse(&bytes).unwrap();
    let mut second = multi.tracks[0].clone();
    second[0].kind = TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(10)));
    multi.header.format = Format::Parallel;
    multi.tracks.push(second);
    let mut conflicting = Vec::new();
    multi.write_std(&mut conflicting).unwrap();
    let imported = read_smf_preserving(&conflicting).unwrap();
    assert_eq!(
        imported
            .assignments
            .iter()
            .map(|a| a.port)
            .collect::<Vec<_>>(),
        vec![Some(9), Some(10)]
    );
    assert!(tunemend::midi::preview_blockers(&conflicting).is_empty());
}

#[test]
fn terminal_controls_survive_changes_to_the_same_editable_lane() {
    let base = source(false);
    let mut smf = Smf::parse(&base).unwrap();
    let end = smf.tracks[0].last_mut().unwrap();
    end.delta = u28::new(0);
    let index = smf.tracks[0].len() - 1;
    smf.tracks[0].insert(
        index,
        TrackEvent {
            delta: u28::new(120),
            kind: cc(7, 0),
        },
    );
    smf.tracks[0].insert(
        3,
        TrackEvent {
            delta: u28::new(0),
            kind: cc(7, 100),
        },
    );
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    let mut imported = read_smf_preserving(&bytes).unwrap();
    let lane = imported
        .composition
        .control_lanes
        .iter_mut()
        .find(|l| l.kind == ControlKind::Volume)
        .unwrap();
    assert_eq!(lane.points.len(), 1);
    lane.points[0].value = 80;
    let output = write_preserving(
        &original(&bytes),
        imported.timebase,
        &imported.composition,
        &plan(&bytes),
    )
    .unwrap();
    assert!(events(&output).contains(&(240, cc(7, 0))));
    assert!(events(&output).contains(&(0, cc(7, 80))));
    assert_eq!(
        read_smf_preserving(&output).unwrap().composition,
        imported.composition
    );
}

#[test]
fn terminal_tempo_survives_tempo_edits_without_extending_the_song() {
    let base = source(false);
    let mut smf = Smf::parse(&base).unwrap();
    let end = smf.tracks[0].last_mut().unwrap();
    let delta = end.delta;
    end.delta = u28::new(0);
    let index = smf.tracks[0].len() - 1;
    smf.tracks[0].insert(
        index,
        TrackEvent {
            delta,
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(666_666))),
        },
    );
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    let mut imported = read_smf_preserving(&bytes).unwrap();
    assert_eq!(imported.composition.playback.end, 240);
    assert_eq!(imported.composition.tempo_map.len(), 1);
    assert_eq!(
        write_preserving(
            &original(&bytes),
            imported.timebase,
            &imported.composition,
            &plan(&bytes)
        )
        .unwrap(),
        bytes
    );
    imported.composition.tempo_map[0].microseconds_per_quarter = 300_000;
    let output = write_preserving(
        &original(&bytes),
        imported.timebase,
        &imported.composition,
        &plan(&bytes),
    )
    .unwrap();
    assert!(events(&output).contains(&(
        240,
        TrackEventKind::Meta(MetaMessage::Tempo(u24::new(666_666)))
    )));
    assert!(events(&output).contains(&(
        0,
        TrackEventKind::Meta(MetaMessage::Tempo(u24::new(300_000)))
    )));
    assert_eq!(
        read_smf_preserving(&output).unwrap().composition,
        imported.composition
    );
}

#[test]
fn channel_pressure_renders_but_a_reset_after_channel_setup_still_refuses() {
    use tunemend::{render::render_midi, review::RenderLayout, soundfont::reference_soundfont};
    let base = source(false);
    let mut smf = Smf::parse(&base).unwrap();
    smf.tracks[0].insert(
        7,
        TrackEvent {
            delta: u28::new(0),
            kind: midi(MidiMessage::ChannelAftertouch { vel: u7::new(127) }),
        },
    );
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    assert!(tunemend::midi::preview_blockers(&bytes).is_empty());
    let audio = render_midi(
        &bytes,
        &RenderLayout::Full { loop_passes: 1 },
        None,
        &reference_soundfont(),
        16000,
    )
    .unwrap();
    assert!(audio.left.iter().any(|v| v.abs() > 0.001));
    smf.tracks[0].insert(
        8,
        TrackEvent {
            delta: u28::new(0),
            kind: TrackEventKind::SysEx(b"\x7e\x7f\x09\x01\xf7"),
        },
    );
    bytes.clear();
    smf.write_std(&mut bytes).unwrap();
    assert_eq!(tunemend::midi::preview_blockers(&bytes).len(), 1);
    assert!(
        render_midi(
            &bytes,
            &RenderLayout::Full { loop_passes: 1 },
            None,
            &reference_soundfont(),
            16000
        )
        .is_err()
    );
}

#[test]
fn ordered_tempos_within_a_track_keep_prior_events_when_the_final_value_changes() {
    let base = source(false);
    let mut smf = Smf::parse(&base).unwrap();
    smf.tracks[0].insert(
        3,
        TrackEvent {
            delta: u28::new(0),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(400_000))),
        },
    );
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    let mut imported = read_smf_preserving(&bytes).unwrap();
    assert_eq!(
        imported.composition.tempo_map[0].microseconds_per_quarter,
        400_000
    );
    imported.composition.tempo_map[0].microseconds_per_quarter = 300_000;
    let output = write_preserving(
        &original(&bytes),
        imported.timebase,
        &imported.composition,
        &plan(&bytes),
    )
    .unwrap();
    let tempos: Vec<_> = events(&output)
        .into_iter()
        .filter_map(|(at, e)| match e {
            TrackEventKind::Meta(MetaMessage::Tempo(t)) => Some((at, t.as_int())),
            _ => None,
        })
        .collect();
    assert_eq!(tempos, vec![(0, 500_000), (0, 300_000)]);
    assert_eq!(
        read_smf_preserving(&output).unwrap().composition,
        imported.composition
    );
    // Unlike same-track ordering, conflicting tracks do not state precedence.
    smf.header.format = Format::Parallel;
    smf.tracks.push(vec![TrackEvent {
        delta: u28::new(0),
        kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(600_000))),
    }]);
    bytes.clear();
    smf.write_std(&mut bytes).unwrap();
    assert!(read_smf_preserving(&bytes).is_err());
}
