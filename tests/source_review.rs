//! Source-only renders reach the listening flow without a project revision:
//! kept records, presentations, feedback located on the source clock, and a
//! bundle section that a fresh workspace imports and resumes.
use midly::num::{u4, u7, u15, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use serde_json::{Value, json};
use std::path::Path;
use tunemend::{
    render::locate_source_sample,
    review::RenderLayout,
    service::{RenderPreviewInput, Service, Status},
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
fn on(channel: u8, key: u8) -> Kind {
    midi(
        channel,
        MidiMessage::NoteOn {
            key: u7::new(key),
            vel: u7::new(100),
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
fn sustain(channel: u8, value: u8) -> Kind {
    midi(
        channel,
        MidiMessage::Controller {
            controller: u7::new(64),
            value: u7::new(value),
        },
    )
}
fn end() -> Kind {
    TrackEventKind::Meta(MetaMessage::EndOfTrack)
}
fn port(value: u8) -> Kind {
    TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(value)))
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

/// At 120 BPM and 16000 Hz a tick lasts 16000/960 samples; tick 480 is
/// sample 8000 and the timeline ends at tick 960, sample 16000.
fn song() -> Vec<u8> {
    encode(vec![
        vec![event(0, on(0, 60)), event(960, off(0, 60)), event(0, end())],
        vec![
            event(0, sustain(1, 127)),
            event(240, on(1, 64)),
            event(120, off(1, 64)),
            event(240, on(1, 60)),
            event(300, off(1, 60)),
            event(60, end()),
        ],
    ])
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

fn held(position: &Value, key: &str) -> Vec<(String, String)> {
    position[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            (
                n["event"].as_str().unwrap().to_owned(),
                n["held_by"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn source_renders_are_reviewed_delivered_and_resumed_without_a_revision() {
    let root = workspace("source-review");
    std::fs::write(root.join("song.mid"), song()).unwrap();
    let mut service = Service::new(&root);
    let source = ok(
        &mut service,
        "load_midi",
        json!({"job_id":"load","path":"song.mid"}),
    )["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let full = json!({"kind":"full","loop_passes":1});
    // Keeping a render for review changes no exported file.
    let plain = ok(
        &mut service,
        "render_preview",
        json!({"job_id":"plain","source":source,"destination":"plain","sample_rate":16000,"layout":full}),
    );
    assert!(plain.get("source_render").is_none());
    let kept = ok(
        &mut service,
        "render_preview",
        json!({"job_id":"kept","source":source,"destination":"kept","sample_rate":16000,"layout":full,"keep_for_review":true}),
    );
    assert_eq!(kept["source_render"]["id"], "source-render-1");
    assert_eq!(kept["source_render"]["content_samples"], 16000);
    for file in ["original.bin", "preview.wav", "render.json"] {
        assert_eq!(
            std::fs::read(root.join("plain").join(file)).unwrap(),
            std::fs::read(root.join("kept").join(file)).unwrap(),
            "{file}"
        );
    }
    let excerpt = ok(
        &mut service,
        "render_preview",
        json!({"job_id":"excerpt","source":source,"destination":"excerpt","sample_rate":16000,
            "layout":{"kind":"excerpt","start":300,"end":700},"keep_for_review":true}),
    );
    assert_eq!(excerpt["source_render"]["id"], "source-render-2");

    // Presentations and feedback belong to the source.
    let both = service.call(
        "present_review",
        json!({"job_id":"both","project":"x","source":source,
        "renders":[{"render":"source-render-1","label":"Original"}]}),
    );
    assert_eq!(both.status, Status::Blocked);
    let unknown = service.call(
        "present_review",
        json!({"job_id":"unknown","source":source,
        "renders":[{"render":"render-1","label":"Original"}]}),
    );
    assert_eq!(unknown.diagnostics[0].location, "/renders/0/render");
    let presented = ok(
        &mut service,
        "present_review",
        json!({"job_id":"present","source":source,"title":"Original",
            "renders":[{"render":"source-render-1","label":"Whole"},{"render":"source-render-2","label":"Excerpt"}]}),
    );
    assert_eq!(presented["presentation"]["id"], "review-1");
    let voiced = service.call(
        "record_feedback",
        json!({"job_id":"voiced","source":source,"presentation":"review-1",
        "marks":[{"render":"source-render-1","start_sample":8000,"voice":"track0-ch1"}]}),
    );
    assert_eq!(voiced.status, Status::Blocked);
    assert_eq!(voiced.diagnostics[0].location, "/marks/0/voice");
    ok(
        &mut service,
        "record_feedback",
        json!({"job_id":"feedback","source":source,"presentation":"review-1","text":"the held chord is muddy",
            "marks":[{"render":"source-render-1","start_sample":8000,"end_sample":16500,"label":"A"},
                     {"render":"source-render-2","start_sample":0,"label":"B"}]}),
    );
    // The listening screen addresses the source by its owner path segment.
    let owner = format!("source:{source}");
    let page = service.review_page(&owner, "review-1").unwrap();
    assert_eq!(page["items"][0]["voices"], json!([]));
    assert_eq!(
        page["items"][1]["length_samples"],
        excerpt["source_render"]["length_samples"]
    );
    let draft = service.save_draft(
        &owner,
        "review-1",
        serde_json::from_value(json!([{"render":"source-render-2","start_sample":10,"label":"C"}]))
            .unwrap(),
        None,
    );
    assert_eq!(draft.status, Status::Ok, "{draft:?}");
    let delivered = service.deliver_draft(&owner, "review-1", "screen-1");
    assert_eq!(delivered.result["feedback"]["id"], "feedback-2");

    let read = ok(&mut service, "read_feedback", json!({"source":source}));
    let marks = &read["feedback"][0]["marks"];
    assert!(marks[0].get("revision").is_none());
    assert_eq!(marks[0]["source"], source.as_str());
    let start = &marks[0]["start"];
    assert_eq!(start["source_tick"], 480);
    assert_eq!(start["source_tick_exact"], "480/1");
    assert_eq!(start["held"][1]["channel"], 1);
    assert_eq!(start["held"][1]["midi_channel"], 2);
    assert_eq!(
        held(start, "held"),
        vec![
            ("t0:e0".to_owned(), "key".to_owned()),
            ("t1:e1".to_owned(), "sustain".to_owned())
        ]
    );
    let tail = &marks[0]["end"];
    assert_eq!(tail["in_tail"], true);
    assert_eq!(
        held(tail, "held_at_end"),
        vec![
            ("t1:e1".to_owned(), "sustain".to_owned()),
            ("t1:e3".to_owned(), "sustain".to_owned())
        ]
    );
    // An excerpt's sample 0 is the excerpt start on the source clock.
    assert_eq!(marks[1]["start"]["source_tick"], 300);
    assert_eq!(read["feedback"][1]["marks"][0]["start"]["source_tick"], 300);

    // A project imported from the same source carries the reviews in its
    // bundle; a project without them keeps its earlier bundle form.
    ok(
        &mut service,
        "import_midi",
        json!({"job_id":"import","project":"song","source":source}),
    );
    let exported = ok(
        &mut service,
        "export_project",
        json!({"job_id":"export","project":"song","destination":"delivery"}),
    );
    assert_eq!(
        exported["source_reviews"]["renders"],
        json!(["source-render-1", "source-render-2"])
    );
    let bundle: Value =
        serde_json::from_slice(&std::fs::read(root.join("delivery/song.project.json")).unwrap())
            .unwrap();
    assert_eq!(bundle["source_reviews"]["source"], source.as_str());
    assert!(bundle["renders"].is_null(), "no project render was made");
    assert_eq!(
        std::fs::read(root.join("delivery/source-renders/source-render-1.wav")).unwrap(),
        std::fs::read(root.join("kept/preview.wav")).unwrap()
    );
    std::fs::write(
        root.join("other.mid"),
        encode(vec![vec![
            event(0, on(0, 67)),
            event(480, off(0, 67)),
            event(0, end()),
        ]]),
    )
    .unwrap();
    ok(
        &mut service,
        "import_midi",
        json!({"job_id":"import-other","project":"other","path":"other.mid"}),
    );
    ok(
        &mut service,
        "export_project",
        json!({"job_id":"export-other","project":"other","destination":"other-delivery"}),
    );
    let other: Value = serde_json::from_slice(
        &std::fs::read(root.join("other-delivery/other.project.json")).unwrap(),
    )
    .unwrap();
    assert!(other.get("source_reviews").is_none());
    assert!(!root.join("other-delivery/source-renders").exists());

    // Default import in a fresh workspace restores and resumes the review.
    let fresh = workspace("source-review-fresh");
    copy_dir(&root.join("delivery"), &fresh.join("delivery"));
    let mut resumed = Service::new(&fresh);
    let imported = ok(
        &mut resumed,
        "import_project",
        json!({"job_id":"import","path":"delivery/song.project.json"}),
    );
    assert_eq!(imported["source_reviews"]["without_audio"], json!([]));
    assert_eq!(
        ok(&mut resumed, "read_feedback", json!({"source":source}))["feedback"],
        read["feedback"]
    );
    let again = ok(
        &mut resumed,
        "present_review",
        json!({"job_id":"again","source":source,"replaces":"review-1",
            "renders":[{"render":"source-render-1","label":"Whole"}]}),
    );
    assert_eq!(again["presentation"]["id"], "review-2");
    assert!(
        ok(&mut resumed, "inspect_midi", json!({"view":"sources"}))["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["sha256"] == source.as_str())
    );

    // Missing WAVs refuse by default, regenerate with a hash check, or stay
    // records without audio that cannot be presented.
    for (name, policy, expected) in [
        ("refuse", None, Status::Blocked),
        ("regenerate", Some("regenerate"), Status::Ok),
        ("without", Some("without_audio"), Status::Ok),
    ] {
        let fresh = workspace(&format!("source-review-{name}"));
        copy_dir(&root.join("delivery"), &fresh.join("delivery"));
        std::fs::remove_dir_all(fresh.join("delivery/source-renders")).unwrap();
        let mut service = Service::new(&fresh);
        let mut request = json!({"job_id":"import","path":"delivery/song.project.json"});
        if let Some(policy) = policy {
            request["missing_renders"] = json!(policy);
        }
        let response = service.call("import_project", request);
        assert_eq!(response.status, expected, "{name}: {response:?}");
        if name == "without" {
            let refused = service.call(
                "present_review",
                json!({"job_id":"p","source":source,
                "renders":[{"render":"source-render-1","label":"Whole"}]}),
            );
            assert_eq!(json!(refused.diagnostics[0].code), "missing_render_audio");
        }
        if name == "regenerate" {
            ok(
                &mut service,
                "present_review",
                json!({"job_id":"p","source":source,"renders":[{"render":"source-render-1","label":"Whole"}]}),
            );
        }
        std::fs::remove_dir_all(fresh).unwrap();
    }

    // Tampered bundles fail the contract checks at the source review location.
    let mut tampered = bundle.clone();
    tampered["source_reviews"]["feedback"][0]["marks"][0]["voice"] = json!("track0-ch1");
    let fresh_tampered = workspace("source-review-tampered");
    copy_dir(&root.join("delivery"), &fresh_tampered.join("delivery"));
    std::fs::write(
        fresh_tampered.join("delivery/song.project.json"),
        serde_json::to_vec(&tampered).unwrap(),
    )
    .unwrap();
    let refused = Service::new(&fresh_tampered).call(
        "import_project",
        json!({"job_id":"import","path":"delivery/song.project.json"}),
    );
    assert_eq!(refused.status, Status::Blocked);
    assert_eq!(
        refused.diagnostics[0].location,
        "/source_reviews/feedback/0/marks/0/voice"
    );

    // Requests that do not opt in keep their canonical job form.
    let request = json!({"job_id":"r","source":"s","destination":"d","layout":full});
    let canonical =
        serde_json::to_value(serde_json::from_value::<RenderPreviewInput>(request).unwrap())
            .unwrap();
    assert!(canonical.get("keep_for_review").is_none());
    for dir in [root, fresh, fresh_tampered] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn the_listening_screen_serves_and_delivers_a_source_presentation() {
    let root = workspace("source-screen");
    std::fs::write(root.join("song.mid"), song()).unwrap();
    let mut service = Service::new(&root);
    let source = ok(
        &mut service,
        "load_midi",
        json!({"job_id":"load","path":"song.mid"}),
    )["source"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(
        &mut service,
        "render_preview",
        json!({"job_id":"kept","source":source,"destination":"kept","sample_rate":16000,
            "layout":{"kind":"full","loop_passes":1},"keep_for_review":true}),
    );
    ok(
        &mut service,
        "present_review",
        json!({"job_id":"present","source":source,"renders":[{"render":"source-render-1","label":"Whole"}]}),
    );
    let shared = std::sync::Arc::new(std::sync::Mutex::new(service));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let base = runtime
        .block_on(tunemend::review_server::start(shared.clone(), 0))
        .unwrap();
    let address = base.trim_start_matches("http://").to_owned();
    let send = |request: String| -> String {
        let mut stream = std::net::TcpStream::connect(&address).unwrap();
        std::io::Write::write_all(&mut stream, request.as_bytes()).unwrap();
        let mut body = String::new();
        std::io::Read::read_to_string(&mut stream, &mut body).unwrap();
        body
    };
    // The page encodes the owner segment as the browser script does.
    let api = format!("/api/review/source%3A{source}/review-1");
    let page = send(format!(
        "GET {api} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    ));
    assert!(page.contains("\"label\":\"Whole\""), "{page}");
    assert!(send(format!("GET /review/source:{source}/review-1 HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")).contains("Send to AI"));
    let draft = r#"{"marks":[{"render":"source-render-1","start_sample":8000,"label":"A"}],"text":"too dry"}"#;
    let saved = send(format!(
        "PUT {api}/draft HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{draft}",
        draft.len()
    ));
    assert!(saved.starts_with("HTTP/1.1 200"), "{saved}");
    let body = r#"{"delivery_id":"once"}"#;
    let delivered = send(format!(
        "POST {api}/deliver HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    assert!(delivered.contains("feedback-1"), "{delivered}");
    let read = ok(
        &mut shared.lock().unwrap(),
        "read_feedback",
        json!({"source":source}),
    );
    assert_eq!(read["feedback"][0]["text"], "too dry");
    assert_eq!(read["feedback"][0]["marks"][0]["start"]["source_tick"], 480);
    drop(runtime);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_positions_follow_each_port_receiver() {
    // Port 0 releases key 60 at tick 240 while port 1 still holds its key 60.
    let bytes = encode(vec![
        vec![
            event(0, port(0)),
            event(0, on(0, 60)),
            event(240, off(0, 60)),
            event(720, end()),
        ],
        vec![
            event(0, port(1)),
            event(0, on(0, 60)),
            event(960, off(0, 60)),
            event(0, end()),
        ],
    ]);
    let layout = RenderLayout::Full { loop_passes: 1 };
    let at = locate_source_sample(&bytes, None, &layout, 16000, 8000).unwrap();
    assert_eq!(at["source_tick"], 480);
    let notes = at["held"].as_array().unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["event"], "t1:e1");
    assert_eq!(notes[0]["port"], 1);
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
