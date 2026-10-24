//! Bundles that leave render audio out, and imports that render it again.
use serde_json::{Value, json};
use tunemend::service::{Service, Status};

mod common;

fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-bundle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn omitted_render_audio_is_regenerated_and_checked_on_import() {
    let root = workspace("regen");
    let mut s = Service::new(&root);
    let bundle = common::village();
    let head = bundle.revisions.last().unwrap().composition.clone();
    call(
        &mut s,
        "create_project",
        json!({"job_id":"c","project":"village","timebase":bundle.project.timebase,"composition":head}),
    );
    for (job, voices) in [("r1", Value::Null), ("r2", json!(["melody"]))] {
        call(
            &mut s,
            "render_preview",
            json!({"job_id":job,"project":"village","layout":{"kind":"excerpt","start":0,"end":1920},"voices":voices}),
        );
    }
    let exported = call(
        &mut s,
        "export_project",
        json!({"job_id":"e","project":"village","destination":"out/lean","render_audio":"none"}),
    );
    assert_eq!(exported["renders_omitted"], json!(["render-1", "render-2"]));
    assert!(!root.join("out/lean/renders/render-1.wav").exists());
    let full = call(
        &mut s,
        "export_project",
        json!({"job_id":"f","project":"village","destination":"out/full"}),
    );
    assert_eq!(full["renders_omitted"], json!([]));
    drop(s);

    let mut fresh = Service::new(root.join("fresh"));
    std::fs::create_dir_all(root.join("fresh")).unwrap();
    let path = root.join("out/lean/village.project.json");
    let refused = fresh.call("import_project", json!({"job_id":"i","path":path}));
    assert_eq!(refused.status, Status::Blocked);
    assert_eq!(refused.diagnostics[0].location, "/missing_renders");
    assert_eq!(
        serde_json::to_value(refused.diagnostics[0].code).unwrap(),
        "missing_render_audio"
    );
    let imported = call(
        &mut fresh,
        "import_project",
        json!({"job_id":"j","path":path,"missing_renders":"regenerate"}),
    );
    assert_eq!(
        imported["regenerated_renders"],
        json!(["render-1", "render-2"])
    );
    assert_eq!(
        imported["regenerated_checks"][0]["checked"],
        "sha256 equals the recorded render"
    );
    // The regenerated audio equals the original render byte for byte.
    let original = std::fs::read(root.join("out/full/renders/render-2.wav")).unwrap();
    let again = call(
        &mut fresh,
        "export_project",
        json!({"job_id":"k","project":"village","destination":"out/again"}),
    );
    assert_eq!(again["renders_omitted"], json!([]));
    assert_eq!(
        std::fs::read(root.join("fresh/out/again/renders/render-2.wav")).unwrap(),
        original
    );

    // A WAV that differs from its recorded hash is refused.
    let mut other = Service::new(root.join("other"));
    std::fs::create_dir_all(root.join("other")).unwrap();
    std::fs::write(root.join("out/full/renders/render-1.wav"), b"RIFF").unwrap();
    let tampered = other.call(
        "import_project",
        json!({"job_id":"t","path":root.join("out/full/village.project.json")}),
    );
    assert_eq!(tampered.status, Status::Blocked);
    assert_eq!(tampered.diagnostics[0].location, "/path");
    drop((fresh, other));
    std::fs::remove_dir_all(root).unwrap();
}

fn refused(s: &mut Service, tool: &str, args: Value) -> (String, String, String) {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Blocked, "{r:?}");
    let d = &r.diagnostics[0];
    (
        serde_json::to_value(d.code)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned(),
        d.location.clone(),
        d.message.clone(),
    )
}

fn village_with_renders(root: &std::path::Path, renders: &[(&str, Value)]) -> Service {
    let mut s = Service::new(root);
    let bundle = common::village();
    let head = bundle.revisions.last().unwrap().composition.clone();
    call(
        &mut s,
        "create_project",
        json!({"job_id":"c","project":"village","timebase":bundle.project.timebase,"composition":head}),
    );
    for (job, voices) in renders {
        call(
            &mut s,
            "render_preview",
            json!({"job_id":job,"project":"village","layout":{"kind":"excerpt","start":0,"end":1920},"voices":voices}),
        );
    }
    s
}

#[test]
fn chosen_renders_are_regenerated_and_the_rest_kept_without_audio() {
    let root = workspace("selective");
    let mut s = village_with_renders(
        &root,
        &[
            ("r1", Value::Null),
            ("r2", json!(["melody"])),
            ("r3", json!(["bass"])),
        ],
    );
    call(
        &mut s,
        "present_review",
        json!({"job_id":"p","project":"village","renders":[{"render":"render-3","label":"Bass"}]}),
    );
    call(
        &mut s,
        "export_project",
        json!({"job_id":"e","project":"village","destination":"out/lean","render_audio":"none"}),
    );
    drop(s);
    let path = root.join("out/lean/village.project.json");
    let fresh_root = root.join("fresh");
    std::fs::create_dir_all(&fresh_root).unwrap();
    let mut fresh = Service::new(&fresh_root);

    // Combinations and references that cannot be honoured are refused first.
    let (code, location, _) = refused(
        &mut fresh,
        "import_project",
        json!({"job_id":"a","path":path,"missing_renders":"regenerate","regenerate_renders":["render-1"]}),
    );
    assert_eq!(
        (code.as_str(), location.as_str()),
        ("invalid_range", "/regenerate_renders")
    );
    let (code, location, _) = refused(
        &mut fresh,
        "import_project",
        json!({"job_id":"b","path":path,"missing_renders":"without_audio","regenerate_renders":["render-9"]}),
    );
    assert_eq!(
        (code.as_str(), location.as_str()),
        ("unknown_reference", "/regenerate_renders/0")
    );
    let (code, location, message) = refused(
        &mut fresh,
        "import_project",
        json!({"job_id":"d","path":path,"missing_renders":"without_audio","regenerate_renders":["render-2"]}),
    );
    assert_eq!(
        (code.as_str(), location.as_str()),
        ("missing_render_audio", "/regenerate_renders")
    );
    assert!(message.contains("`render-3`"), "{message}");
    // The default refusal with a list counts only the unlisted missing renders.
    let (_, location, message) = refused(
        &mut fresh,
        "import_project",
        json!({"job_id":"f","path":path,"regenerate_renders":["render-3"]}),
    );
    assert_eq!(location, "/missing_renders");
    assert!(
        message.starts_with("2 of 3 render WAVs are missing"),
        "{message}"
    );

    let imported = call(
        &mut fresh,
        "import_project",
        json!({"job_id":"g","path":path,"missing_renders":"without_audio","regenerate_renders":["render-3"]}),
    );
    assert_eq!(imported["renders"], 3);
    assert_eq!(imported["regenerated_renders"], json!(["render-3"]));
    assert_eq!(
        imported["renders_without_audio"],
        json!(["render-1", "render-2"])
    );
    let reviews = call(
        &mut fresh,
        "inspect_project",
        json!({"project":"village","view":"reviews"}),
    );
    let renders = reviews["renders"].as_array().unwrap();
    let audio: Vec<&Value> = renders.iter().map(|r| &r["audio"]).collect();
    assert_eq!(
        audio,
        [&json!("absent"), &json!("absent"), &json!("present")]
    );
    // Only present audio has a path, and that file exists.
    assert!(renders[0].get("path").is_none() && renders[1].get("path").is_none());
    assert!(std::path::Path::new(renders[2]["path"].as_str().unwrap()).is_file());

    // Every tool that needs the audio refuses it with the recovery path.
    let measure = refused(
        &mut fresh,
        "measure_audio",
        json!({"audio":{"project":"village","render":"render-1"}}),
    );
    assert_eq!(
        (measure.0.as_str(), measure.1.as_str()),
        ("missing_render_audio", "/audio/render")
    );
    assert!(measure.2.contains("regenerate_renders"), "{}", measure.2);
    let compare = refused(
        &mut fresh,
        "compare_audio",
        json!({"reference":{"project":"village","render":"render-3"},"candidate":{"project":"village","render":"render-2"}}),
    );
    assert_eq!(compare.1, "/candidate/render");
    let present = refused(
        &mut fresh,
        "present_review",
        json!({"job_id":"p2","project":"village","renders":[{"render":"render-3","label":"A"},{"render":"render-1","label":"B"}]}),
    );
    assert_eq!(
        (present.0.as_str(), present.1.as_str()),
        ("missing_render_audio", "/renders/1/render")
    );
    let export = refused(
        &mut fresh,
        "export_project",
        json!({"job_id":"x","project":"village","destination":"out/all"}),
    );
    assert_eq!(
        (export.0.as_str(), export.1.as_str()),
        ("missing_render_audio", "/render_audio")
    );
    assert!(export.2.contains("render-2"), "{}", export.2);
    let export = refused(
        &mut fresh,
        "export_project",
        json!({"job_id":"y","project":"village","destination":"out/listed","render_audio":"listed","render_ids":["render-2"]}),
    );
    assert_eq!(export.1, "/render_ids");
    // Delivering only the render with audio keeps the others' records and hashes.
    let lean = call(
        &mut fresh,
        "export_project",
        json!({"job_id":"z","project":"village","destination":"out/again","render_audio":"presented"}),
    );
    assert_eq!(lean["renders_omitted"], json!(["render-1", "render-2"]));
    let again: Value = serde_json::from_slice(
        &std::fs::read(fresh_root.join("out/again/village.project.json")).unwrap(),
    )
    .unwrap();
    let original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(again["renders"], original["renders"]);

    // render_batch restores the audio by hash; the record does not change.
    let (code, location, _) = refused(
        &mut fresh,
        "render_batch",
        json!({"job_id":"bad","project":"village","regenerate_renders":["render-1"],"format":"float32"}),
    );
    assert_eq!(
        (code.as_str(), location.as_str()),
        ("invalid_range", "/regenerate_renders")
    );
    let restored = call(
        &mut fresh,
        "render_batch",
        json!({"job_id":"regen","project":"village","regenerate_renders":["render-1","render-3"]}),
    );
    assert_eq!(restored["regenerated"][0]["render"], "render-1");
    assert_eq!(restored["already_present"], json!(["render-3"]));
    assert_eq!(
        restored["regenerated"][0]["sha256"],
        original["renders"][0]["sha256"]
    );
    call(
        &mut fresh,
        "measure_audio",
        json!({"audio":{"project":"village","render":"render-1"}}),
    );
    let reviews = call(
        &mut fresh,
        "inspect_project",
        json!({"project":"village","view":"reviews"}),
    );
    assert_eq!(reviews["renders"][0]["audio"], "present");
    let next = call(
        &mut fresh,
        "render_preview",
        json!({"job_id":"n","project":"village","layout":{"kind":"excerpt","start":0,"end":960}}),
    );
    assert_eq!(next["render"]["id"], "render-4");
    drop(fresh);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn delivered_records_leave_undelivered_renders_out_of_the_bundle() {
    let root = workspace("delivered");
    let mut s = village_with_renders(
        &root,
        &[
            ("r1", Value::Null),
            ("r2", Value::Null),
            ("r3", Value::Null),
        ],
    );
    call(
        &mut s,
        "present_review",
        json!({"job_id":"p","project":"village","renders":[{"render":"render-1","label":"A"}]}),
    );
    // A presented render must be delivered.
    let (code, location, message) = refused(
        &mut s,
        "export_project",
        json!({"job_id":"e1","project":"village","destination":"out/a","render_audio":"listed","render_ids":["render-2"],"render_records":"delivered"}),
    );
    assert_eq!(
        (code.as_str(), location.as_str()),
        ("unknown_reference", "/render_records")
    );
    assert!(message.contains("presentation `review-1`") && message.contains("render-1"));
    let exported = call(
        &mut s,
        "export_project",
        json!({"job_id":"e2","project":"village","destination":"out/b","render_audio":"listed","render_ids":["render-1","render-3"],"render_records":"delivered"}),
    );
    assert_eq!(exported["renders_omitted"], json!(["render-2"]));
    assert!(
        exported["renders_note"]
            .as_str()
            .unwrap()
            .contains("without regeneration")
    );
    let written: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/b/village.project.json")).unwrap())
            .unwrap();
    let ids: Vec<&str> = written["renders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["render-1", "render-3"]);
    drop(s);
    // The default import needs no regeneration, and numbering skips the gap.
    let fresh_root = root.join("fresh");
    std::fs::create_dir_all(&fresh_root).unwrap();
    let mut fresh = Service::new(&fresh_root);
    let imported = call(
        &mut fresh,
        "import_project",
        json!({"job_id":"i","path":root.join("out/b/village.project.json")}),
    );
    assert_eq!(imported["renders"], 2);
    assert_eq!(imported["regenerated_renders"], json!([]));
    let next = call(
        &mut fresh,
        "render_preview",
        json!({"job_id":"n","project":"village","layout":{"kind":"excerpt","start":0,"end":960}}),
    );
    assert_eq!(next["render"]["id"], "render-4");
    drop(fresh);
    std::fs::remove_dir_all(root).unwrap();
}
