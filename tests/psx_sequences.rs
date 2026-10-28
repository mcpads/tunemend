//! PlayStation SEQ/SEP sequences through load_psx_sequence,
//! inspect_psx_sequence, export_psx_sequence and render_psx_sequence, on
//! original synthetic files: separate and container sources, located
//! refusals with the source kept, exact and MIDI export into load_midi,
//! restart, SEP sequences, VAB binding and bounded SPU audition.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tunemend::service::{Service, Status};

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-psxseq-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}

fn refusal(s: &mut Service, tool: &str, args: Value) -> (String, String, Value) {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Blocked, "{r:?}");
    (
        r.diagnostics[0].location.clone(),
        r.diagnostics[0].message.clone(),
        r.result,
    )
}

fn load(s: &mut Service, root: &Path, name: &str, bytes: &[u8]) -> String {
    std::fs::write(root.join(name), bytes).unwrap();
    call(
        s,
        "load_instrument_source",
        json!({"job_id": format!("load-{name}"), "path": name}),
    )["source"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn header(resolution: u16, tempo: u32) -> Vec<u8> {
    let mut b = b"pQES".to_vec();
    b.extend(1u32.to_be_bytes());
    b.extend(resolution.to_be_bytes());
    b.extend(&tempo.to_be_bytes()[1..]);
    b.extend([4, 2]);
    b
}

/// Program 0, an endless NRPN loop around two notes (one running status),
/// reverb NRPNs, a tempo change without length byte, end of track.
fn song() -> Vec<u8> {
    let mut b = header(480, 500_000);
    b.extend([0x00, 0xC0, 0x00]);
    b.extend([0x00, 0xB0, 99, 16, 0x00, 98, 15, 0x00, 6, 1]); // reverb type
    b.extend([0x00, 99, 20, 0x00, 6, 127]); // loop start, endless
    b.extend([0x00, 0x90, 60, 100, 0x00, 64, 90]); // two notes, running status
    b.extend([0x83, 0x60, 60, 0, 0x00, 64, 0]); // 480 ticks later
    b.extend([0x00, 0xFF, 0x51, 0x07, 0xA1, 0x20]);
    b.extend([0x00, 0xB0, 99, 30]); // loop end
    b.extend([0x00, 0xFF, 0x2F, 0x00]);
    b
}

/// One looped region (near full-scale with `loud`), program 0 with one tone
/// over keys 0..100.
fn bank(loud: bool) -> (Vec<u8>, Vec<u8>) {
    let mut region = vec![0u8; 16];
    region.extend([if loud { 0x00 } else { 0x0A }, 0x04]);
    region.extend([if loud { 0x77 } else { 0x13 }; 14]);
    region.extend([if loud { 0x00 } else { 0x1A }, 0x03]);
    region.extend([if loud { 0x77 } else { 0x31 }; 14]);
    let tone_table = 0x820;
    let mut h = vec![0u8; tone_table + 0x200 + 0x200];
    h[..4].copy_from_slice(b"pBAV");
    h[4..8].copy_from_slice(&7u32.to_le_bytes());
    h[0x12] = 1;
    h[0x14] = 1;
    h[0x16] = 1;
    h[0x18] = 127;
    h[0x19] = 64;
    h[0x20] = 1;
    h[0x21] = 127;
    h[0x24] = 64;
    let t = tone_table;
    h[t + 2] = 127;
    h[t + 3] = 64;
    h[t + 4] = 60;
    h[t + 6] = 0;
    h[t + 7] = 100;
    h[t + 0x0C] = 2;
    h[t + 0x0D] = 2;
    h[t + 0x10..t + 0x12].copy_from_slice(&0x000Fu16.to_le_bytes());
    h[t + 0x12..t + 0x14].copy_from_slice(&0x1FC0u16.to_le_bytes());
    h[t + 0x16..t + 0x18].copy_from_slice(&1u16.to_le_bytes());
    let table = h.len() - 0x200;
    h[table + 2..table + 4].copy_from_slice(&((region.len() / 8) as u16).to_le_bytes());
    let total = (h.len() + region.len()) as u32;
    h[0x0C..0x10].copy_from_slice(&total.to_le_bytes());
    (h, region)
}

fn load_bank(s: &mut Service, root: &Path, loud: bool) -> String {
    let (vh, vb) = bank(loud);
    let h = load(s, root, "bank.vh", &vh);
    let b = load(s, root, "bank.vb", &vb);
    call(
        s,
        "load_vab_bank",
        json!({"job_id": "bank", "header": {"source": h}, "body": {"source": b}}),
    )["bank"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn seq_events_loops_and_container_slices_are_located() {
    let root = workspace("load");
    let mut s = Service::new(&root);
    let bytes = song();
    let src = load(&mut s, &root, "song.seq", &bytes);
    let loaded = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "seq", "sequence": {"source": src}}),
    );
    assert_eq!(loaded["container"], "seq");
    assert_eq!(loaded["sequences"][0]["end"], bytes.len());
    assert_eq!(loaded["trailing_bytes"], 0);
    let id = loaded["sequence"].as_str().unwrap().to_owned();
    let summary = call(&mut s, "inspect_psx_sequence", json!({"sequence": id}));
    let seq = &summary["sequences"][0];
    assert_eq!(seq["fields"]["resolution"]["value"], 480);
    assert_eq!(seq["fields"]["tempo"]["offset"], 0x0A);
    assert_eq!(seq["fields"]["time_signature"]["denominator"], 4);
    assert_eq!(seq["notes"], 2);
    assert_eq!(seq["loops"][0]["count"], 127);
    assert_eq!(seq["loops"][0]["endless"], true);
    assert_eq!(seq["tempo_events"][0]["microseconds_per_quarter"], 500_000);
    let events = call(
        &mut s,
        "inspect_psx_sequence",
        json!({"sequence": id, "view": "events", "limit": 500}),
    );
    let items = events["items"].as_array().unwrap();
    assert_eq!(items[0]["offset"], 15);
    // Events tile the bytes from the score start to the end of track.
    for w in items.windows(2) {
        assert_eq!(
            w[0]["offset"].as_u64().unwrap() + w[0]["length"].as_u64().unwrap(),
            w[1]["offset"].as_u64().unwrap()
        );
    }
    assert_eq!(items[3]["meaning"]["nrpn"], "attribute");
    assert_eq!(items[3]["meaning"]["attribute"], "reverb_type");
    assert_eq!(items[4]["meaning"]["nrpn"], "loop_start");
    assert_eq!(items[5]["meaning"]["nrpn"], "loop_count");
    assert_eq!(items[7]["running_status"], true);
    assert_eq!(items[8]["kind"], "note_off");
    assert_eq!(items[8]["tick"], 480);
    assert_eq!(items[11]["meaning"]["nrpn"], "loop_end");

    // The same bytes inside a container, by offset and length.
    let mut container = vec![0xEEu8; 2048];
    container.extend(&bytes);
    container.extend([0u8; 100]);
    let csrc = load(&mut s, &root, "pack.dat", &container);
    let open = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "slice-open", "sequence": {"source": csrc, "offset": 2048}}),
    );
    assert_eq!(open["trailing_bytes"], 100);
    let exact = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "slice", "sequence": {"source": csrc, "offset": 2048, "length": open["sequences"][0]["end"]}}),
    );
    assert_eq!(exact["sha256"], loaded["sha256"]);
    assert_eq!(exact["trailing_bytes"], 0);

    // A refusal names the sequence-relative and the source offset, and the
    // container stays exact.
    let mut broken = container.clone();
    broken[2048 + 15 + 1] = 0xF0;
    let bsrc = load(&mut s, &root, "broken.dat", &broken);
    let (location, message, result) = refusal(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "broken", "sequence": {"source": bsrc, "offset": 2048}}),
    );
    assert_eq!(location, "/sequence");
    assert!(message.contains("system message"), "{message}");
    assert_eq!(result["offset"], 16);
    assert_eq!(result["source_offset"], 2048 + 16);
    call(
        &mut s,
        "export_instrument_source",
        json!({"job_id": "keep", "source": bsrc, "destination": "kept"}),
    );
    let kept = std::fs::read_dir(root.join("kept"))
        .unwrap()
        .map(|e| std::fs::read(e.unwrap().path()).unwrap())
        .any(|b| b == broken);
    assert!(kept);
    for (patch, expected) in [
        ((0x04usize, 0x02u8), "version bytes"),
        ((0x00, b'X'), "magic"),
    ] {
        let mut b = bytes.clone();
        b[patch.0] = patch.1;
        let src = load(&mut s, &root, &format!("bad-{}.seq", patch.0), &b);
        let (_, message, _) = refusal(
            &mut s,
            "load_psx_sequence",
            json!({"job_id": format!("bad-{}", patch.0), "sequence": {"source": src}}),
        );
        assert!(message.contains(expected), "{message}");
    }
}

#[test]
fn export_is_exact_midi_loads_and_restart_needs_no_original() {
    let root = workspace("export");
    let mut s = Service::new(&root);
    let bytes = song();
    let src = load(&mut s, &root, "song.seq", &bytes);
    let id = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "seq", "sequence": {"source": src}}),
    )["sequence"]
        .as_str()
        .unwrap()
        .to_owned();
    std::fs::remove_file(root.join("song.seq")).unwrap();
    drop(s);
    let mut s = Service::new(&root);
    let exported = call(
        &mut s,
        "export_psx_sequence",
        json!({"job_id": "export", "sequence": id, "destination": "out/song"}),
    );
    assert_eq!(
        std::fs::read(root.join("out/song/sequence.seq")).unwrap(),
        bytes
    );
    let midi = &exported["midi"][0];
    assert_eq!(midi["file"], "sequence-00.mid");
    assert!(
        midi["not_in_midi"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["what"] == "running status")
    );
    let report: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/song/sequence.json")).unwrap())
            .unwrap();
    assert_eq!(
        report["midi"][0]["report"]["readback"]["sequence_events_matched"],
        13
    );
    assert_eq!(report["midi"][0]["report"]["readback"]["markers"], 2);
    let smf = std::fs::read(root.join("out/song/sequence-00.mid")).unwrap();
    assert_eq!(&smf[0..4], b"MThd");
    assert_eq!(u16::from_be_bytes([smf[12], smf[13]]), 480);
    let loaded = call(
        &mut s,
        "load_midi",
        json!({"job_id": "midi", "path": "out/song/sequence-00.mid"}),
    );
    assert_eq!(loaded["format"], 0);
    assert_eq!(loaded["editable_projection"]["preview"]["available"], true);
    let imported = call(
        &mut s,
        "import_midi",
        json!({"job_id":"import-seq", "project":"seq", "path":"out/song/sequence-00.mid"}),
    );
    assert!(imported.is_object());
    // Without markers only the header metas are added.
    call(
        &mut s,
        "export_psx_sequence",
        json!({"job_id": "plain", "sequence": id, "destination": "out/plain", "loop_markers": false}),
    );
    let plain = std::fs::read(root.join("out/plain/sequence-00.mid")).unwrap();
    assert_eq!(
        smf.len() - plain.len(),
        2 * 4 + "loopStart".len() + "loopEnd".len()
    );
    // A retry with the same job ID replays; a different request conflicts.
    let again = s.call(
        "export_psx_sequence",
        json!({"job_id": "export", "sequence": id, "destination": "out/song"}),
    );
    assert!(again.replayed);
    let views = call(&mut s, "inspect_psx_sequence", json!({"view": "sequences"}));
    assert_eq!(views["total"], 1);
}

#[test]
fn sep_sequences_are_separate_midi_files() {
    let root = workspace("sep");
    let mut s = Service::new(&root);
    let mut b = b"pQES".to_vec();
    b.extend([0, 0]);
    for (id, key) in [(0u16, 60u8), (1, 72)] {
        let score = [
            0x00, 0x90, key, 0x40, 0x60, key, 0x00, 0x00, 0xFF, 0x2F, 0x00,
        ];
        b.extend(id.to_be_bytes());
        b.extend(96u16.to_be_bytes());
        b.extend([0x07, 0xA1, 0x20, 3, 2]);
        b.extend((score.len() as u32).to_be_bytes());
        b.extend(score);
    }
    let src = load(&mut s, &root, "set.sep", &b);
    let loaded = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "sep", "sequence": {"source": src}}),
    );
    assert_eq!(loaded["container"], "sep");
    assert_eq!(loaded["sequences"].as_array().unwrap().len(), 2);
    let id = loaded["sequence"].as_str().unwrap();
    let summary = call(&mut s, "inspect_psx_sequence", json!({"sequence": id}));
    assert_eq!(summary["sequences"][1]["fields"]["sequence_id"]["value"], 1);
    assert_eq!(
        summary["sequences"][1]["fields"]["data_size"]["offset"],
        6 + 24 + 9
    );
    let events = call(
        &mut s,
        "inspect_psx_sequence",
        json!({"sequence": id, "view": "events", "index": 1}),
    );
    assert_eq!(events["items"][0]["key"], 72);
    assert_eq!(events["items"][0]["offset"], 6 + 24 + 13);
    call(
        &mut s,
        "export_psx_sequence",
        json!({"job_id": "export", "sequence": id, "destination": "out/set"}),
    );
    assert_eq!(std::fs::read(root.join("out/set/sequence.sep")).unwrap(), b);
    for i in 0..2 {
        let path = format!("out/set/sequence-{i:02}.mid");
        call(
            &mut s,
            "load_midi",
            json!({"job_id": format!("midi-{i}"), "path": path}),
        );
    }
    let (location, _, _) = refusal(
        &mut s,
        "inspect_psx_sequence",
        json!({"sequence": id, "view": "events", "index": 2}),
    );
    assert_eq!(location, "/index");
}

#[test]
fn binding_and_audition_against_a_bank() {
    let root = workspace("render");
    let mut s = Service::new(&root);
    let bank = load_bank(&mut s, &root, false);
    let mut bytes = song();
    // A key outside the tone's 0..100 range, before the loop.
    bytes.splice(18..18, [0x00, 0x90, 110, 0x40, 0x00, 110, 0x00]);
    let src = load(&mut s, &root, "song.seq", &bytes);
    let id = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "seq", "sequence": {"source": src}}),
    )["sequence"]
        .as_str()
        .unwrap()
        .to_owned();
    let binding = call(
        &mut s,
        "inspect_psx_sequence",
        json!({"sequence": id, "view": "binding", "bank": bank}),
    );
    assert_eq!(binding["notes_without_tone"], 1);
    assert_eq!(binding["programs"][0]["keys_without_tone"][0]["key"], 110);

    let once = call(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "once", "sequence": id, "bank": bank, "loop_repeats": 0, "destination": "out/once"}),
    );
    let r = &once["render"];
    assert_eq!(r["notes"], 3);
    assert_eq!(r["voices_started"], 2);
    assert_eq!(r["skipped_notes"]["count"], 1);
    assert_eq!(r["content_samples"], 22050);
    assert_eq!(
        r["reverb_settings_not_applied"].as_array().unwrap().len(),
        1
    );
    assert_eq!(once["wav"]["format"], "float32");
    assert!(root.join("out/once/render.wav").exists());
    call(&mut s, "measure_audio", once["measure_with"].clone());
    // One jump back at the loop end replays the loop section.
    let twice = call(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "twice", "sequence": id, "bank": bank, "loop_repeats": 1}),
    );
    assert_eq!(twice["render"]["notes"], 5);
    assert_eq!(twice["render"]["loop_jumps"].as_array().unwrap().len(), 1);
    // The output bound stops playback.
    let bounded = call(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "bound", "sequence": id, "bank": bank, "loop_repeats": 64, "max_seconds": 3}),
    );
    assert_eq!(bounded["render"]["end_reason"], "max_seconds");
    assert_eq!(bounded["render"]["samples"], 3 * 44100);
    let (location, message, _) = refusal(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "long", "sequence": id, "bank": bank, "max_seconds": 601}),
    );
    assert_eq!(location, "");
    assert!(message.contains("max_seconds"), "{message}");
}

#[test]
fn voice_overflow_and_unmodelled_messages_refuse_at_their_events() {
    let root = workspace("overflow");
    let mut s = Service::new(&root);
    let bank = load_bank(&mut s, &root, false);
    let mut chord = header(480, 500_000);
    for i in 0..25u8 {
        chord.extend([0x00, 0x90, 40 + i, 0x40]);
    }
    chord.extend([0x83, 0x60, 0xFF, 0x2F, 0x00]);
    let src = load(&mut s, &root, "chord.seq", &chord);
    let id = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "chord", "sequence": {"source": src}}),
    )["sequence"]
        .as_str()
        .unwrap()
        .to_owned();
    let (location, message, result) = refusal(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "strict", "sequence": id, "bank": bank}),
    );
    assert_eq!(location, "/sequence");
    assert!(message.contains("24 SPU voices"), "{message}");
    assert_eq!(result["event"], 24);
    assert_eq!(result["offset"], 15 + 24 * 4);
    let dropped = call(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "drop", "sequence": id, "bank": bank, "voice_overflow": "drop"}),
    );
    assert_eq!(dropped["render"]["peak_voices"], 24);
    assert_eq!(dropped["render"]["voice_overflow"]["dropped"], 1);
    let stolen = call(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "steal", "sequence": id, "bank": bank, "voice_overflow": "steal_releasing"}),
    );
    assert_eq!(stolen["render"]["voice_overflow"]["stolen"], 0);
    assert_eq!(stolen["render"]["voice_overflow"]["dropped"], 1);

    for (name, score, event, needle) in [
        (
            "attribute",
            vec![0x00, 0xB0, 99, 0, 0x00, 98, 4, 0x00, 6, 10],
            2usize,
            "attribute",
        ),
        ("bank", vec![0x00, 0xB0, 0, 1], 0, "bank select"),
        ("pressure", vec![0x00, 0xD0, 0x40], 0, "aftertouch"),
    ] {
        let mut b = header(480, 500_000);
        b.extend(&score);
        b.extend([0x00, 0xFF, 0x2F, 0x00]);
        let src = load(&mut s, &root, &format!("{name}.seq"), &b);
        let id = call(
            &mut s,
            "load_psx_sequence",
            json!({"job_id": format!("load-{name}"), "sequence": {"source": src}}),
        )["sequence"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, message, result) = refusal(
            &mut s,
            "render_psx_sequence",
            json!({"job_id": format!("render-{name}"), "sequence": id, "bank": bank}),
        );
        assert!(message.contains(needle), "{message}");
        assert_eq!(result["event"], event);
    }
}

#[test]
fn discovery_points_to_load_psx_sequence_for_files_and_container_members() {
    let root = workspace("discovery");
    let mut s = Service::new(&root);
    let bytes = song();
    std::fs::write(root.join("song.seq"), &bytes).unwrap();
    let loaded = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id": "seq", "path": "song.seq"}),
    );
    let hint = &loaded["discovery"]["formats"][0];
    assert_eq!(hint["format"], "psx_sequence");
    assert_eq!(hint["load_with"], "load_psx_sequence");
    assert_eq!(hint["header"]["container"], "seq");
    assert_eq!(hint["header"]["bytes"], bytes.len());
    let mut container = vec![0xEEu8; 2048];
    container.extend(&bytes);
    container.extend(b"pQES not a sequence");
    container.extend(&bytes);
    std::fs::write(root.join("pack.dat"), &container).unwrap();
    let loaded = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id": "pack", "path": "pack.dat"}),
    );
    let hint = loaded["discovery"]["formats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["format"] == "psx_sequences_inside")
        .cloned()
        .unwrap();
    assert_eq!(hint["count"], 2);
    assert_eq!(hint["first"][0]["offset"], 2048);
    assert_eq!(hint["first"][0]["bytes"], bytes.len());
    assert_eq!(hint["first"][1]["offset"], 2048 + bytes.len() + 19);
    assert_eq!(hint["truncated"], false);
}

#[test]
fn renders_report_levels_warnings_gain_and_overflow_totals() {
    let root = workspace("levels");
    let mut s = Service::new(&root);
    let bank = load_bank(&mut s, &root, true);
    let mut chord = header(480, 500_000);
    for i in 0..100u8 {
        chord.extend([0x00, 0x90, i, 0x7F]);
    }
    chord.extend([0x83, 0x60, 0xFF, 0x2F, 0x00]);
    let src = load(&mut s, &root, "chord.seq", &chord);
    let id = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "chord", "sequence": {"source": src}}),
    )["sequence"]
        .as_str()
        .unwrap()
        .to_owned();
    let loud = call(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "loud", "sequence": id, "bank": bank, "voice_overflow": "drop"}),
    );
    let overflow = &loud["render"]["voice_overflow"];
    assert_eq!(overflow["events"], 76);
    assert_eq!(overflow["dropped"], 76);
    assert_eq!(overflow["by_channel"]["0"]["dropped"], 76);
    assert_eq!(overflow["listed"].as_array().unwrap().len(), 64);
    assert_eq!(overflow["truncated"], true);
    assert_eq!(overflow["first_tick"], 0);
    let over = loud["levels"]["samples_over_full_scale"].as_u64().unwrap();
    assert!(over > 0);
    assert!(loud["levels"]["sample_peak_dbfs"].as_f64().unwrap() > 0.0);
    assert_eq!(loud["warnings"][0]["code"], "samples_over_full_scale");
    assert_eq!(
        loud["warnings"][0]["location"],
        "/levels/samples_over_full_scale"
    );
    assert!(loud.get("gain").is_none());
    let suggested = loud["warnings"][0]["suggested_gain_db"].as_f64().unwrap();
    let quiet = call(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "quiet", "sequence": id, "bank": bank, "voice_overflow": "drop", "gain_db": suggested}),
    );
    assert_eq!(quiet["levels"]["samples_over_full_scale"], 0);
    assert!(quiet.get("warnings").is_none());
    assert_eq!(
        quiet["gain"]["sample_peak_before_gain"],
        loud["levels"]["sample_peak"]
    );
    assert_ne!(quiet["wav"]["sha256"], loud["wav"]["sha256"]);
    let (location, _, _) = refusal(
        &mut s,
        "render_psx_sequence",
        json!({"job_id": "too-loud", "sequence": id, "bank": bank, "gain_db": 13.0}),
    );
    assert_eq!(location, "/gain_db");
}

#[test]
fn binding_candidates_rank_container_banks_by_coverage() {
    let root = workspace("candidates");
    let mut s = Service::new(&root);
    let (wide, _) = bank(false);
    let mut narrow = wide.clone();
    narrow[0x820 + 7] = 10; // key range 0..10
    // Narrow bank first, then the wide one, then the sequence.
    let mut container = narrow.clone();
    container.resize(4096, 0);
    container.extend(&wide);
    container.resize(8192, 0);
    let seq_at = container.len();
    container.extend(song());
    let src = load(&mut s, &root, "pack.vdd", &container);
    let id = call(
        &mut s,
        "load_psx_sequence",
        json!({"job_id": "seq", "sequence": {"source": src, "offset": seq_at}}),
    )["sequence"]
        .as_str()
        .unwrap()
        .to_owned();
    let ranked = call(
        &mut s,
        "inspect_psx_sequence",
        json!({"sequence": id, "view": "binding_candidates", "bank_source": src}),
    );
    assert_eq!(ranked["total"], 2);
    let items = ranked["items"].as_array().unwrap();
    assert_eq!(items[0]["offset"], 4096);
    assert_eq!(items[0]["notes_without_tone"], 0);
    assert_eq!(items[0]["header"]["length"], wide.len());
    assert_eq!(items[1]["offset"], 0);
    assert_eq!(items[1]["notes_without_tone"], 2);
    let (location, _, _) = refusal(
        &mut s,
        "inspect_psx_sequence",
        json!({"sequence": id, "view": "binding_candidates"}),
    );
    assert_eq!(location, "/bank_source");
}
