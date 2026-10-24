//! End-to-end tool workflows through the shared service, as the CLI and MCP
//! server call it. Each test uses a fresh temporary workspace.

mod common;

use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

struct Workspace {
    path: std::path::PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tunemend-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn service(&self) -> Service {
        Service::new(&self.path)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn ok(response: ToolResponse) -> Value {
    assert_eq!(response.status, Status::Ok, "{response:#?}");
    response.result
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn codes(response: &ToolResponse) -> Vec<String> {
    response
        .diagnostics
        .iter()
        .map(|d| {
            serde_json::to_value(d.code)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

/// Creates the village BGM project from the fixture's first revision.
fn village_project(service: &mut Service) {
    let bundle = common::village();
    ok(service.call(
        "create_project",
        json!({"job_id": "create-village", "project": "village", "timebase": bundle.project.timebase,
               "brief": bundle.project.brief, "composition": bundle.revisions[0].composition}),
    ));
}

fn smooth_request(job: &str, base: &str) -> Value {
    let bundle = common::village();
    let mut request =
        serde_json::to_value(bundle.revisions[1].edit_request.as_ref().unwrap()).unwrap();
    request["job_id"] = json!(job);
    request["base_revision"] = json!(base);
    request.as_object_mut().unwrap().remove("feedback");
    request
}

#[test]
fn capabilities_summarize_by_default_and_return_sections_in_full() {
    let workspace = Workspace::new("capabilities");
    let mut service = workspace.service();
    let summary = ok(service.call("get_capabilities", json!({})));
    let full = ok(service.call("get_capabilities", json!({"detail": "full"})));
    assert!(serde_json::to_vec(&summary).unwrap().len() < 8 * 1024);
    assert_eq!(summary["detail"], "summary");
    assert_eq!(summary["limits"], full["limits"]);
    // Every full section has a summary line, and each returns unchanged.
    let lines = summary["sections"].as_object().unwrap();
    let sections: Vec<&String> = full
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| *k != "version")
        .collect();
    assert_eq!(lines.keys().collect::<Vec<_>>(), sections);
    for name in sections {
        let one = ok(service.call("get_capabilities", json!({"sections": [name]})));
        assert_eq!(one[name.as_str()], full[name.as_str()], "{name}");
        assert_eq!(one["version"], full["version"]);
    }
    for (input, location) in [
        (json!({"sections": ["nope"]}), "/sections/0"),
        (json!({"sections": []}), "/sections"),
        (
            json!({"sections": ["mixer"], "detail": "full"}),
            "/sections",
        ),
    ] {
        let refused = service.call("get_capabilities", input);
        assert_eq!(refused.diagnostics[0].location, location);
    }
}

#[test]
fn off_beat_events_name_the_nearest_following_beat() {
    let workspace = Workspace::new("nearest-beat");
    let mut service = workspace.service();
    village_project(&mut service);
    // PPQ 480 in 4/4: 3826 is bar 2 beat 4 +466 ticks, 14 before bar 3; 1930
    // is 10 after bar 2 beat 1, nearer to that beat.
    let request = json!({"job_id": "off-beat", "base_revision": "r1", "operations": [
        {"op": "add_voice", "voice": {"id": "lead", "role": "lead"}},
        {"op": "add_notes", "notes": [
            {"id": "late", "voice": "lead", "start": 3826, "duration": 10, "pitch": 72, "velocity": 80},
            {"id": "early", "voice": "lead", "start": 1930, "duration": 10, "pitch": 72, "velocity": 80}]},
    ]});
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": request}),
    ));
    let page = ok(service.call(
        "inspect_project",
        json!({"view": "events", "project": "village", "voices": ["lead"]}),
    ));
    let notes = page["notes"].as_array().unwrap();
    assert_eq!(notes[0]["position"], "bar 2 beat 1 +10 ticks");
    assert!(notes[0].get("nearest").is_none());
    assert_eq!(notes[1]["position"], "bar 2 beat 4 +466 ticks");
    assert_eq!(notes[1]["nearest"], "bar 3 beat 1 -14 ticks");
}

#[test]
fn reads_do_not_create_storage() {
    let workspace = Workspace::new("reads");
    let mut service = workspace.service();
    ok(service.call("get_capabilities", json!({})));
    ok(service.call("inspect_project", json!({"view": "projects"})));
    assert!(!workspace.path.join(".tunemend").exists());
    assert!(!workspace.path.join(".retro-midi").exists());
}

#[test]
fn legacy_workspace_resumes_without_moving_saved_work() {
    let workspace = Workspace::new("legacy");
    let legacy = workspace.path.join(".retro-midi");
    std::fs::create_dir(&legacy).unwrap();
    rusqlite::Connection::open(legacy.join("store.sqlite3")).unwrap();
    let mut service = workspace.service();
    village_project(&mut service);
    let rendered = ok(service.call(
        "render_preview",
        json!({"job_id": "legacy-render", "project": "village", "layout": {"kind": "excerpt", "start": 0, "end": 480}}),
    ));
    drop(service);
    let mut resumed = workspace.service();
    let summary = ok(resumed.call(
        "inspect_project",
        json!({"project": "village", "view": "summary"}),
    ));
    assert_eq!(summary["project"], "village");
    let replay = resumed.call(
        "render_preview",
        json!({"job_id": "legacy-render", "project": "village", "layout": {"kind": "excerpt", "start": 0, "end": 480}}),
    );
    assert!(replay.replayed);
    assert_eq!(ok(replay), rendered);
    assert!(!workspace.path.join(".tunemend").exists());
    assert!(legacy.join("store.sqlite3").is_file());
}

#[test]
fn current_store_takes_priority_without_merging_legacy_projects() {
    let workspace = Workspace::new("store-priority");
    village_project(&mut workspace.service());
    let current = workspace.path.join(".tunemend");
    let legacy = workspace.path.join(".retro-midi");
    assert!(current.join("store.sqlite3").is_file());
    std::fs::rename(&current, &legacy).unwrap();
    std::fs::create_dir(&current).unwrap();
    let projects = ok(workspace
        .service()
        .call("inspect_project", json!({"view": "projects"})));
    assert_eq!(projects["projects"], json!([]));
    assert!(legacy.join("store.sqlite3").is_file());
}

#[test]
fn edits_are_atomic_repeatable_and_resume_in_a_new_process() {
    let workspace = Workspace::new("edits");
    let mut service = workspace.service();
    village_project(&mut service);

    let edit = json!({"project": "village", "request": smooth_request("smooth", "r1")});
    let first = service.call("edit_project", edit.clone());
    assert_eq!(first.result["revision"], "r2");
    let again = service.call("edit_project", edit.clone());
    assert!(again.replayed);
    assert_eq!(
        again.result["revision"], "r2",
        "a repeated job does not add a revision"
    );

    // The same request with default fields spelled out is the same job.
    let mut explicit = edit.clone();
    explicit["branch"] = json!(false);
    explicit["request"]["protections"] = json!([{"voice": "melody"}]);
    explicit["request"]["scope"]["timeline"] = json!(false);
    assert!(service.call("edit_project", explicit).replayed);

    let mut changed = edit.clone();
    changed["request"]["rationale"] = json!("different text");
    assert_eq!(
        codes(&service.call("edit_project", changed)),
        ["job_conflict"]
    );

    // A protection violation blocks the whole request, including the valid first operation.
    let mut touches_melody = smooth_request("touch-melody", "r2");
    touches_melody["scope"] = json!({});
    touches_melody["operations"] = json!([
        {"op": "update_notes", "notes": [{"id": "acc-5-1", "pitch": 57}]},
        {"op": "update_notes", "notes": [{"id": "mel-4-1", "pitch": 67}]},
    ]);
    let blocked = service.call(
        "edit_project",
        json!({"project": "village", "request": touches_melody}),
    );
    assert_eq!(codes(&blocked), ["protected_change"]);

    // An old base needs an explicit branch.
    let stale = service.call(
        "edit_project",
        json!({"project": "village", "request": smooth_request("from-r1", "r1")}),
    );
    assert_eq!(codes(&stale), ["stale_revision"]);
    let branch = ok(service.call("edit_project", json!({"project": "village", "request": smooth_request("branch-from-r1", "r1"), "branch": true})));
    assert_eq!(branch["revision"], "r3");
    assert_eq!(branch["parent"], "r1");
    drop(service);

    let mut resumed = workspace.service();
    let history = ok(resumed.call(
        "inspect_project",
        json!({"view": "history", "project": "village"}),
    ));
    let revisions: Vec<&str> = history["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["revision"].as_str().unwrap())
        .collect();
    assert_eq!(revisions, ["r1", "r2", "r3"]);
    assert_eq!(history["head_revision"], "r3");
    let replay = resumed.call("edit_project", edit);
    assert!(replay.replayed && replay.result["revision"] == "r2");
    let summary = ok(resumed.call(
        "inspect_project",
        json!({"view": "summary", "project": "village", "revision": "r1"}),
    ));
    assert_eq!(summary["total_notes"], 67);
}

#[test]
fn constraint_violations_are_located_and_fixed_by_editing() {
    let workspace = Workspace::new("profile");
    let mut service = workspace.service();
    village_project(&mut service);
    let check = |service: &mut Service| {
        ok(service.call(
            "check_project",
            json!({"project": "village", "profile": "test-four-channel"}),
        ))
    };
    assert_eq!(check(&mut service)["report"]["passed"], true);

    // A fourth sounding part exceeds the three notes left for music.
    let pad = json!({"job_id": "add-pad", "base_revision": "r1", "operations": [
        {"op": "add_voice", "voice": {"id": "pad", "role": "pad"}},
        {"op": "add_notes", "notes": [{"id": "pad-1", "voice": "pad", "start": 3840, "duration": 1920, "pitch": 100, "velocity": 50}]},
    ]});
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": pad}),
    ));
    let report = check(&mut service)["report"].clone();
    let rules: Vec<&str> = report["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["rule"].as_str().unwrap())
        .collect();
    assert!(
        rules.contains(&"max_simultaneous_notes") && rules.contains(&"pitch_range"),
        "{report:#}"
    );
    let crowded = &report["violations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["rule"] == "max_simultaneous_notes")
        .unwrap()["notes"];
    assert!(crowded.as_array().unwrap().contains(&json!("pad-1")));
    assert!(
        report["unchecked"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u.as_str().unwrap().contains("occupancy"))
    );

    // The agent's fix: drop the pad note; four voices are still within the voice limit.
    let fix = json!({"job_id": "drop-pad", "base_revision": "r2", "operations": [{"op": "remove_notes", "ids": ["pad-1"]}]});
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": fix}),
    ));
    assert_eq!(check(&mut service)["report"]["passed"], true);
}

#[test]
fn feedback_on_a_second_loop_pass_maps_back_to_the_source() {
    let workspace = Workspace::new("feedback");
    let mut service = workspace.service();
    village_project(&mut service);
    let render = service.call(
        "render_preview",
        json!({"job_id": "render-1", "project": "village", "layout": {"kind": "full", "loop_passes": 2}}),
    );
    let render_id = render.result["render"]["id"].as_str().unwrap().to_owned();
    assert!(std::path::Path::new(&render.artifacts[0].path).is_file());
    assert_eq!(render.result["timing"]["loop_seconds"], 10.4);

    let presented = ok(service.call(
        "present_review",
        json!({"job_id": "present-1", "project": "village", "title": "Village BGM", "renders": [{"render": render_id, "label": "First draft"}]}),
    ));
    let presentation = presented["presentation"]["id"].as_str().unwrap().to_owned();
    assert!(presented["url"].is_null(), "no screen runs in this test");

    // The screen saves a draft, then delivers it; a retried click returns the same delivery.
    let mark =
        serde_json::from_value(json!({"render": render_id, "start_sample": 723_240})).unwrap();
    ok(service.save_draft(
        "village",
        &presentation,
        vec![mark],
        Some("The accompaniment jumps here — especially in the café scene".into()),
    ));
    let delivered = service.deliver_draft("village", &presentation, "click-1");
    assert_eq!(delivered.status, Status::Ok);
    assert!(
        service
            .deliver_draft("village", &presentation, "click-1")
            .replayed
    );
    assert_eq!(
        service
            .deliver_draft("village", &presentation, "click-2")
            .status,
        Status::Blocked,
        "the draft was consumed"
    );

    let feedback = ok(service.call("read_feedback", json!({"project": "village"})));
    let item = &feedback["feedback"][0];
    assert_eq!(
        item["text"],
        "The accompaniment jumps here — especially in the café scene"
    );
    let start = &item["marks"][0]["start"];
    assert_eq!(start["loop_pass"], 2);
    assert_eq!(start["source_position"], "5760");
    assert_eq!(start["position"], "bar 4 beat 1");
    let sounding: Vec<&str> = start["sounding"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert!(
        sounding.contains(&"acc-4-1") && sounding.contains(&"mel-4-1"),
        "{sounding:?}"
    );
    assert_eq!(feedback["next_cursor"], 1);
    assert_eq!(
        ok(service.call(
            "read_feedback",
            json!({"project": "village", "after_sequence": 1})
        ))["feedback"],
        json!([])
    );

    // A fix links the feedback; the stale presentation guard protects newer screens.
    let mut fix = smooth_request("fix-jump", "r1");
    fix["feedback"] = json!(["feedback-1"]);
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": fix}),
    ));
    let stale = service.call("present_review", json!({"job_id": "present-2", "project": "village", "renders": [{"render": render_id, "label": "x"}], "replaces": "review-0"}));
    assert_eq!(codes(&stale), ["stale_revision"]);

    // Conversation feedback without a screen is stored verbatim.
    ok(service.call(
        "record_feedback",
        json!({"job_id": "chat-1", "project": "village", "text": "I would like a slightly slower tempo overall"}),
    ));
    assert_eq!(
        ok(service.call(
            "read_feedback",
            json!({"project": "village", "after_sequence": 1})
        ))["feedback"][0]["text"],
        "I would like a slightly slower tempo overall"
    );
}

#[test]
fn export_writes_midi_bundle_report_and_renders_that_reimport() {
    let workspace = Workspace::new("export");
    let mut service = workspace.service();
    village_project(&mut service);
    ok(service.call("render_preview", json!({"job_id": "render", "project": "village", "layout": {"kind": "full", "loop_passes": 1}})));
    let exported = service.call(
        "export_project",
        json!({"job_id": "export", "project": "village", "destination": "out/village"}),
    );
    assert_eq!(exported.status, Status::Ok, "{exported:#?}");
    let kinds: Vec<&str> = exported.artifacts.iter().map(|a| a.kind.as_str()).collect();
    assert_eq!(kinds, ["midi", "project", "wav", "report"]);
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(workspace.path.join("out/village/report.json")).unwrap(),
    )
    .unwrap();
    assert!(
        report["not_in_midi"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().contains("loopStart"))
    );

    // The exported bundle passes the contract checks and the MIDI imports as a new project.
    let bundle: tunemend::ProjectBundle = serde_json::from_str(
        &std::fs::read_to_string(workspace.path.join("out/village/village.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(tunemend::check_project_bundle(&bundle), vec![]);
    let imported = ok(service.call("import_midi", json!({"job_id": "reimport", "project": "village-again", "path": "out/village/village.mid"})));
    assert_eq!(imported["voices"].as_array().unwrap().len(), 3);

    let again = service.call(
        "export_project",
        json!({"job_id": "export-2", "project": "village", "destination": "out/village"}),
    );
    assert_eq!(
        codes(&again),
        ["duplicate_id"],
        "an export never overwrites earlier files"
    );

    std::fs::write(
        workspace.path.join("broken.mid"),
        b"MThd\x00\x00\x00\x06\x00\x02\x00\x01\x01\xe0",
    )
    .unwrap();
    let blocked = service.call(
        "import_midi",
        json!({"job_id": "broken", "project": "broken", "path": "broken.mid"}),
    );
    assert_eq!(blocked.status, Status::Blocked);
    assert_eq!(
        ok(service.call("inspect_project", json!({"view": "projects"})))["projects"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn effects_export_on_an_explicit_grid_or_block() {
    let workspace = Workspace::new("effect");
    let mut service = workspace.service();
    let bundle = common::chime();
    ok(service.call("create_project", json!({"job_id": "create", "project": "chime", "timebase": {"kind": "effect"}, "composition": bundle.revisions[0].composition})));
    let shorten = json!({"job_id": "shorten", "base_revision": "r1", "operations": [{"op": "update_notes", "notes": [{"id": "rise-1", "duration": 25_000}]}]});
    ok(service.call(
        "edit_project",
        json!({"project": "chime", "request": shorten}),
    ));
    let coarse = service.call("export_project", json!({"job_id": "coarse", "project": "chime", "destination": "out/coarse", "timing": {"ppq": 60, "microseconds_per_quarter": 1_000_000}}));
    assert!(
        codes(&coarse)
            .iter()
            .all(|c| c == "effect_time_not_representable")
            && !codes(&coarse).is_empty()
    );
    assert!(!workspace.path.join("out/coarse").exists());
    ok(service.call(
        "export_project",
        json!({"job_id": "fine", "project": "chime", "destination": "out/fine"}),
    ));
}

#[test]
fn sections_are_duplicated_voices_split_and_changes_compared() {
    let workspace = Workspace::new("arrange");
    let mut service = workspace.service();
    village_project(&mut service);

    // Repeat bars 3–4 of the melody over bars 7–8 after clearing them.
    let bars_7_8: Vec<String> = ["mel-7-1", "mel-7-2", "mel-7-3", "mel-7-4", "mel-8-1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let repeat = json!({"job_id": "repeat-a", "base_revision": "r1", "scope": {"voices": ["melody"], "range": {"start": 11520, "end": 15360}},
    "operations": [
        {"op": "remove_notes", "ids": bars_7_8},
        {"op": "duplicate_range", "source": {"start": 3840, "end": 7680}, "target_start": 11520, "voices": ["melody"]},
    ]});
    let repeated = ok(service.call(
        "edit_project",
        json!({"project": "village", "request": repeat}),
    ));
    assert_eq!(
        repeated["difference"]["added_notes"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    let copy = repeated["difference"]["added_notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "mel-3-1@11520")
        .unwrap();
    assert_eq!(copy["start"], 11520);

    // Copying outside the scope is refused as a whole.
    let outside = json!({"job_id": "repeat-b", "base_revision": "r2", "scope": {"voices": ["melody"], "range": {"start": 11520, "end": 15360}},
        "operations": [{"op": "duplicate_range", "source": {"start": 3840, "end": 5760}, "target_start": 9600, "voices": ["melody"]}]});
    let refused = codes(&service.call(
        "edit_project",
        json!({"project": "village", "request": outside}),
    ));
    assert_eq!(
        refused,
        vec!["out_of_scope"; 4],
        "one diagnostic per copied note"
    );

    // Split the bass's fifths into their own voice.
    let fifths: Vec<Value> = (1..=8)
        .map(|bar| json!({"id": format!("bass-{bar}-2"), "voice": "bass-fifths"}))
        .collect();
    let split = json!({"job_id": "split-bass", "base_revision": "r2", "protections": [{"voice": "melody"}],
        "operations": [{"op": "add_voice", "voice": {"id": "bass-fifths", "role": "bass"}}, {"op": "update_notes", "notes": fifths}]});
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": split}),
    ));
    let summary = ok(service.call(
        "inspect_project",
        json!({"view": "summary", "project": "village"}),
    ));
    let voices: Vec<(String, u64)> = summary["voices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["id"].as_str().unwrap().to_owned(),
                v["notes"].as_u64().unwrap(),
            )
        })
        .collect();
    assert!(
        voices.contains(&("bass".into(), 8)) && voices.contains(&("bass-fifths".into(), 8)),
        "{voices:?}"
    );

    // Quantize a deliberately late note back onto the eighth grid.
    let nudge = json!({"job_id": "nudge", "base_revision": "r3", "operations": [{"op": "shift_notes", "ids": ["acc-2-2"], "time_offset": 30}]});
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": nudge}),
    ));
    let snap = json!({"job_id": "snap", "base_revision": "r4", "operations": [{"op": "quantize_notes", "ids": ["acc-2-2"], "grid": 240}]});
    let snapped = ok(service.call(
        "edit_project",
        json!({"project": "village", "request": snap}),
    ));
    assert_eq!(
        snapped["difference"]["changed_notes"][0]["after"]["start"],
        2400
    );

    // Compare limited to the repeated bars only lists melody changes there.
    let compared = ok(service.call("compare_revisions", json!({"project": "village", "from": "r1", "to": "r5", "range": {"start": 11520, "end": 15360}, "voices": ["melody"]})));
    assert_eq!(compared["added_notes"].as_array().unwrap().len(), 7);
    assert_eq!(compared["removed_notes"].as_array().unwrap().len(), 5);
    assert!(compared["changed_notes"].as_array().unwrap().is_empty());
    let applies = &compared["filter_applies_to"];
    assert!(strings(&applies["voices"]).contains(&"added_notes".to_owned()));
    assert!(strings(&applies["range"]).contains(&"changed_clips".to_owned()));
    assert_eq!(
        applies["never"],
        json!(["added_voices", "removed_voices", "changed_timeline"])
    );
    let unfiltered = ok(service.call(
        "compare_revisions",
        json!({"project": "village", "from": "r1", "to": "r5"}),
    ));
    assert!(unfiltered.get("filter_applies_to").is_none());

    // Paged events keep identifiers and musical positions.
    let page = ok(service.call(
        "inspect_project",
        json!({"view": "events", "project": "village", "voices": ["melody"], "limit": 5}),
    ));
    assert_eq!(page["notes"].as_array().unwrap().len(), 5);
    assert_eq!(page["notes"][0]["position"], "bar 3 beat 1");
    let next = page["next_cursor"].as_u64().unwrap();
    let rest = ok(service.call("inspect_project", json!({"view": "events", "project": "village", "voices": ["melody"], "cursor": next, "limit": 500})));
    assert_eq!(next as usize + rest["notes"].as_array().unwrap().len(), 21);
}

#[test]
fn a_delivered_project_continues_in_another_workspace() {
    let (studio, elsewhere) = (Workspace::new("deliver-from"), Workspace::new("deliver-to"));
    let mut service = studio.service();
    village_project(&mut service);
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": smooth_request("smooth", "r1")}),
    ));
    ok(service.call("render_preview", json!({"job_id": "render", "project": "village", "revision": "r1", "layout": {"kind": "full", "loop_passes": 2}})));
    ok(service.call("present_review", json!({"job_id": "show", "project": "village", "renders": [{"render": "render-1", "label": "First draft"}]})));
    ok(service.call(
        "record_feedback",
        json!({"job_id": "said", "project": "village", "presentation": "review-1",
        "marks": [{"render": "render-1", "start_sample": 723_240}], "text": "The accompaniment jumps here — especially in the café scene"}),
    ));
    ok(service.call(
        "export_project",
        json!({"job_id": "deliver", "project": "village", "destination": "delivery"}),
    ));

    // The person moves the delivery folder into another workspace.
    std::fs::rename(
        studio.path.join("delivery"),
        elsewhere.path.join("received"),
    )
    .unwrap();
    let mut other = elsewhere.service();
    let imported = ok(other.call(
        "import_project",
        json!({"job_id": "open", "path": "received/village.project.json"}),
    ));
    assert_eq!(imported["head_revision"], "r2");
    assert_eq!(imported["renders"], 1);

    let feedback = ok(other.call("read_feedback", json!({"project": "village"})));
    assert_eq!(
        feedback["feedback"][0]["marks"][0]["start"]["source_position"],
        "5760"
    );
    let next = ok(other.call("edit_project", json!({"project": "village", "request": {"job_id": "softer", "base_revision": "r2",
        "scope": {"voices": ["accompaniment"]}, "operations": [{"op": "update_notes", "notes": [{"id": "acc-4-1", "velocity": 50}]}]}})));
    assert_eq!(next["revision"], "r3");
    let reviews = ok(other.call(
        "inspect_project",
        json!({"view": "reviews", "project": "village"}),
    ));
    let audio = reviews["renders"][0]["path"].as_str().unwrap();
    assert!(
        std::path::Path::new(audio).is_file()
            && audio.starts_with(elsewhere.path.to_str().unwrap())
    );
}
