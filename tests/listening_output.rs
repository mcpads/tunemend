//! Listening outputs: source render gain and full-scale warnings, one sound
//! source name, precise port retention wording and readable delivery names.
mod common;

#[test]
fn every_command_prints_its_help_and_succeeds() {
    let empty = workspace("cli-help");
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_tunemend"))
            .args(["--workspace", empty.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap()
    };
    for args in [
        &["review", "--help"][..],
        &["review", "-h"],
        &["call", "--help"],
        &["call", "render_preview", "--help"],
        &["mcp", "--help"],
        &["schema", "-h"],
        &["tools", "--help"],
        &["help", "review"],
        &["--help"],
    ] {
        let output = run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("USAGE"),
            "{args:?}"
        );
    }
    assert!(!run(&["help", "nope"]).status.success());
    // Help never creates a store in the workspace.
    assert!(std::fs::read_dir(&empty).unwrap().next().is_none());
    std::fs::remove_dir_all(empty).unwrap();
}

use midly::num::{u4, u7, u15, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use serde_json::{Value, json};
use tunemend::service::{RenderPreviewInput, Service, Status};

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
fn encode(tracks: Vec<Vec<TrackEvent<'static>>>) -> Vec<u8> {
    let mut bytes = Vec::new();
    Smf {
        header: Header::new(Format::Parallel, Timing::Metrical(u15::new(480))),
        tracks,
    }
    .write_std(&mut bytes)
    .unwrap();
    bytes
}

/// A dense loud chord on eight channels, far above full scale in the
/// reference render.
fn loud() -> Vec<u8> {
    let mut track = Vec::new();
    for channel in 0..8u8 {
        for key in [48u8, 55, 60, 64, 67, 72] {
            track.push(event(
                0,
                midi(
                    channel,
                    MidiMessage::NoteOn {
                        key: u7::new(key),
                        vel: u7::new(127),
                    },
                ),
            ));
        }
    }
    track.push(event(
        480,
        midi(
            0,
            MidiMessage::NoteOff {
                key: u7::new(48),
                vel: u7::new(64),
            },
        ),
    ));
    track.push(event(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)));
    encode(vec![track])
}

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn ok(service: &mut Service, tool: &str, arguments: Value) -> Value {
    let response = service.call(tool, arguments);
    assert_eq!(response.status, Status::Ok, "{tool}: {response:?}");
    response.result
}

fn samples(path: &std::path::Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    let wav = tunemend::wav::parse(&bytes).unwrap();
    wav.data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|s| f32::from_le_bytes(*s))
        .collect()
}

#[test]
fn source_gain_gives_headroom_without_changing_requests_that_omit_it() {
    let root = workspace("source-gain");
    std::fs::write(root.join("loud.mid"), loud()).unwrap();
    let mut service = Service::new(&root);
    let source = ok(
        &mut service,
        "load_midi",
        json!({"job_id":"load","path":"loud.mid"}),
    )["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let full = json!({"kind":"full","loop_passes":1});
    let plain = ok(
        &mut service,
        "render_preview",
        json!({"job_id":"plain","source":source,"destination":"plain","sample_rate":16000,"layout":full,"keep_for_review":true}),
    );
    let report = &plain["render"];
    assert_eq!(report["sound_source"], "reference-waveforms");
    assert_eq!(
        plain["source_render"]["sound_source"],
        report["sound_source"]
    );
    assert!(report.get("gain_db").is_none());
    assert!(plain["source_render"].get("gain_db").is_none());
    let peak = report["sample_peak"].as_f64().unwrap();
    assert!(peak > 1.0, "the fixture must clip: {peak}");
    let suggested = plain["headroom"]["suggested_gain_db"].as_f64().unwrap();
    assert!(suggested < 0.0);
    // The listening screen would serve clipped samples; present_review says so.
    let presented = ok(
        &mut service,
        "present_review",
        json!({"job_id":"loud","source":source,"renders":[{"render":"source-render-1","label":"As is"}]}),
    );
    assert_eq!(presented["warnings"][0]["render"], "source-render-1");
    assert!(presented["warnings"][0]["samples_over_full_scale"].as_u64() > Some(0));

    let gained = ok(
        &mut service,
        "render_preview",
        json!({"job_id":"gained","source":source,"destination":"gained","sample_rate":16000,"layout":full,
            "keep_for_review":true,"gain_db":suggested}),
    );
    let report = &gained["render"];
    assert_eq!(report["gain_db"], json!(suggested));
    assert_eq!(report["sample_peak_before_gain"], json!(peak));
    assert_eq!(report["samples_over_full_scale"], 0);
    assert!(gained.get("headroom").is_none());
    assert_eq!(gained["source_render"]["gain_db"], json!(suggested));
    let factor = 10f64.powf(suggested / 20.0) as f32;
    let before = samples(&root.join("plain/preview.wav"));
    let after = samples(&root.join("gained/preview.wav"));
    assert_eq!(before.len(), after.len());
    assert!(before.iter().zip(&after).all(|(b, a)| b * factor == *a));
    assert_eq!(
        std::fs::read(root.join("plain/original.bin")).unwrap(),
        std::fs::read(root.join("gained/original.bin")).unwrap()
    );
    let quiet = ok(
        &mut service,
        "present_review",
        json!({"job_id":"quiet","source":source,"renders":[{"render":"source-render-2","label":"With headroom"}]}),
    );
    assert!(quiet.get("warnings").is_none());

    for (arguments, location) in [
        (
            json!({"job_id":"too-loud","source":source,"destination":"x","layout":full,"gain_db":12.1}),
            "/gain_db",
        ),
        (
            json!({"job_id":"too-fine","source":source,"destination":"y","layout":full,"gain_db":-1.25}),
            "",
        ),
    ] {
        let refused = service.call("render_preview", arguments);
        assert_eq!(refused.status, Status::Blocked);
        assert_eq!(refused.diagnostics[0].location, location);
    }
    let bundle = common::village();
    ok(
        &mut service,
        "create_project",
        json!({"job_id":"create","project":"village","timebase":bundle.project.timebase,
            "composition":bundle.revisions[0].composition}),
    );
    let refused = service.call(
        "render_preview",
        json!({"job_id":"project-gain","project":"village","layout":full,"gain_db":-3.0}),
    );
    assert_eq!(refused.diagnostics[0].location, "/gain_db");

    // Regeneration in a fresh workspace repeats the gain and its hash.
    ok(
        &mut service,
        "import_midi",
        json!({"job_id":"import","project":"loud","source":source}),
    );
    ok(
        &mut service,
        "export_project",
        json!({"job_id":"export","project":"loud","destination":"delivery"}),
    );
    let fresh = workspace("source-gain-fresh");
    std::fs::create_dir_all(fresh.join("delivery")).unwrap();
    for file in ["loud.project.json", "loud.mid"] {
        std::fs::copy(
            root.join("delivery").join(file),
            fresh.join("delivery").join(file),
        )
        .unwrap();
    }
    let imported = Service::new(&fresh).call(
        "import_project",
        json!({"job_id":"import","path":"delivery/loud.project.json","missing_renders":"regenerate"}),
    );
    assert_eq!(imported.status, Status::Ok, "{imported:?}");

    let request = json!({"job_id":"r","source":"s","destination":"d","layout":full});
    let canonical =
        serde_json::to_value(serde_json::from_value::<RenderPreviewInput>(request).unwrap())
            .unwrap();
    assert!(canonical.get("gain_db").is_none());
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(fresh).unwrap();
}

#[test]
fn spoken_times_beyond_a_render_are_refused_with_its_length() {
    let root = workspace("mark-length");
    std::fs::write(root.join("loud.mid"), loud()).unwrap();
    let mut service = Service::new(&root);
    let source = ok(
        &mut service,
        "load_midi",
        json!({"job_id":"load","path":"loud.mid"}),
    )["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let kept = ok(
        &mut service,
        "render_preview",
        json!({"job_id":"kept","source":source,"destination":"kept","sample_rate":16000,
            "layout":{"kind":"full","loop_passes":1},"keep_for_review":true}),
    );
    let length = kept["source_render"]["length_samples"].as_u64().unwrap();
    let refused = service.call(
        "record_feedback",
        json!({"job_id":"late","source":source,"text":"at 0:05 it clicks",
            "marks":[{"render":"source-render-1","start_sample":80000}]}),
    );
    assert_eq!(refused.status, Status::Blocked);
    let message = &refused.diagnostics[0].message;
    assert!(message.contains(&format!("{length} samples")), "{message}");
    assert!(
        message.contains(&format!("{:.3} s", length as f64 / 16000.0)),
        "{message}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_declared_but_unused_port_is_described_precisely() {
    let note = |delta, on: bool| {
        event(
            delta,
            midi(
                0,
                if on {
                    MidiMessage::NoteOn {
                        key: u7::new(60),
                        vel: u7::new(100),
                    }
                } else {
                    MidiMessage::NoteOff {
                        key: u7::new(60),
                        vel: u7::new(64),
                    }
                },
            ),
        )
    };
    let port = |p| event(0, TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(p))));
    let end = || event(0, TrackEventKind::Meta(MetaMessage::EndOfTrack));
    let bytes = encode(vec![
        vec![port(0), note(0, true), note(480, false), end()],
        vec![port(1), end()],
    ]);
    let analysis = tunemend::midi::analyze_smf(&bytes);
    let retained: Vec<&Value> = analysis["editing"]["retained"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["message"].as_str().unwrap().contains("MIDI Port"))
        .collect();
    assert_eq!(retained.len(), 2);
    assert_eq!(
        retained[1]["message"],
        "MIDI Port event retained unchanged; the file declares ports 0 and 1 and only port 0 carries channel and SysEx events, the receiver the projection uses"
    );
    assert_eq!(retained[1]["location"], "/tracks/1/events/0");
    let single = encode(vec![vec![port(0), note(0, true), note(480, false), end()]]);
    assert_eq!(
        tunemend::midi::analyze_smf(&single)["editing"]["retained"][0]["message"],
        "MIDI Port event retained unchanged; the file declares port 0 and every channel and SysEx event uses port 0"
    );
}

#[test]
fn a_resumed_project_keeps_its_original_as_a_loaded_source() {
    let root = workspace("resume-original");
    std::fs::write(root.join("loud.mid"), loud()).unwrap();
    let mut service = Service::new(&root);
    let source = ok(
        &mut service,
        "load_midi",
        json!({"job_id":"load","path":"loud.mid"}),
    )["source"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(
        &mut service,
        "import_midi",
        json!({"job_id":"import","project":"loud","source":source}),
    );
    ok(
        &mut service,
        "export_project",
        json!({"job_id":"export","project":"loud","destination":"delivery"}),
    );
    let fresh = workspace("resume-original-fresh");
    std::fs::create_dir_all(fresh.join("delivery")).unwrap();
    for file in ["loud.project.json", "loud.mid"] {
        std::fs::copy(
            root.join("delivery").join(file),
            fresh.join("delivery").join(file),
        )
        .unwrap();
    }
    let mut resumed = Service::new(&fresh);
    let imported = ok(
        &mut resumed,
        "import_project",
        json!({"job_id":"import","path":"delivery/loud.project.json"}),
    );
    assert_eq!(imported["midi_source"]["source"], source.as_str());
    ok(
        &mut resumed,
        "export_midi_source",
        json!({"job_id":"original","source":source,"destination":"original.mid"}),
    );
    assert_eq!(std::fs::read(fresh.join("original.mid")).unwrap(), loud());
    let sources = ok(&mut resumed, "inspect_midi", json!({"view":"sources"}));
    assert_eq!(sources["sources"][0]["sha256"], source.as_str());
    // A second import of the same original keeps the one catalog record.
    ok(
        &mut resumed,
        "load_midi",
        json!({"job_id":"reload","path":"original.mid"}),
    );
    assert_eq!(
        ok(&mut resumed, "inspect_midi", json!({"view":"sources"}))["sources"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(fresh).unwrap();
}

#[test]
fn a_source_without_timed_content_says_so_when_loaded() {
    let root = workspace("untimed");
    let port = |p| event(0, TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(p))));
    let end = || event(0, TrackEventKind::Meta(MetaMessage::EndOfTrack));
    let untimed = encode((0..4).map(|p| vec![port(p), end()]).collect());
    std::fs::write(root.join("untimed.mid"), &untimed).unwrap();
    std::fs::write(root.join("loud.mid"), loud()).unwrap();
    let mut service = Service::new(&root);
    let loaded = ok(
        &mut service,
        "load_midi",
        json!({"job_id":"load","path":"untimed.mid"}),
    );
    assert_eq!(loaded["timed_content"], false);
    assert!(
        loaded["timed_content_note"]
            .as_str()
            .unwrap()
            .contains("tick 0")
    );
    assert_eq!(
        loaded["editable_projection"]["preview"]["timed_content"],
        false
    );
    let summary = ok(
        &mut service,
        "inspect_midi",
        json!({"view":"summary","source":loaded["source"]}),
    );
    assert_eq!(summary["timed_content"], false);
    let refused = service.call(
        "render_preview",
        json!({"job_id":"r","source":loaded["source"],
        "destination":"r","layout":{"kind":"full","loop_passes":1}}),
    );
    assert!(refused.summary.contains("no timed content"), "{refused:?}");
    let timed = ok(
        &mut service,
        "load_midi",
        json!({"job_id":"load-loud","path":"loud.mid"}),
    );
    assert_eq!(timed["timed_content"], true);
    assert!(timed.get("timed_content_note").is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_lead_in_delays_a_source_render_and_marks_still_find_source_ticks() {
    let root = workspace("lead-in");
    std::fs::write(root.join("loud.mid"), loud()).unwrap();
    let mut service = Service::new(&root);
    let source = ok(
        &mut service,
        "load_midi",
        json!({"job_id":"load","path":"loud.mid"}),
    )["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let full = json!({"kind":"full","loop_passes":1});
    ok(
        &mut service,
        "render_preview",
        json!({"job_id":"plain","source":source,"destination":"plain","sample_rate":16000,"layout":full,"keep_for_review":true}),
    );
    let delayed = ok(
        &mut service,
        "render_preview",
        json!({"job_id":"delayed","source":source,"destination":"delayed","sample_rate":16000,"layout":full,
            "keep_for_review":true,"lead_in_ms":250,"gain_db":-12.0}),
    );
    assert_eq!(delayed["render"]["lead_in_samples"], 4000);
    assert_eq!(delayed["source_render"]["lead_in_samples"], 4000);
    let plain = samples(&root.join("plain/preview.wav"));
    let after = samples(&root.join("delayed/preview.wav"));
    assert!(after[..8000].iter().all(|s| *s == 0.0));
    let factor = 10f64.powf(-12.0 / 20.0) as f32;
    assert!(
        plain
            .iter()
            .zip(&after[8000..])
            .all(|(p, a)| p * factor == *a)
    );
    assert_eq!(
        delayed["source_render"]["length_samples"].as_u64().unwrap(),
        plain.len() as u64 / 2 + 4000
    );
    ok(
        &mut service,
        "record_feedback",
        json!({"job_id":"marks","source":source,"text":"compare",
            "marks":[{"render":"source-render-1","start_sample":4000},
                     {"render":"source-render-2","start_sample":8000},
                     {"render":"source-render-2","start_sample":100}]}),
    );
    let marks =
        &ok(&mut service, "read_feedback", json!({"source":source}))["feedback"][0]["marks"];
    assert_eq!(
        marks[0]["start"]["source_tick"],
        marks[1]["start"]["source_tick"]
    );
    assert_eq!(marks[0]["start"]["held"], marks[1]["start"]["held"]);
    assert_eq!(marks[1]["start"]["render_seconds"], 0.5);
    assert_eq!(marks[2]["start"]["in_lead_in"], true);
    for (arguments, location) in [
        (
            json!({"job_id":"long","source":source,"destination":"x","layout":full,"lead_in_ms":10001}),
            "/lead_in_ms",
        ),
        (
            json!({"job_id":"zero","source":source,"destination":"y","layout":full,"lead_in_ms":0}),
            "/lead_in_ms",
        ),
    ] {
        assert_eq!(
            service.call("render_preview", arguments).diagnostics[0].location,
            location
        );
    }
    // Regeneration repeats the lead-in and gain and matches the hash.
    ok(
        &mut service,
        "import_midi",
        json!({"job_id":"import","project":"loud","source":source}),
    );
    ok(
        &mut service,
        "export_project",
        json!({"job_id":"export","project":"loud","destination":"delivery"}),
    );
    let fresh = workspace("lead-in-fresh");
    std::fs::create_dir_all(fresh.join("delivery")).unwrap();
    std::fs::copy(
        root.join("delivery/loud.project.json"),
        fresh.join("delivery/loud.project.json"),
    )
    .unwrap();
    let imported = Service::new(&fresh).call(
        "import_project",
        json!({"job_id":"import","path":"delivery/loud.project.json","missing_renders":"regenerate"}),
    );
    assert_eq!(imported.status, Status::Ok, "{imported:?}");
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(fresh).unwrap();
}

#[test]
fn reference_waveforms_carry_no_dc_offset() {
    let font = tunemend::soundfont::reference_soundfont();
    // Programs 80, 0, 32 and 40 select the square, 25% pulse, triangle and saw.
    for program in [80u8, 0, 32, 40] {
        let bytes = encode(vec![vec![
            event(
                0,
                midi(
                    0,
                    MidiMessage::ProgramChange {
                        program: u7::new(program),
                    },
                ),
            ),
            event(
                0,
                midi(
                    0,
                    MidiMessage::NoteOn {
                        key: u7::new(57),
                        vel: u7::new(100),
                    },
                ),
            ),
            event(
                1920,
                midi(
                    0,
                    MidiMessage::NoteOff {
                        key: u7::new(57),
                        vel: u7::new(64),
                    },
                ),
            ),
            event(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)),
        ]]);
        let audio = tunemend::render::render_midi_source(
            &bytes,
            &tunemend::review::RenderLayout::Full { loop_passes: 1 },
            &font,
            44_100,
        )
        .unwrap();
        // 220 Hz held for two seconds: average whole cycles of the sustain.
        let window = &audio.left[22_050..22_050 + 200 * 44_100 / 220];
        let mean = window.iter().map(|s| f64::from(*s)).sum::<f64>() / window.len() as f64;
        let peak = window.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(
            mean.abs() < 2e-3 * f64::from(peak),
            "program {program}: mean {mean}, peak {peak}"
        );
    }
}

#[test]
fn a_cut_tail_is_part_of_the_delivery_name() {
    let root = workspace("tail-names");
    let mut service = Service::new(&root);
    let bundle = common::village();
    ok(
        &mut service,
        "create_project",
        json!({"job_id":"create","project":"village","timebase":bundle.project.timebase,
            "composition":bundle.revisions[0].composition}),
    );
    ok(
        &mut service,
        "render_batch",
        json!({"job_id":"cut","project":"village","tail":"cut","sample_rate":16000}),
    );
    let exported = ok(
        &mut service,
        "export_project",
        json!({"job_id":"export","project":"village","destination":"out","named_copies":true}),
    );
    let paths: Vec<String> = exported["named_copies"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().replace('\\', "/"))
        .collect();
    assert_eq!(paths, vec!["mix/full-tail-cut.wav"]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn colliding_delivery_names_say_revision_and_role() {
    let root = workspace("delivery-names");
    let mut service = Service::new(&root);
    let bundle = common::village();
    ok(
        &mut service,
        "create_project",
        json!({"job_id":"create","project":"village","timebase":bundle.project.timebase,
            "composition":bundle.revisions[0].composition}),
    );
    let full = json!({"kind":"full","loop_passes":1});
    ok(
        &mut service,
        "render_preview",
        json!({"job_id":"r1","project":"village","layout":full,"sample_rate":16000}),
    );
    ok(
        &mut service,
        "edit_project",
        json!({"project":"village","request":{"job_id":"quieter","base_revision":"r1","scope":{"buses":true},
            "operations":[{"op":"set_mix","target":{"kind":"master"},"volume_db":-6.0}]}}),
    );
    for (job, rate) in [("r2-a", 16000), ("r2-b", 22050)] {
        ok(
            &mut service,
            "render_preview",
            json!({"job_id":job,"project":"village","layout":full,"sample_rate":rate}),
        );
    }
    let exported = ok(
        &mut service,
        "export_project",
        json!({"job_id":"export","project":"village","destination":"out","named_copies":true}),
    );
    let mut names: Vec<(String, String, String, bool)> = exported["named_copies"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().replace('\\', "/"),
                f["role"].as_str().unwrap().to_owned(),
                f["revision"].as_str().unwrap().to_owned(),
                f["delivered_revision"].as_bool().unwrap(),
            )
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            (
                "mix/full-r1.wav".into(),
                "mixdown".into(),
                "r1".into(),
                false
            ),
            (
                "mix/full-r2-render-2.wav".into(),
                "mixdown".into(),
                "r2".into(),
                true
            ),
            (
                "mix/full-r2-render-3.wav".into(),
                "mixdown".into(),
                "r2".into(),
                true
            ),
        ]
    );
    std::fs::remove_dir_all(root).unwrap();
}
