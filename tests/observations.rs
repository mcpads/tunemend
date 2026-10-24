use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tunemend::service::{Service, Status, ToolResponse};

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}
fn location(r: &ToolResponse) -> &str {
    assert_eq!(r.status, Status::Blocked, "{r:?}");
    &r.diagnostics[0].location
}
fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-obs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn portable_receipts_refuse_mismatches_require_accepted_limitations_and_survive_restart() {
    let root = workspace("portable");
    std::fs::write(root.join("bank.bin"), [9, 8, 7]).unwrap();
    std::fs::create_dir_all(root.join("obs/events")).unwrap();
    std::fs::write(root.join("obs/events/segment-000.ndjson"), b"{\"seq\":0}\n").unwrap();
    std::fs::write(root.join("obs/aram.bin"), [1, 2, 3, 4]).unwrap();
    std::fs::write(root.join("obs/note.txt"), b"undescribed").unwrap();
    let mut s = Service::new(&root);
    let source = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id":"src","path":"bank.bin"}),
    )["source"]
        .clone();
    let receipt = json!({
        "producer": {"name":"emucap","system":"snes","backend":"mesen2","build":"b1","vendor_extra":{"kept":true}},
        "content": {"rom_sha1":"aa"},
        "capability": {"revision":"cap1"},
        "clock": {"domain":"snes.dsp_sample","start":0,"end":512},
        "completion": {"status":"interrupted","dropped":0},
        "members": [
            {"path":"events/segment-000.ndjson","role":"events","sha256":sha(b"{\"seq\":0}\n")},
            {"path":"aram.bin","role":"memory","sha256":sha(&[1, 2, 3, 4])}
        ],
        "future_field": [1, 2]
    });
    let subjects =
        json!([{"kind":"sound_source","id":source,"relation":"donor bank observed in ARAM"}]);
    let base = json!({"job_id":"a1","directory":"obs","receipt":receipt,"subjects":subjects,
        "expected":{"system":"snes","content":{"rom_sha1":"aa"},"producer_build":"b2","capability_revision":"cap1"}});

    // Hard mismatches refuse and leave nothing attached.
    let mut wrong = base.clone();
    wrong["job_id"] = json!("system");
    wrong["expected"]["system"] = json!("md");
    assert_eq!(
        location(&s.call("attach_observation", wrong)),
        "/expected/system"
    );
    let mut wrong = base.clone();
    wrong["job_id"] = json!("content");
    wrong["expected"]["content"]["rom_sha1"] = json!("bb");
    assert_eq!(
        location(&s.call("attach_observation", wrong)),
        "/expected/content/rom_sha1"
    );
    let mut wrong = base.clone();
    wrong["job_id"] = json!("hash");
    wrong["receipt"]["members"][1]["sha256"] = json!(sha(b"other"));
    assert_eq!(
        location(&s.call("attach_observation", wrong)),
        "/receipt/members/1/sha256"
    );
    let mut wrong = base.clone();
    wrong["job_id"] = json!("subject");
    wrong["subjects"][0]["id"] = json!("missing");
    assert_eq!(location(&s.call("attach_observation", wrong)), "/subjects");

    // Incomplete capture, stale build and undescribed files are named, not hidden.
    let blocked = s.call("attach_observation", base.clone());
    assert_eq!(location(&blocked), "/accepted_limitations");
    for kind in [
        "incomplete_capture",
        "stale_producer_build",
        "undescribed_members",
    ] {
        assert!(blocked.summary.contains(kind), "{}", blocked.summary);
    }
    assert!(!blocked.summary.contains("capability_unrecorded"));
    let mut accepted = base.clone();
    accepted["job_id"] = json!("a2");
    accepted["accepted_limitations"] = json!([
        "undescribed_members",
        "incomplete_capture",
        "stale_producer_build"
    ]);
    let attached = call(&mut s, "attach_observation", accepted.clone());
    let id = attached["observation"].as_str().unwrap().to_owned();
    assert_eq!(attached["files"], 3);
    // Defaults spelled out are the same request.
    accepted["producer_format"] = json!("portable");
    assert!(s.call("attach_observation", accepted).replayed);

    // A receipt without capability or clock reports both.
    let mut bare = base.clone();
    bare["job_id"] = json!("bare");
    bare["expected"] = json!({});
    bare["receipt"]
        .as_object_mut()
        .unwrap()
        .remove("capability");
    bare["receipt"].as_object_mut().unwrap().remove("clock");
    let blocked = s.call("attach_observation", bare);
    for kind in [
        "capability_unrecorded",
        "clock_unrecorded",
        "content_unverified",
    ] {
        assert!(blocked.summary.contains(kind), "{}", blocked.summary);
    }
    // The refusal names the receipt's own content keys to compare against.
    assert!(
        blocked.summary.contains("receipt has [\"rom_sha1\"]"),
        "{}",
        blocked.summary
    );

    // Restart without the original folder: identities verify, unknown fields survive.
    std::fs::remove_dir_all(root.join("obs")).unwrap();
    drop(s);
    let mut s = Service::new(&root);
    let summary = call(&mut s, "inspect_observation", json!({"observation":id}));
    assert_eq!(summary["receipt"]["future_field"], json!([1, 2]));
    assert_eq!(summary["receipt"]["producer"]["vendor_extra"]["kept"], true);
    let listed = call(
        &mut s,
        "inspect_observation",
        json!({"view":"observations","subject":source}),
    );
    assert_eq!(listed["total"], 1);
    let listed = call(
        &mut s,
        "inspect_observation",
        json!({"view":"observations","subject":"other"}),
    );
    assert_eq!(listed["total"], 0);
    let bytes = call(
        &mut s,
        "inspect_observation",
        json!({"observation":id,"view":"bytes","member":"aram.bin","offset":1,"limit":2}),
    );
    assert_eq!(bytes["bytes"], json!([2, 3]));
    call(
        &mut s,
        "export_observation",
        json!({"job_id":"x","observation":id,"destination":"out"}),
    );
    assert_eq!(
        std::fs::read(root.join("out/members/aram.bin")).unwrap(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        std::fs::read(root.join("out/members/events/segment-000.ndjson")).unwrap(),
        b"{\"seq\":0}\n"
    );
    assert_eq!(
        std::fs::read(root.join("out/members/note.txt")).unwrap(),
        b"undescribed"
    );

    // The associated original is untouched and still exports exactly.
    call(
        &mut s,
        "export_instrument_source",
        json!({"job_id":"orig","source":source,"destination":"orig"}),
    );
    assert_eq!(
        std::fs::read(root.join("orig/original.bin")).unwrap(),
        [9, 8, 7]
    );
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn links_are_refused_before_anything_is_retained() {
    let root = workspace("links");
    std::fs::create_dir_all(root.join("obs")).unwrap();
    std::fs::write(root.join("secret"), b"x").unwrap();
    std::os::unix::fs::symlink(root.join("secret"), root.join("obs/link")).unwrap();
    let mut s = Service::new(&root);
    let r = s.call(
        "attach_observation",
        json!({"job_id":"l","directory":"obs","receipt":{}}),
    );
    assert_eq!(location(&r), "/directory");
    let r = s.call(
        "attach_observation",
        json!({"job_id":"p","directory":"../obs","receipt":{}}),
    );
    assert_eq!(location(&r), "/directory");
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

fn wav(frames: u32, rate: u32) -> Vec<u8> {
    let payload = frames * 4;
    let mut w = Vec::new();
    w.extend(b"RIFF");
    w.extend((payload + 36).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(2u16.to_le_bytes());
    w.extend(rate.to_le_bytes());
    w.extend((rate * 4).to_le_bytes());
    w.extend(4u16.to_le_bytes());
    w.extend(16u16.to_le_bytes());
    w.extend(b"data");
    w.extend(payload.to_le_bytes());
    w.extend((0..payload).map(|i| i as u8));
    w
}

#[test]
fn emucap_audio_folders_derive_a_receipt_checked_against_the_wav() {
    let root = workspace("emucap");
    std::fs::create_dir_all(root.join("cap")).unwrap();
    let audio = wav(8, 32000);
    let manifest = json!({
        "status":"completed","state":"frozen","sink_outcome":"stopped",
        "producer":{"system":"snes","adapter":"mesen2","build":"3a685361","launch_id":"L1","host_build":"h"},
        "content":{"rom_sha1":"cc"},
        "clock":{"domain":"snes.dsp_sample","epoch":1,"start":100,"end":108,"start_generation":3,"end_generation":3},
        "audio":{"source":"snes.dsp","file":"audio.wav","format":"pcm_s16le","channels":2,"sample_rate":32000,
            "sample_frames":8,"payload_bytes":32,"payload_sha256":sha(&audio[44..]),"file_sha256":sha(&audio)}
    });
    std::fs::write(root.join("cap/audio.wav"), &audio).unwrap();
    std::fs::write(
        root.join("cap/manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut s = Service::new(&root);
    let args = json!({"job_id":"e","directory":"cap","producer_format":"emucap_audio",
        "expected":{"system":"snes","content":{"rom_sha1":"cc"},"producer_build":"3a685361"}});
    let blocked = s.call("attach_observation", args.clone());
    // The audio manifest carries no capability revision; nothing else is uncertain.
    assert!(
        blocked.summary.contains("[\"capability_unrecorded\"]"),
        "{}",
        blocked.summary
    );
    let mut accepted = args;
    accepted["job_id"] = json!("e2");
    accepted["accepted_limitations"] = json!(["capability_unrecorded"]);
    let attached = call(&mut s, "attach_observation", accepted);
    assert_eq!(attached["receipt"]["completion"]["status"], "complete");
    assert_eq!(attached["receipt"]["clock"]["end"], 108);
    assert_eq!(attached["receipt"]["producer"]["backend"], "mesen2");

    // A WAV that disagrees with its manifest is refused.
    let mut short = audio.clone();
    short.truncate(60);
    std::fs::write(root.join("cap/audio.wav"), short).unwrap();
    let r = s.call(
        "attach_observation",
        json!({"job_id":"bad","directory":"cap","producer_format":"emucap_audio","accepted_limitations":["capability_unrecorded","content_unverified"]}),
    );
    assert_eq!(location(&r), "/producer_format");
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}
