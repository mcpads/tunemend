//! PlayStation VAB banks through load_vab_bank, inspect_vab_bank,
//! replace_vab_sample, export_vab_bank and render_vab_tone, on original
//! synthetic banks: separate and combined packing, located refusals,
//! replacement from a sample plan and a donor bank, and SPU audition.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tunemend::service::{Service, Status};

const TONE_TABLE: usize = 0x820;

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-vab-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}

fn refusal(s: &mut Service, tool: &str, args: Value) -> (String, String) {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Blocked, "{r:?}");
    (
        r.diagnostics[0].location.clone(),
        r.diagnostics[0].message.clone(),
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

fn block(header: u8, flags: u8, fill: u8) -> Vec<u8> {
    let mut b = vec![header, flags];
    b.extend((0..14).map(|i| fill.wrapping_add(i * 17)));
    b
}

/// Zero block, two data blocks, end_mute, then the unplayed after-end block.
fn one_shot_region() -> Vec<u8> {
    let mut r = vec![0u8; 16];
    r.extend(block(0x08, 0, 3));
    r.extend(block(0x19, 0, 40));
    r.extend(block(0x29, 1, 90));
    r.extend([0x00, 0x07]);
    r.extend([0x77u8; 14]);
    r
}

/// Zero block, attack, loop start block, two loop blocks ending in repeat.
fn loop_region() -> Vec<u8> {
    let mut r = vec![0u8; 16];
    r.extend(block(0x07, 0, 5));
    r.extend(block(0x08, 4, 11));
    r.extend(block(0x18, 0, 23));
    r.extend(block(0x28, 3, 61));
    r
}

/// Programs 0 (two tones) and 5 (one tone): VAG 1 one-shot, VAG 2 looping.
fn bank(regions: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
    let mut h = vec![0u8; TONE_TABLE + 2 * 0x200 + 0x200];
    h[..4].copy_from_slice(b"pBAV");
    h[4..8].copy_from_slice(&7u32.to_le_bytes());
    h[0x10..0x12].copy_from_slice(&0xEEEEu16.to_le_bytes());
    h[0x12..0x14].copy_from_slice(&2u16.to_le_bytes());
    h[0x14..0x16].copy_from_slice(&3u16.to_le_bytes());
    h[0x16..0x18].copy_from_slice(&(regions.len() as u16).to_le_bytes());
    h[0x18] = 0x7F;
    h[0x19] = 0x40;
    h[0x1C..0x20].fill(0xFF);
    for p in 0..128 {
        let at = 0x20 + p * 16;
        h[at + 1] = 0x7F;
        h[at + 2] = 0xFF;
        h[at + 8..at + 16].fill(0xFF);
    }
    h[0x20] = 2;
    h[0x20 + 5 * 16] = 1;
    for block in 0..2 {
        for slot in 0..16 {
            let at = TONE_TABLE + block * 0x200 + slot * 0x20;
            h[at + 14] = 0xB1;
            h[at + 15] = 0xB2;
            h[at + 0x10..at + 0x12].copy_from_slice(&0x80FFu16.to_le_bytes());
            h[at + 0x12..at + 0x14].copy_from_slice(&0x5FC0u16.to_le_bytes());
            h[at + 0x18..at + 0x20].copy_from_slice(&[0xC0, 0, 0xC1, 0, 0xC2, 0, 0xC3, 0]);
        }
    }
    let tone = |h: &mut Vec<u8>, block: usize, slot: usize, program: u16, vag: u16, center: u8| {
        let at = TONE_TABLE + block * 0x200 + slot * 0x20;
        h[at + 2] = 100;
        h[at + 3] = 64;
        h[at + 4] = center;
        h[at + 7] = 127;
        h[at + 0x10..at + 0x12].copy_from_slice(&0x00FFu16.to_le_bytes());
        h[at + 0x12..at + 0x14].copy_from_slice(&0xDFE0u16.to_le_bytes());
        h[at + 0x14..at + 0x16].copy_from_slice(&program.to_le_bytes());
        h[at + 0x16..at + 0x18].copy_from_slice(&vag.to_le_bytes());
    };
    tone(&mut h, 0, 0, 0, 1, 60);
    tone(&mut h, 0, 1, 0, 2, 72);
    tone(&mut h, 1, 0, 5, 2, 60);
    let table = h.len() - 0x200;
    for (i, r) in regions.iter().enumerate() {
        let units = (r.len() / 8) as u16;
        h[table + (i + 1) * 2..table + (i + 1) * 2 + 2].copy_from_slice(&units.to_le_bytes());
    }
    let body: Vec<u8> = regions.concat();
    let total = (h.len() + body.len()) as u32;
    h[0x0C..0x10].copy_from_slice(&total.to_le_bytes());
    (h, body)
}

fn separate(s: &mut Service, root: &Path, tag: &str, h: &[u8], b: &[u8]) -> String {
    let vh = load(s, root, &format!("{tag}.vh"), h);
    let vb = load(s, root, &format!("{tag}.vb"), b);
    call(
        s,
        "load_vab_bank",
        json!({"job_id": format!("vab-{tag}"), "header": {"source": vh}, "body": {"source": vb}}),
    )["bank"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn separate_bank_is_interpreted_exported_exactly_and_survives_restart() {
    let root = workspace("separate");
    let mut s = Service::new(&root);
    let (h, b) = bank(&[one_shot_region(), loop_region()]);
    let id = separate(&mut s, &root, "a", &h, &b);
    // Discovery recognizes the VH and names the VB only by its file name.
    let found = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id": "again-vh", "path": "a.vh"}),
    );
    let hint = &found["discovery"]["formats"][0];
    assert_eq!(
        (hint["format"].as_str(), hint["packing"].as_str()),
        (Some("vab"), Some("separate"))
    );
    assert_eq!(hint["header"]["body_bytes_expected"], b.len());
    assert!(
        !found["discovery"]["interpretation"]
            .as_str()
            .unwrap()
            .contains("LoROM")
    );
    let body = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id": "again-vb", "path": "a.vb"}),
    );
    assert_eq!(
        body["discovery"]["formats"][0]["format"],
        "vab_body_candidate"
    );
    std::fs::remove_file(root.join("a.vh")).unwrap();
    std::fs::remove_file(root.join("a.vb")).unwrap();
    let mut s = Service::new(&root);

    let summary = call(&mut s, "inspect_vab_bank", json!({"bank": id}));
    assert_eq!(
        summary["fields"]["version"],
        json!({"offset": 4, "value": 7})
    );
    assert_eq!(summary["fields"]["programs"]["value"], 2);
    assert_eq!(
        summary["layout"]["tone_table"]["block_programs"],
        json!([0, 5])
    );
    assert_eq!(summary["checks"]["total_size_matches"], true);
    assert_eq!(
        summary["samples"]["endings"],
        json!({"loop": 1, "end_mute": 1, "silent_loop_block": 0})
    );

    let all = call(
        &mut s,
        "inspect_vab_bank",
        json!({"bank": id, "view": "programs", "limit": 200}),
    );
    assert_eq!(all["items"].as_array().unwrap().len(), 128);
    assert_eq!(
        all["limit"],
        json!({"requested": 200, "applied": 128, "clamped": true, "note": "pages hold 1..=128 items; continue with next_cursor"})
    );
    let programs = call(
        &mut s,
        "inspect_vab_bank",
        json!({"bank": id, "view": "programs", "offset": 5, "limit": 2}),
    );
    assert_eq!(programs["total"], 128);
    assert_eq!(
        programs["items"][0]["fields"]["tones"],
        json!({"offset": 0x20 + 80, "value": 1})
    );
    assert_eq!(programs["items"][0]["tone_block"]["index"], 1);
    // Unused entries keep their reserved and nonzero bytes visible.
    assert_eq!(programs["items"][1]["used"], false);
    assert_eq!(programs["items"][1]["fields"]["priority"]["value"], 0xFF);

    let tones = call(
        &mut s,
        "inspect_vab_bank",
        json!({"bank": id, "view": "tones", "program": 5, "limit": 2}),
    );
    assert_eq!(tones["total"], 16);
    let first = &tones["items"][0];
    assert_eq!(first["offset"], TONE_TABLE + 0x200);
    assert_eq!(first["fields"]["vag"]["value"], 2);
    assert_eq!(
        first["fields"]["reserved_0x18"]["value"],
        "c000c100c200c300"
    );
    assert_eq!(first["adsr"]["sustain"]["direction"], "decrease");
    assert_eq!(first["parent_program_matches"], true);
    assert_eq!(tones["items"][1]["used"], false);
    assert_eq!(tones["items"][1]["fields"]["adsr1"]["value"], "0x80ff");

    let samples = call(
        &mut s,
        "inspect_vab_bank",
        json!({"bank": id, "view": "samples"}),
    );
    let one = &samples["items"][0];
    assert_eq!(one["stream"]["ending"], "end_mute");
    assert_eq!(one["after_end"]["bytes"], 16);
    assert_eq!(
        one["after_end"]["first_block_hex"],
        "0007".to_owned() + &"77".repeat(14)
    );
    assert_eq!(one["referenced_by"], json!([{"program": 0, "tone": 0}]));
    let two = &samples["items"][1];
    assert_eq!(two["stream"]["ending"], "loop");
    assert_eq!(two["stream"]["loop_start_block"], 2);
    let start = &two["offsets"]["loop_start"];
    assert_eq!(
        (
            start["region_offset"].as_u64(),
            start["body_offset"].as_u64(),
            start["source_offset"].as_u64()
        ),
        (Some(32), Some(112), Some(112))
    );
    assert_eq!(two["stream"]["loop_start_offset"], 112);
    assert_eq!(two["body_offset"], 80);
    assert_eq!(two["referenced_by"].as_array().unwrap().len(), 2);

    // The prepare_sample slice decodes through the existing sample path.
    let mut slice = two["prepare_sample"].clone();
    slice.as_object_mut().unwrap().remove("note");
    slice["job_id"] = json!("plan-from-vab");
    slice["target"] = json!("s16le");
    slice["loop_policy"] = json!("preserve");
    let plan = call(&mut s, "prepare_sample", slice);
    assert_eq!(
        plan["report"]["conversion"]["loop"]["policy"], "preserve",
        "{plan}"
    );

    let exported = call(
        &mut s,
        "export_vab_bank",
        json!({"job_id": "export", "bank": id, "destination": "out/a"}),
    );
    assert_eq!(exported["readback_checked"], true);
    assert_eq!(std::fs::read(root.join("out/a/bank.vh")).unwrap(), h);
    assert_eq!(std::fs::read(root.join("out/a/bank.vb")).unwrap(), b);
    let report: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/a/bank.json")).unwrap()).unwrap();
    assert_eq!(report["samples"][0]["decoded"]["samples"], 4 * 28);
    let wav = std::fs::read(root.join("out/a/samples/vag-002.wav")).unwrap();
    assert_eq!(wav.len(), 44 + 5 * 28 * 2);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn combined_bank_loads_from_a_container_slice() {
    let root = workspace("combined");
    let mut s = Service::new(&root);
    let (h, b) = bank(&[one_shot_region(), loop_region()]);
    let mut container = vec![0xAA; 100];
    container.extend(&h);
    container.extend(&b);
    container.extend([0x55; 37]);
    let source = load(&mut s, &root, "VAB.DAT", &container);
    let found = call(
        &mut s,
        "inspect_instrument_source",
        json!({"source": source}),
    );
    let hint = &found["discovery"]["formats"][0];
    assert_eq!(hint["format"], "vab_headers_inside");
    assert_eq!(hint["first"][0]["offset"], 100);
    assert_eq!(hint["first"][0]["header_bytes"], h.len());
    assert_eq!(hint["count"], 1);
    assert_eq!(hint["truncated"], false);
    assert_eq!(hint["headers"]["rows"][0], json!([100, h.len(), b.len()]));
    let mut combined = h.clone();
    combined.extend(&b);
    std::fs::write(root.join("bank.vab"), &combined).unwrap();
    let whole = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id": "whole", "path": "bank.vab"}),
    );
    assert_eq!(whole["discovery"]["formats"][0]["packing"], "combined");
    assert!(
        whole["discovery"]["formats"][0]["note"]
            .as_str()
            .unwrap()
            .contains("alone")
    );
    let length = h.len() + b.len();
    let loaded = call(
        &mut s,
        "load_vab_bank",
        json!({"job_id": "c", "header": {"source": source, "offset": 100, "length": length}}),
    );
    assert_eq!(loaded["packing"], "combined");
    let id = loaded["bank"].as_str().unwrap();
    let samples = call(
        &mut s,
        "inspect_vab_bank",
        json!({"bank": id, "view": "samples"}),
    );
    assert_eq!(
        samples["items"][1]["prepare_sample"]["offset"],
        100 + h.len() + 80
    );
    call(
        &mut s,
        "export_vab_bank",
        json!({"job_id": "e", "bank": id, "destination": "out/c", "decoded_samples": false}),
    );
    assert_eq!(
        std::fs::read(root.join("out/c/bank.vab")).unwrap(),
        container[100..100 + length]
    );
    assert!(!root.join("out/c/samples").exists());
    // Without the length, the rest of the container is retained as trailing body bytes.
    let rest = call(
        &mut s,
        "load_vab_bank",
        json!({"job_id": "c2", "header": {"source": source, "offset": 100}}),
    );
    assert_eq!(rest["checks"]["body_trailing_bytes"], 37);
    assert_eq!(rest["checks"]["total_size_matches"], false);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn malformed_banks_refuse_at_their_offsets_and_sources_stay_retained() {
    let root = workspace("refusals");
    let mut s = Service::new(&root);
    let (h, b) = bank(&[one_shot_region(), loop_region()]);
    let vb = load(&mut s, &root, "ok.vb", &b);
    type Mutation = Box<dyn Fn(&mut Vec<u8>)>;
    let cases: Vec<(Mutation, &str, &str)> = vec![
        (
            Box::new(|h| h[..4].copy_from_slice(b"VABp")),
            "/header",
            "byte 0x0 ",
        ),
        (Box::new(|h| h[0x12] = 1), "/header", "byte 0x12 "),
        (Box::new(|h| h[0x14] = 4), "/header", "byte 0x14 "),
        (Box::new(|h| h[0x20 + 16] = 17), "/header", "byte 0x30 "),
        (
            Box::new(|h| {
                let t = h.len() - 0x200 + 6;
                h[t] = 2;
            }),
            "/header",
            "slot 3",
        ),
        (Box::new(|h| h.truncate(h.len() - 2)), "/header", "VH holds"),
    ];
    for (i, (mutate, location, needle)) in cases.into_iter().enumerate() {
        let mut bad = h.clone();
        mutate(&mut bad);
        let vh = load(&mut s, &root, &format!("bad{i}.vh"), &bad);
        let (at, message) = refusal(
            &mut s,
            "load_vab_bank",
            json!({"job_id": format!("bad-{i}"), "header": {"source": vh}, "body": {"source": vb}}),
        );
        assert_eq!(at, location, "{i}: {message}");
        assert!(message.contains(needle), "{i}: {message}");
        // The rejected original remains retained and exportable.
        let summary = call(&mut s, "inspect_instrument_source", json!({"source": vh}));
        assert_eq!(summary["bytes"], bad.len());
    }
    let vh = load(&mut s, &root, "ok.vh", &h);
    let short = load(&mut s, &root, "short.vb", &b[..100]);
    let (at, message) = refusal(
        &mut s,
        "load_vab_bank",
        json!({"job_id": "short", "header": {"source": vh}, "body": {"source": short}}),
    );
    assert_eq!(at, "/body");
    assert!(message.contains("runs past it"), "{message}");
    let (at, _) = refusal(
        &mut s,
        "load_vab_bank",
        json!({"job_id": "range", "header": {"source": vh, "offset": 4000}, "body": {"source": vb}}),
    );
    assert_eq!(at, "/header");
    let _ = std::fs::remove_dir_all(root);
}

fn mono_wav(rate: u32, pcm: &[i16]) -> Vec<u8> {
    let data: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut w = b"RIFF".to_vec();
    w.extend((36 + data.len() as u32).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend([1, 0, 1, 0]);
    w.extend(rate.to_le_bytes());
    w.extend((rate * 2).to_le_bytes());
    w.extend([2, 0, 16, 0]);
    w.extend(b"data");
    w.extend((data.len() as u32).to_le_bytes());
    w.extend(data);
    w
}

#[test]
fn replacement_from_a_plan_or_donor_accounts_for_every_byte() {
    let root = workspace("replace");
    let mut s = Service::new(&root);
    let (h, b) = bank(&[one_shot_region(), loop_region()]);
    let id = separate(&mut s, &root, "base", &h, &b);
    // 56 samples encode as two data blocks plus end_mute.
    let pcm: Vec<i16> = (0..56)
        .map(|i| ((i as f64 * 0.4).sin() * 9000.0) as i16)
        .collect();
    let wav = load(&mut s, &root, "tone.wav", &mono_wav(22050, &pcm));
    let plan = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "p", "source": wav, "encoding": "wav", "sample_rate": 22050, "target": "psx_adpcm",
            "root": {"note": 60}, "psx_adpcm": {"one_shot_end": "end_mute"}}),
    )["plan"]
        .as_str()
        .unwrap()
        .to_owned();
    // Match-original framing: zero block + 2 blocks + the old after-end block = 64 bytes, not 80.
    let (at, message) = refusal(
        &mut s,
        "replace_vab_sample",
        json!({"job_id": "r0", "bank": id, "vag": 1, "plan": plan}),
    );
    assert_eq!(at, "/vag");
    assert!(
        message.contains("16 bytes short")
            && message.contains("silent_padding")
            && !message.contains("exactly"),
        "{message}"
    );
    // Accepting silent_padding fills the slot with unplayed zero blocks.
    let padded = call(
        &mut s,
        "replace_vab_sample",
        json!({"job_id": "pad", "bank": id, "vag": 1, "plan": plan, "accepted_changes": ["silent_padding"]}),
    );
    assert_eq!(padded["changes"]["silent_padding"]["bytes"], 16);
    // Every changed flag byte is named with its origin.
    let flags = &padded["changes"]["block_flags"];
    assert_eq!(flags["changed"], 3, "{flags}");
    assert_eq!(flags["blocks"][2]["origin"], "silent_padding");
    assert!(
        flags["blocks"][2]["effect"]
            .as_str()
            .unwrap()
            .starts_with("none")
    );
    assert_eq!(flags["blocks"][0]["block"], 2);
    assert_eq!(
        (
            flags["blocks"][0]["old"].as_u64(),
            flags["blocks"][0]["new"].as_u64()
        ),
        (Some(0), Some(1))
    );
    assert_eq!(flags["blocks"][0]["origin"], "the plan's own encoding");
    assert_eq!(flags["blocks"][1]["origin"], "framing: after_end");
    // Block 3 ended the old stream; the new one ends at block 2.
    assert!(
        flags["blocks"][1]["effect"]
            .as_str()
            .unwrap()
            .starts_with("none for this flag")
    );
    assert_eq!(padded["changes"]["region_bytes"]["old_ending"], "end_mute");
    assert_eq!(padded["transfer"]["total_size"]["updated"], false);
    assert_eq!(padded["changes"]["silent_padding"]["body_offset"], 64);
    assert_eq!(padded["transfer"]["size_change"], 0);
    assert_eq!(padded["transfer"]["changed_header_bytes"], json!([]));
    assert_eq!(padded["readback"]["decoded_matches_plan"], true);
    let padded_bank = padded["bank"].as_str().unwrap().to_owned();
    let rows = call(
        &mut s,
        "inspect_vab_bank",
        json!({"bank": padded_bank, "view": "samples"}),
    );
    assert_eq!(rows["items"][0]["after_end"]["bytes"], 32);
    for (args, location) in [
        (
            json!({"job_id": "pad-x", "bank": id, "vag": 1, "plan": plan, "accepted_changes": ["truncate"]}),
            "/accepted_changes",
        ),
        (
            json!({"job_id": "pad-r", "bank": id, "vag": 1, "plan": plan, "layout": "resize", "accepted_changes": ["silent_padding"]}),
            "/accepted_changes",
        ),
    ] {
        assert_eq!(refusal(&mut s, "replace_vab_sample", args).0, location);
    }
    let replaced = call(
        &mut s,
        "replace_vab_sample",
        json!({"job_id": "r1", "bank": id, "vag": 1, "plan": plan, "layout": "resize",
            "tone_updates": [{"program": 0, "tone": 0, "center_note": 72, "fine_tune": 0}]}),
    );
    let parts: Vec<&str> = replaced["source"]["framing_parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["part"].as_str().unwrap())
        .collect();
    assert_eq!(parts, ["leading_zero_block", "after_end"]);
    assert_eq!(replaced["transfer"]["size_change"], -16);
    assert_eq!(
        replaced["transfer"]["shifted_regions"],
        json!([{"vag": 2, "old_body_offset": 80, "new_body_offset": 64}])
    );
    assert_eq!(replaced["readback"]["decoded_matches_plan"], true);
    assert_eq!(replaced["readback"]["leading_silent_samples"], 28);
    assert_eq!(replaced["tone_changes"][0]["field"], "center_note");
    assert_eq!(
        replaced["tones_playing_it"],
        json!([{"program": 0, "tone": 0}])
    );
    // 22050 Hz with root 60 plays in tune one octave up from 44100 Hz.
    assert_eq!(replaced["suggested_tone_tuning"]["center_note"], 72.0);
    assert_eq!(replaced["suggested_tone_tuning"]["fine_tune"], 0.0);
    let derived = replaced["bank"].as_str().unwrap().to_owned();
    let changed: Vec<u64> = replaced["transfer"]["changed_header_bytes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["offset"].as_u64().unwrap())
        .collect();
    let table = (h.len() - 0x200 + 2) as u64;
    assert!(
        changed
            .iter()
            .all(|&o| o == table || (0x0C..0x10).contains(&o)),
        "{changed:?}"
    );
    call(
        &mut s,
        "export_vab_bank",
        json!({"job_id": "x", "bank": derived, "destination": "out/derived"}),
    );
    let new_vh = std::fs::read(root.join("out/derived/bank.vh")).unwrap();
    let new_vb = std::fs::read(root.join("out/derived/bank.vb")).unwrap();
    assert_eq!(new_vb.len(), b.len() - 16);
    assert_eq!(new_vb[64..], b[80..]);
    let differing: Vec<usize> = (0..h.len()).filter(|&i| h[i] != new_vh[i]).collect();
    assert!(differing.contains(&(TONE_TABLE + 4)));
    // The derived bank reloads from its exported files as an ordinary bank.
    let reloaded = separate(&mut s, &root, "derived", &new_vh, &new_vb);
    let summary = call(&mut s, "inspect_vab_bank", json!({"bank": reloaded}));
    assert_eq!(summary["checks"]["total_size_matches"], true);
    // Exact retry replays; the same job with other arguments conflicts.
    let again = s.call(
        "replace_vab_sample",
        json!({"job_id": "r1", "bank": id, "vag": 1, "plan": plan, "layout": "resize",
        "tone_updates": [{"program": 0, "tone": 0, "center_note": 72, "fine_tune": 0}]}),
    );
    assert!(again.replayed);
    // A donor region of the same size copies byte for byte with layout fixed.
    let donor = separate(
        &mut s,
        &root,
        "donor",
        &bank(&[loop_region(), one_shot_region()]).0,
        &[loop_region(), one_shot_region()].concat(),
    );
    let copied = call(
        &mut s,
        "replace_vab_sample",
        json!({"job_id": "d", "bank": id, "vag": 2, "donor_bank": donor, "donor_vag": 2}),
    );
    assert_eq!(copied["transfer"]["changed_header_bytes"], json!([]));
    assert_eq!(copied["source"]["kind"], "vab_region");
    let refusals = [
        (
            json!({"job_id": "q1", "bank": id, "vag": 3, "donor_bank": donor, "donor_vag": 1}),
            "/vag",
        ),
        (
            json!({"job_id": "q2", "bank": id, "vag": 1, "plan": plan, "donor_bank": donor, "donor_vag": 1}),
            "/plan",
        ),
        (
            json!({"job_id": "q3", "bank": id, "vag": 1, "donor_bank": donor, "donor_vag": 1, "framing": "none"}),
            "/framing",
        ),
        (
            json!({"job_id": "q4", "bank": id, "vag": 2, "donor_bank": donor, "donor_vag": 1,
            "tone_updates": [{"program": 5, "tone": 1, "center_note": 60}]}),
            "/tone_updates/0",
        ),
    ];
    for (args, location) in refusals {
        assert_eq!(refusal(&mut s, "replace_vab_sample", args).0, location);
    }
    // A longer region cannot be padded; the refusal names resize and a shorter sample.
    let (at, message) = refusal(
        &mut s,
        "replace_vab_sample",
        json!({"job_id": "long", "bank": derived, "vag": 1, "donor_bank": donor, "donor_vag": 2}),
    );
    assert_eq!(at, "/vag");
    assert!(
        message.contains("16 bytes too long") && message.contains("layout resize"),
        "{message}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn tone_audition_follows_tuning_envelope_and_loop_flags() {
    let root = workspace("audition");
    let mut s = Service::new(&root);
    let (h, b) = bank(&[one_shot_region(), loop_region()]);
    let id = separate(&mut s, &root, "a", &h, &b);
    // Program 0 tone 1 loops with center 72: note 72 plays at pitch 0x1000.
    let looped = call(
        &mut s,
        "render_vab_tone",
        json!({"job_id": "t1", "bank": id, "program": 0, "tone": 1, "note": 72, "key_on_samples": 2000, "release_samples": 500, "destination": "out/t1"}),
    );
    assert_eq!(looped["pitch"]["register"], 0x1000);
    assert_eq!(looped["tone"]["vag"], 2);
    assert!(looped["voice"]["loop_traversals"].as_u64().unwrap() > 10);
    assert_eq!(looped["wav"]["samples"], 2500);
    assert!(root.join("out/t1/audition.wav").is_file());
    let measured = call(&mut s, "measure_audio", looped["measure_with"].clone());
    assert!(measured.to_string().contains("peak"), "{measured}");
    // One octave down halves the pitch register.
    let low = call(
        &mut s,
        "render_vab_tone",
        json!({"job_id": "t2", "bank": id, "program": 0, "tone": 1, "note": 60}),
    );
    assert_eq!(low["pitch"]["register"], 0x800);
    // The one-shot tone stops at its end_mute block.
    let one = call(
        &mut s,
        "render_vab_tone",
        json!({"job_id": "t3", "bank": id, "program": 0, "tone": 0, "pitch_register": 0x1000, "key_on_samples": 1000, "release_samples": 0}),
    );
    assert_eq!(one["voice"]["final_phase"], "off");
    let events = one["voice"]["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "end_mute" && e["sample"] == 112)
    );
    for (args, location) in [
        (
            json!({"job_id": "t4", "bank": id, "program": 1, "tone": 0, "note": 60}),
            "/tone",
        ),
        (
            json!({"job_id": "t5", "bank": id, "program": 0, "tone": 0, "note": 60, "pitch_register": 4096}),
            "/note",
        ),
        (
            json!({"job_id": "t6", "bank": id, "program": 0, "tone": 0, "note": 60, "key_on_samples": 0, "release_samples": 0}),
            "/key_on_samples",
        ),
    ] {
        assert_eq!(refusal(&mut s, "render_vab_tone", args).0, location);
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_dropped_loop_start_flag_on_a_one_shot_is_named_and_judged_unplayed() {
    let root = workspace("flags");
    let mut s = Service::new(&root);
    // VAG 1 is an end_mute one-shot whose block 1 also carries loop start.
    let mut flagged = one_shot_region();
    flagged[17] = 4;
    let (h, b) = bank(&[flagged, loop_region()]);
    let id = separate(&mut s, &root, "flagged", &h, &b);
    let (dh, db) = bank(&[one_shot_region(), loop_region()]);
    let donor = separate(&mut s, &root, "plain", &dh, &db);
    let replaced = call(
        &mut s,
        "replace_vab_sample",
        json!({"job_id": "drop", "bank": id, "vag": 1, "donor_bank": donor, "donor_vag": 1}),
    );
    let changes = &replaced["changes"];
    assert_eq!(changes["region_bytes"]["bytes_differing"], 1);
    let flag = &changes["block_flags"]["blocks"][0];
    assert_eq!(flag["body_offset"], 17);
    assert_eq!(flag["old_bits"], json!(["loop_start"]));
    assert_eq!(flag["new_bits"], json!([]));
    assert_eq!(flag["origin"], "donor region");
    assert!(
        flag["effect"]
            .as_str()
            .unwrap()
            .starts_with("none under the documented SPU"),
        "{flag}"
    );
    assert_eq!(replaced["transfer"]["total_size"]["updated"], false);
    let _ = std::fs::remove_dir_all(root);
}
