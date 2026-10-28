//! prepare_sample level changes (gain_db) and the name-only md_dac4
//! discovery candidate.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tunemend::service::{Service, Status};

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-gain-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}

fn load(s: &mut Service, root: &Path, name: &str, bytes: &[u8]) -> Value {
    std::fs::write(root.join(name), bytes).unwrap();
    call(
        s,
        "load_instrument_source",
        json!({"job_id": format!("load-{name}"), "path": name}),
    )
}

fn s16(values: &[i16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn values(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks(2)
        .map(|w| i16::from_le_bytes([w[0], w[1]]))
        .collect()
}

#[test]
fn gain_scales_before_encoding_and_is_a_reported_accepted_change() {
    let root = workspace("level");
    let mut s = Service::new(&root);
    let source = load(
        &mut s,
        &root,
        "pcm",
        &s16(&[0, 1001, -1001, 20000, -32768, 3]),
    )["source"]
        .clone();
    let base =
        json!({"source": source, "encoding": "s16le", "sample_rate": 22050, "target": "s16le"});
    // Absent and zero gain are the same request and the same plan.
    let mut plain = base.clone();
    plain["job_id"] = json!("plain");
    let plan = call(&mut s, "prepare_sample", plain.clone());
    assert_eq!(plan["report"]["conversion"]["changes"], json!({}));
    assert!(plan["report"].get("gain_db").is_none());
    plain["gain_db"] = json!(0.0);
    let replay = s.call("prepare_sample", plain);
    assert!(replay.replayed, "{replay:?}");
    assert_eq!(replay.result["plan"], plan["plan"]);

    // -6.0206 dB halves: values round half away from zero.
    let mut half = base.clone();
    half["job_id"] = json!("half");
    half["gain_db"] = json!(-6.020599913279624);
    let plan = call(&mut s, "prepare_sample", half);
    let report = &plan["report"];
    assert_eq!(report["gain_db"], -6.020599913279624);
    let gain = &report["conversion"]["changes"]["gain"];
    assert_eq!(gain["clipped_samples"], 0);
    assert_eq!(
        (gain["peak_before"].as_u64(), gain["peak_after"].as_u64()),
        (Some(32768), Some(16384))
    );
    assert!((gain["measured_level_change_db"].as_f64().unwrap() + 6.02).abs() < 0.01);
    assert_eq!(report["conversion"]["sample_values_exact"], true);
    assert_eq!(report["conversion"]["decoded_stream_exact"], false);
    let refused = s.call(
        "export_sample",
        json!({"job_id": "half-none", "plan": plan["plan"], "destination": "none", "accepted_changes": []}),
    );
    assert_eq!(refused.status, Status::Blocked);
    call(
        &mut s,
        "export_sample",
        json!({"job_id": "half-x", "plan": plan["plan"], "destination": "half", "accepted_changes": ["gain"]}),
    );
    let out = values(&std::fs::read(root.join("half/sample.bin")).unwrap());
    assert_eq!(out, [0, 501, -501, 10000, -16384, 2]);

    // +12 dB clips; the count and the clamped peak are reported.
    let mut loud = base.clone();
    loud["job_id"] = json!("loud");
    loud["gain_db"] = json!(12.0);
    let plan = call(&mut s, "prepare_sample", loud);
    let gain = &plan["report"]["conversion"]["changes"]["gain"];
    assert_eq!(gain["clipped_samples"], 2);
    assert_eq!(gain["peak_after"], 32768);

    // Out of range, or with the external codec, refuses.
    for (id, extra) in [
        ("far", json!({"gain_db": 30.0})),
        ("codec", json!({"gain_db": -3.0, "codec": "codec.py"})),
    ] {
        let mut args = base.clone();
        args["job_id"] = json!(id);
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let r = s.call("prepare_sample", args);
        assert_eq!(r.status, Status::Blocked, "{id}: {r:?}");
        assert!(r.diagnostics[0].message.contains("gain"), "{id}: {r:?}");
    }
}

#[test]
fn gain_applies_to_a_preserved_loop_and_to_lossy_targets() {
    let root = workspace("loop");
    let mut s = Service::new(&root);
    let pcm: Vec<i16> = (0..64).map(|i| ((i % 16) * 1500 - 12000) as i16).collect();
    let source = load(&mut s, &root, "loop", &s16(&pcm))["source"].clone();
    let plan = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "loop", "source": source, "encoding": "s16le", "sample_rate": 32000,
            "target": "brr", "loop_policy": "preserve", "loop_offset": 64, "gain_db": 6.0}),
    );
    let conversion = &plan["report"]["conversion"];
    let changes = conversion["changes"].as_object().unwrap();
    assert!(
        changes.contains_key("gain") && changes.contains_key("sample_values"),
        "{changes:?}"
    );
    assert_eq!(changes["gain"]["loop_traversal_clipped_samples"], 0);
    assert_eq!(conversion["loop"]["target_loop_offset"], 18);
    // The md_dac4 target quantizes the scaled stream.
    let plan = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "dac", "source": source, "encoding": "s16le", "sample_rate": 32000,
            "target": "md_dac4", "md_dac": {"nibble_order": "high_first"}, "gain_db": -12.0}),
    );
    let gain = &plan["report"]["conversion"]["changes"]["gain"];
    assert_eq!(gain["peak_before"], 12000);
    assert_eq!(gain["peak_after"], 3014);
}

#[test]
fn a_packed4_name_is_a_declared_candidate_never_a_format_identity() {
    let root = workspace("names");
    let mut s = Service::new(&root);
    let bytes = [0x88u8, 0x97, 0x3F];
    let named = load(&mut s, &root, "voice-001.packed4", &bytes);
    let formats = &named["discovery"]["formats"];
    assert_eq!(formats[0]["format"], "md_dac4_candidate", "{named}");
    assert_eq!(formats[0]["samples_if_whole_file"], 6);
    assert!(
        formats[0]["basis"]
            .as_str()
            .unwrap()
            .contains("not a format identification")
    );
    let other = load(&mut s, &root, "voice-001.bin", &bytes);
    assert_eq!(other["discovery"]["formats"], json!([]));
}

#[test]
fn negative_gain_comes_before_resampling_so_overshoot_does_not_clip() {
    let root = workspace("order");
    let mut s = Service::new(&root);
    // A full-scale square wave: resampling it overshoots full scale.
    let pcm: Vec<i16> = (0..4000)
        .map(|i| if (i / 40) % 2 == 0 { 32767 } else { -32768 })
        .collect();
    let source = load(&mut s, &root, "square", &s16(&pcm))["source"].clone();
    let base = json!({"source": source, "encoding": "s16le", "sample_rate": 32000,
        "target_sample_rate": 22050, "target": "s16le"});
    let mut plain = base.clone();
    plain["job_id"] = json!("plain");
    let plan = call(&mut s, "prepare_sample", plain);
    let overshoot = plan["report"]["conversion"]["changes"]["resampled"]["clipped_samples"]
        .as_u64()
        .unwrap();
    assert!(overshoot > 0, "the fixture must overshoot without gain");
    let mut quiet = base.clone();
    quiet["job_id"] = json!("quiet");
    quiet["gain_db"] = json!(-3.0);
    let plan = call(&mut s, "prepare_sample", quiet);
    let changes = &plan["report"]["conversion"]["changes"];
    assert_eq!(changes["gain"]["clipped_samples"], 0);
    assert_eq!(changes["gain"]["peak_before"], 32768);
    assert_eq!(changes["resampled"]["clipped_samples"], 0, "{changes}");
}

#[test]
fn long_pcm_and_loop_conversion_preserve_every_frame_with_bounded_pitch_analysis() {
    let root = workspace("long-loop");
    let mut service = Service::new(&root);
    // Six seconds at 44.1 kHz crosses the old short-instrument bound.
    let pcm: Vec<i32> = (0..264600).map(|i| (i % 200 - 100) * 180).collect();
    let source = load(
        &mut service,
        &root,
        "long.wav",
        &tunemend::sample_codec::mono_wav(&pcm, 44100),
    )["source"]
        .clone();
    let plan = call(
        &mut service,
        "sample",
        json!({"operation":"prepare_sample","arguments":{
        "job_id":"prepare","source":source,"encoding":"wav","target":"s16le","sample_rate":44100,
        "loop_policy":"preserve","wav":{"loop_start_frame":0}}}),
    );
    let changes = plan["report"]["conversion"]["changes"].as_object().unwrap();
    assert!(!changes.contains_key("sample_values"));
    let accepted: Vec<_> = changes.keys().collect();
    call(
        &mut service,
        "export_sample",
        json!({"job_id":"export","plan":plan["plan"],"destination":"out","accepted_changes":accepted}),
    );
    let encoded = std::fs::read(root.join("out/sample.bin")).unwrap();
    assert_eq!(
        values(&encoded),
        pcm.iter().map(|&v| v as i16).collect::<Vec<_>>()
    );
    let guide = plan.to_string();
    assert!(
        guide.contains("loop exceeds bounded tone analysis"),
        "{guide}"
    );
    assert!(guide.contains("seam"));
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}
