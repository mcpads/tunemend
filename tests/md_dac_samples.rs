//! Mega Drive packed four-bit DAC samples (md_dac4) through prepare_sample,
//! inspect_sample and export_sample: declared nibble order, exact decoding to
//! DAC values and PCM16, measured encoding, loops and refusals.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tunemend::service::{Service, Status};

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-mddac-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}

/// The refusal's location and message.
fn refusal(s: &mut Service, mut args: Value) -> (String, String) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    args["job_id"] = json!(format!("refuse-{n}"));
    let r = s.call("prepare_sample", args);
    assert_eq!(r.status, Status::Blocked, "{r:?}");
    let d = &r.diagnostics[0];
    (d.location.clone(), d.message.clone())
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

fn mono_wav(rate: u32, pcm: &[i16]) -> Vec<u8> {
    let data: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut body = b"WAVEfmt ".to_vec();
    body.extend(16u32.to_le_bytes());
    body.extend([1, 0, 1, 0]);
    body.extend(rate.to_le_bytes());
    body.extend((rate * 2).to_le_bytes());
    body.extend([2, 0, 16, 0]);
    body.extend(b"data");
    body.extend((data.len() as u32).to_le_bytes());
    body.extend(data);
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

fn pcm16(wav: &[u8]) -> Vec<i16> {
    wav[44..]
        .chunks(2)
        .map(|w| i16::from_le_bytes([w[0], w[1]]))
        .collect()
}

fn export(s: &mut Service, root: &Path, prepared: &Value, name: &str) -> PathBuf {
    let changes: Vec<String> = prepared["report"]["conversion"]["changes"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    call(
        s,
        "export_sample",
        json!({"job_id": format!("export-{name}"), "plan": prepared["plan"], "destination": name, "accepted_changes": changes}),
    );
    root.join(name)
}

/// A synthetic packed payload inside a larger file: 4 unrelated bytes, then
/// a ramp of every byte value, then 3 trailing bytes.
fn container() -> (Vec<u8>, usize, usize) {
    let mut file = vec![0xA5, 0x5A, 0x00, 0xFF];
    let payload: Vec<u8> = (0..=255u8).collect();
    file.extend(&payload);
    file.extend([0x11, 0x22, 0x33]);
    (file, 4, payload.len())
}

#[test]
fn packed_dac_values_decode_exactly_in_the_declared_order() {
    let root = workspace("decode");
    let mut s = Service::new(&root);
    let (file, offset, length) = container();
    let source = load(&mut s, &root, "bank.bin", &file);
    let base = json!({"source": source, "offset": offset, "length": length,
        "encoding": "md_dac4", "sample_rate": 9861, "md_dac": {"nibble_order": "high_first"}});
    let mut args = base.clone();
    args["job_id"] = json!("codes");
    args["target"] = json!("u8");
    let codes = call(&mut s, "prepare_sample", args);
    let report = &codes["report"]["conversion"];
    assert_eq!(report["decoded_stream_exact"], true, "{report}");
    assert_eq!(report["source_samples"], 512);
    let md = &report["md_dac"];
    assert_eq!(md["layout"]["nibble_order"], "high_first");
    assert_eq!(md["bytes"], 256);
    assert_eq!(md["first_bytes"][1]["dac_values"], json!([0x00, 0x10]));
    assert_eq!(md["nibble_histogram"][0], 32);
    assert_eq!(codes["report"]["md_dac"]["nibble_order"], "high_first");
    let out = export(&mut s, &root, &codes, "codes");
    // The u8 target holds the 8-bit DAC values: high nibble first, << 4.
    let dac: Vec<u8> = (0..=255u8).flat_map(|b| [b & 0xF0, b << 4]).collect();
    assert_eq!(std::fs::read(out.join("sample.bin")).unwrap(), dac);
    assert_eq!(std::fs::read(out.join("original.bin")).unwrap(), file);
    assert_eq!(
        std::fs::read(out.join("input.bin")).unwrap(),
        file[offset..offset + length]
    );
    let source_wav = std::fs::read(out.join("source.wav")).unwrap();
    let expected: Vec<i16> = dac.iter().map(|&c| (i16::from(c) - 128) * 256).collect();
    assert_eq!(pcm16(&source_wav), expected);

    // The other order reads the same bytes with each pair swapped.
    let mut args = base.clone();
    args["job_id"] = json!("low");
    args["target"] = json!("u8");
    args["md_dac"] = json!({"nibble_order": "low_first"});
    let low = call(&mut s, "prepare_sample", args);
    let out = export(&mut s, &root, &low, "low");
    let swapped: Vec<u8> = (0..=255u8).flat_map(|b| [b << 4, b & 0xF0]).collect();
    assert_eq!(std::fs::read(out.join("sample.bin")).unwrap(), swapped);

    // md_dac4 to md_dac4 is byte-identical with no changes; to PCM16 exact.
    for (id, target) in [("same", "md_dac4"), ("pcm16", "s16le")] {
        let mut args = base.clone();
        args["job_id"] = json!(id);
        args["target"] = json!(target);
        let plan = call(&mut s, "prepare_sample", args);
        assert_eq!(plan["report"]["conversion"]["changes"], json!({}), "{id}");
        let out = export(&mut s, &root, &plan, id);
        let bytes = std::fs::read(out.join("sample.bin")).unwrap();
        if target == "md_dac4" {
            assert_eq!(bytes, file[offset..offset + length]);
        } else {
            let values: Vec<i16> = bytes
                .chunks(2)
                .map(|w| i16::from_le_bytes([w[0], w[1]]))
                .collect();
            assert_eq!(values, expected);
        }
    }
    // The summary carries the declared layout.
    let summary = call(&mut s, "inspect_sample", json!({"plan": codes["plan"]}));
    assert_eq!(
        summary["conversion"]["md_dac"]["layout"]["nibble_order"],
        "high_first"
    );
}

#[test]
fn pcm_encodes_to_packed_dac_values_with_measured_accepted_loss() {
    let root = workspace("encode");
    let mut s = Service::new(&root);
    // Odd length, values between levels and one above the top level.
    let pcm: Vec<i16> = vec![0, 1000, 2048, -2049, -20000, 32767, 12288];
    let source = load(&mut s, &root, "in.wav", &mono_wav(9495, &pcm));
    let plan = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "enc", "source": source, "encoding": "wav", "sample_rate": 9495,
            "target": "md_dac4", "md_dac": {"nibble_order": "high_first"}}),
    );
    let conversion = &plan["report"]["conversion"];
    let changes = conversion["changes"].as_object().unwrap();
    assert_eq!(
        changes.keys().collect::<Vec<_>>(),
        ["padding", "sample_values"]
    );
    assert_eq!(changes["padding"]["added_samples"], 1);
    assert_eq!(changes["sample_values"]["changed_samples"], 5);
    let target = &conversion["md_dac_target"];
    assert_eq!(target["clamped_samples"], 1);
    assert_eq!(target["padding_nibble"], 8);
    assert_eq!(target["bytes"], 4);
    // Export refuses until exactly the measured changes are accepted.
    let r = s.call(
        "export_sample",
        json!({"job_id": "partial", "plan": plan["plan"], "destination": "partial", "accepted_changes": ["padding"]}),
    );
    assert_eq!(r.status, Status::Blocked);
    let out = export(&mut s, &root, &plan, "enc");
    let bytes = std::fs::read(out.join("sample.bin")).unwrap();
    // 0->8, 1000->8, 2048->9, -2049->7, -20000->3 (-4.88 rounds to -5),
    // 32767->15 (clamped), 12288->11, padding 8.
    assert_eq!(bytes, [0x88, 0x97, 0x3F, 0xB8]);
    // The exported bytes, decoded again, are the target preview.
    let readback = load(&mut s, &root, "readback.bin", &bytes);
    let again = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "again", "source": readback, "encoding": "md_dac4", "sample_rate": 9495,
            "target": "s16le", "md_dac": {"nibble_order": "high_first"}}),
    );
    let out_again = export(&mut s, &root, &again, "again");
    assert_eq!(
        pcm16(&std::fs::read(out.join("target.wav")).unwrap()),
        pcm16(&std::fs::read(out_again.join("source.wav")).unwrap())
    );
}

#[test]
fn loops_are_declared_and_must_land_on_whole_bytes() {
    let root = workspace("loops");
    let mut s = Service::new(&root);
    let payload: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(37)).collect();
    let source = load(&mut s, &root, "loop.bin", &payload);
    let md = json!({"nibble_order": "high_first"});
    let plan = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "keep", "source": source, "encoding": "md_dac4", "sample_rate": 8000,
            "target": "md_dac4", "md_dac": md, "loop_policy": "preserve", "loop_offset": 10}),
    );
    let report = &plan["report"]["conversion"];
    assert_eq!(report["loop"]["loop_start_sample"], 20);
    assert_eq!(report["loop"]["target_loop_offset"], 10);
    assert_eq!(report["decoded_stream_exact"], true);
    assert!(
        report["loop"]["loop_in_sample_bin"]
            .as_str()
            .unwrap()
            .contains("carries no loop")
    );
    // md_dac4 holds no loop to bake.
    let (_, message) = refusal(
        &mut s,
        json!({"source": source, "encoding": "md_dac4", "sample_rate": 8000, "target": "s16le",
            "md_dac": md, "loop_offset": 10}),
    );
    assert!(message.contains("holds no loop"), "{message}");
    let (_, message) = refusal(
        &mut s,
        json!({"source": source, "encoding": "md_dac4", "sample_rate": 8000, "target": "s16le",
            "md_dac": md, "loop_policy": "preserve", "loop_offset": 32}),
    );
    assert!(message.contains("outside the md_dac4 slice"), "{message}");
    // A WAV loop at an odd frame cannot become a whole-byte target loop.
    let pcm: Vec<i16> = (0..64).map(|i| (i % 8) * 1000).collect();
    let wav = load(&mut s, &root, "odd.wav", &mono_wav(8000, &pcm));
    let (_, message) = refusal(
        &mut s,
        json!({"source": wav, "encoding": "wav", "sample_rate": 8000, "target": "md_dac4",
            "md_dac": md, "loop_policy": "preserve", "wav": {"loop_start_frame": 9}}),
    );
    assert!(message.contains("whole bytes"), "{message}");
    assert!(
        message.contains("wav.loop_start_frame 8 or 10"),
        "{message}"
    );
}

#[test]
fn the_layout_is_required_and_other_misuse_is_refused_where_it_occurs() {
    let root = workspace("refusals");
    let mut s = Service::new(&root);
    let byte_limit = tunemend::sample_codec::MAX_SAMPLES / 2;
    let source = load(&mut s, &root, "p.bin", &vec![0x88; byte_limit + 1]);
    let base = json!({"source": source, "length": 64, "encoding": "md_dac4",
        "sample_rate": 9861, "target": "s16le"});
    let (location, message) = refusal(&mut s, base.clone());
    assert_eq!(location, "/md_dac");
    assert!(message.contains("never inferred"), "{message}");
    let mut args = base.clone();
    args["md_dac"] = json!({"nibble_order": "middle"});
    assert_eq!(refusal(&mut s, args).0, "/md_dac/nibble_order");
    let mut args = base.clone();
    args["encoding"] = json!("u8");
    args["md_dac"] = json!({"nibble_order": "high_first"});
    assert_eq!(refusal(&mut s, args).0, "/md_dac");
    let mut args = base.clone();
    args["md_dac"] = json!({"nibble_order": "high_first"});
    args["codec"] = json!("codec.py");
    assert!(refusal(&mut s, args).1.contains("built-in codec"));
    // Over the decoded-sample limit: the refusal names where it is reached.
    let mut args = base.clone();
    args["md_dac"] = json!({"nibble_order": "high_first"});
    args["length"] = json!(byte_limit + 1);
    let (_, message) = refusal(&mut s, args);
    assert!(
        message.contains(&format!("slice byte {byte_limit}")),
        "{message}"
    );
    // An empty slice is refused rather than producing an empty sample.
    let mut args = base.clone();
    args["md_dac"] = json!({"nibble_order": "high_first"});
    args["length"] = json!(0);
    assert!(refusal(&mut s, args).1.contains("empty"));
}
