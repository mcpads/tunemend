//! PlayStation SPU ADPCM and VAG through prepare_sample, inspect_sample and
//! export_sample: looping and one-shot sources, header preservation,
//! refusals and BRR/PCM/WAV routes.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tunemend::service::{Service, Status};

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-psx-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}

fn refusal(s: &mut Service, mut args: Value) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    args["job_id"] = json!(format!("refuse-{n}"));
    let r = s.call("prepare_sample", args);
    assert_eq!(r.status, Status::Blocked, "{r:?}");
    r.diagnostics[0].message.clone()
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

fn vag(blocks: &[u8], rate: u32, trailing: &[u8]) -> Vec<u8> {
    let mut v = b"VAGp".to_vec();
    v.extend(3u32.to_be_bytes());
    v.extend([0xDE, 0xAD, 0xBE, 0xEF]);
    v.extend((blocks.len() as u32).to_be_bytes());
    v.extend(rate.to_be_bytes());
    v.extend([0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0, 0, 0, 0, 0x55]);
    v.extend(b"harp\0\0\0\0\0\0\0\0\0\0\0\x7f");
    v.extend(blocks);
    v.extend(trailing);
    v
}

/// A looping stream: attack blocks 0..4, loop start flag on block 4,
/// loop end + repeat on block 7.
fn looping_blocks() -> Vec<u8> {
    let mut b = Vec::new();
    for i in 0..8u8 {
        let flags = match i {
            4 => 4,
            7 => 3,
            _ => 0,
        };
        let header = if i == 0 || i == 4 {
            0x06
        } else {
            ((i % 5) << 4) | 7
        };
        b.extend(block(header, flags, i * 29));
    }
    b
}

fn mono_wav(rate: u32, pcm: &[i16], smpl_loop: Option<(u32, u32)>) -> Vec<u8> {
    let data: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut body = b"WAVEfmt ".to_vec();
    body.extend(16u32.to_le_bytes());
    body.extend([1, 0, 1, 0]);
    body.extend(rate.to_le_bytes());
    body.extend((rate * 2).to_le_bytes());
    body.extend([2, 0, 16, 0]);
    if let Some((start, end)) = smpl_loop {
        let mut smpl = Vec::new();
        for v in [0u32, 0, 22676, 60, 0, 0, 0, 1, 0, 0, 0, start, end, 0, 0] {
            smpl.extend(v.to_le_bytes());
        }
        body.extend(b"smpl");
        body.extend((smpl.len() as u32).to_le_bytes());
        body.extend(smpl);
    }
    body.extend(b"data");
    body.extend((data.len() as u32).to_le_bytes());
    body.extend(data);
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
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

#[test]
fn looping_vag_keeps_its_header_and_loop_through_vag_pcm_and_brr() {
    let root = workspace("loop");
    let mut s = Service::new(&root);
    let blocks = looping_blocks();
    let file = vag(&blocks, 22050, &[0xA5, 0x5A]);
    let source = load(&mut s, &root, "harp.vag", &file);
    // Discovery names the container and its declared rate.
    let summary = call(
        &mut s,
        "inspect_instrument_source",
        json!({"source": source}),
    );
    let hint = &summary["discovery"]["formats"][0];
    assert_eq!(
        (hint["format"].as_str(), hint["sample_rate"].as_u64()),
        (Some("vag"), Some(22050))
    );
    assert_eq!(hint["trailing_bytes"], 2);

    let args = json!({"job_id": "vag-vag", "source": source, "encoding": "vag", "sample_rate": 22050,
        "target": "vag", "loop_policy": "preserve"});
    let prepared = call(&mut s, "prepare_sample", args.clone());
    let conversion = &prepared["report"]["conversion"];
    let interpretation = &conversion["psx_adpcm"];
    assert_eq!(interpretation["ending"], "loop");
    assert_eq!(interpretation["loop_start_block"], 4);
    assert_eq!(interpretation["loop_address_source"], "loop_start_flag");
    assert_eq!(interpretation["vag"]["name"], "harp");
    assert_eq!(interpretation["vag"]["reserved"][0]["hex"], "deadbeef");
    assert_eq!(interpretation["vag"]["trailing_bytes"], 2);
    assert_eq!(conversion["loop"]["source_loop_offset"], 48 + 4 * 16);
    assert_eq!(conversion["loop"]["target_loop_offset"], 48 + 4 * 16);
    let target = &conversion["psx_adpcm_target"];
    assert_eq!(target["loop_start_block"], 4);
    assert_eq!(target["end_flags"], 3);
    assert_eq!(target["vag"]["changed_header_offsets"], json!([]));
    let out = export(&mut s, &root, &prepared, "vag-out");
    let sample = std::fs::read(out.join("sample.bin")).unwrap();
    // Every header byte, including reserved and name padding, is the source's.
    assert_eq!(&sample[..48], &file[..48]);
    assert_eq!((sample[48 + 4 * 16 + 1], sample[48 + 7 * 16 + 1]), (4, 3));
    assert_eq!(
        sample[48 + 4 * 16] >> 4,
        0,
        "loop block restarts with filter 0"
    );
    assert_eq!(std::fs::read(out.join("original.bin")).unwrap(), file);
    // The comparison covers loop re-entry.
    assert_eq!(
        conversion["loop"]["compared"],
        "first pass plus one loop traversal"
    );

    // To PCM16: a preserved loop at the same sample.
    let pcm = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "vag-pcm", "source": source, "encoding": "vag", "sample_rate": 22050,
            "target": "s16le", "loop_policy": "preserve"}),
    );
    assert_eq!(
        pcm["report"]["conversion"]["loop"]["target_loop_offset"],
        4 * 28 * 2
    );
    assert_eq!(pcm["report"]["conversion"]["source_samples"], 8 * 28);
    // To BRR through PCM: 112 and 224 samples are 16-aligned too.
    let brr = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "vag-brr", "source": source, "encoding": "vag", "sample_rate": 22050,
            "target": "brr", "loop_policy": "preserve"}),
    );
    let brr_report = &brr["report"]["conversion"];
    assert_eq!(brr_report["loop"]["target_loop_offset"], 7 * 9);
    assert!(brr_report["tuning"]["snes_pitch"].is_object());
    // And back: the BRR plan's sample.bin loaded as a source becomes PSX again.
    let brr_out = export(&mut s, &root, &brr, "brr-out");
    let brr_bytes = std::fs::read(brr_out.join("sample.bin")).unwrap();
    let brr_source = load(&mut s, &root, "harp.brr", &brr_bytes);
    let back = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "brr-psx", "source": brr_source, "encoding": "brr", "sample_rate": 22050,
            "loop_offset": 63, "target": "psx_adpcm", "loop_policy": "preserve", "root": {"note": 64}}),
    );
    let back = &back["report"]["conversion"];
    assert_eq!(back["loop"]["target_loop_offset"], 4 * 16);
    assert_eq!(back["tuning"]["spu_pitch"]["original_pitch"]["value"], 2048);
    assert_eq!(
        back["tuning"]["spu_pitch"]["formula"],
        "pitch = sample_rate * 4096 / 44100"
    );
    // Finite baking of the PSX loop carries history and is reported.
    let baked = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "vag-bake", "source": source, "encoding": "vag", "sample_rate": 22050,
            "target": "s16le", "loop_repeats": 2}),
    );
    let baked = &baked["report"]["conversion"];
    assert_eq!(baked["source_samples"], (8 + 2 * 4) * 28);
    assert_eq!(baked["changes"]["finite_loop"]["additional_traversals"], 2);
    // Equal explicit defaults replay the same job.
    let mut explicit = args;
    explicit["psx_adpcm"] = json!({"one_shot_end": "silent_loop_block", "reinterpret_rate": false});
    let replay = s.call("prepare_sample", explicit);
    assert!(replay.replayed, "{replay:?}");
    assert_eq!(replay.result, prepared);
}

#[test]
fn one_shot_wav_becomes_psx_adpcm_with_the_documented_terminator() {
    let root = workspace("oneshot");
    let mut s = Service::new(&root);
    let tone: Vec<i16> = (0..100)
        .map(|i| (9000.0 * (i as f64 * 0.21).sin()) as i16)
        .collect();
    let source = load(&mut s, &root, "tone.wav", &mono_wav(44100, &tone, None));
    let prepared = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "wav-psx", "source": source, "encoding": "wav", "sample_rate": 44100, "target": "psx_adpcm"}),
    );
    let conversion = &prepared["report"]["conversion"];
    // 100 samples pad to four blocks; the terminator adds one silent block.
    assert_eq!(conversion["target_samples"], 5 * 28);
    assert_eq!(
        conversion["changes"]["padding"]["added_samples"],
        5 * 28 - 100
    );
    assert!(
        conversion["changes"]["sample_values"]["max_absolute_error"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        conversion["psx_adpcm_target"]["one_shot_end"],
        "silent_loop_block"
    );
    assert_eq!(
        conversion["tuning"]["spu_pitch"]["original_pitch"]["hex"],
        "0x1000"
    );
    assert!(
        conversion["scope"]
            .as_str()
            .unwrap()
            .contains("PSX ADPCM uses an exhaustive")
    );
    let refused = s.call(
        "export_sample",
        json!({"job_id": "refuse", "plan": prepared["plan"], "destination": "nope", "accepted_changes": ["padding"]}),
    );
    assert_eq!(refused.status, Status::Blocked);
    let out = export(&mut s, &root, &prepared, "oneshot");
    let sample = std::fs::read(out.join("sample.bin")).unwrap();
    assert_eq!(sample.len(), 5 * 16);
    assert_eq!(&sample[64..66], &[0, 7]);
    assert!(sample[66..].iter().all(|&b| b == 0));
    assert!((0..4).all(|i| sample[i * 16 + 1] == 0));
    // The exported stream reads back as a one-shot source.
    let again = load(&mut s, &root, "oneshot.psx", &sample);
    let read = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "psx-pcm", "source": again, "encoding": "psx_adpcm", "sample_rate": 44100, "target": "s16le"}),
    );
    let read = &read["report"]["conversion"];
    assert_eq!(read["psx_adpcm"]["ending"], "silent_loop_block");
    assert_eq!(read["decoded_stream_exact"], true);
    assert!(read["changes"].get("finite_loop").is_none());
    // Preserving a silent terminator is refused: it is not a musical loop.
    let msg = refusal(
        &mut s,
        json!({"job_id": "psx-keep", "source": again, "encoding": "psx_adpcm", "sample_rate": 44100,
            "target": "s16le", "loop_policy": "preserve"}),
    );
    assert!(msg.contains("silent_loop_block"), "{msg}");
    // end_mute instead sets loop end without repeat on the last data block.
    let mute = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "wav-vag-mute", "source": source, "encoding": "wav", "sample_rate": 44100,
            "target": "vag", "psx_adpcm": {"one_shot_end": "end_mute", "name": "tone"}}),
    );
    let mute_out = export(&mut s, &root, &mute, "mute");
    let vag_bytes = std::fs::read(mute_out.join("sample.bin")).unwrap();
    assert_eq!(&vag_bytes[..8], b"VAGp\0\0\0\x20");
    assert_eq!(&vag_bytes[12..20], &[0, 0, 0, 64, 0, 0, 0xAC, 0x44]);
    assert_eq!(&vag_bytes[32..36], b"tone");
    assert_eq!(vag_bytes.len(), 48 + 4 * 16);
    assert_eq!(vag_bytes[48 + 3 * 16 + 1], 1);
}

#[test]
fn wav_loop_becomes_a_flagged_psx_loop_and_misaligned_loops_are_refused() {
    let root = workspace("wavloop");
    let mut s = Service::new(&root);
    let pcm: Vec<i16> = (0..112).map(|i| ((i % 28) * 700 - 9000) as i16).collect();
    let source = load(
        &mut s,
        &root,
        "loop.wav",
        &mono_wav(32000, &pcm, Some((56, 111))),
    );
    let prepared = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "wav-loop", "source": source, "encoding": "wav", "sample_rate": 32000,
            "target": "psx_adpcm", "loop_policy": "preserve", "wav": {"smpl_loop": 0}}),
    );
    let conversion = &prepared["report"]["conversion"];
    assert_eq!(conversion["loop"]["target_loop_offset"], 32);
    assert_eq!(conversion["loop"]["target_restarts_with_filter_0"], true);
    let sample = &export(&mut s, &root, &prepared, "loop-out");
    let bytes = std::fs::read(sample.join("sample.bin")).unwrap();
    assert_eq!(bytes.len(), 64);
    assert_eq!((bytes[1], bytes[17], bytes[33], bytes[49]), (0, 0, 4, 3));
    let preview = std::fs::read(sample.join("target.wav")).unwrap();
    assert_eq!(&preview[preview.len() - 68..preview.len() - 64], b"smpl");
    // A loop at sample 50 is not on a 28-sample block.
    let source = load(
        &mut s,
        &root,
        "odd.wav",
        &mono_wav(32000, &pcm, Some((50, 111))),
    );
    let msg = refusal(
        &mut s,
        json!({"job_id": "odd", "source": source, "encoding": "wav", "sample_rate": 32000,
            "target": "vag", "loop_policy": "preserve", "wav": {"smpl_loop": 0}}),
    );
    assert!(msg.contains("28-sample-aligned"), "{msg}");
    // The refusal names concrete recoveries for a WAV, not a slice.
    assert!(msg.contains("wav.loop_start_frame 28 or 56"), "{msg}");
    assert!(msg.contains("no target_sample_rate"), "{msg}");
    assert!(msg.contains("loop_policy=finite"), "{msg}");
    assert!(!msg.contains("compatible slice"), "{msg}");
    // 120 frames looping at 60 are whole 28-sample blocks at multiples of
    // 44800 Hz (168 frames looping at 84).
    let pcm: Vec<i16> = (0..120).map(|i| ((i % 30) * 600 - 9000) as i16).collect();
    let source = load(
        &mut s,
        &root,
        "rate.wav",
        &mono_wav(32000, &pcm, Some((60, 119))),
    );
    let args = json!({"source": source, "encoding": "wav", "sample_rate": 32000,
        "target": "psx_adpcm", "loop_policy": "preserve", "wav": {"smpl_loop": 0}});
    let msg = refusal(&mut s, args.clone());
    assert!(msg.contains("target_sample_rate 44800 or 89600"), "{msg}");
    assert!(msg.contains("total length of 112 or 140 frames"), "{msg}");
    let mut fixed = args;
    fixed["job_id"] = json!("rate-fixed");
    fixed["target_sample_rate"] = json!(44800);
    let prepared = call(&mut s, "prepare_sample", fixed);
    assert_eq!(
        prepared["report"]["conversion"]["loop"]["target_loop_start_sample"],
        84
    );
    // The guide keys the encoder's histograms by number and marks which
    // values are in the source's domain.
    let guide = &prepared["report_guide"];
    let filters = &guide["histograms"]["conversion.psx_adpcm_target.filters"];
    let blocks: u64 = filters
        .as_object()
        .unwrap()
        .values()
        .map(|n| n.as_u64().unwrap())
        .sum();
    assert_eq!(
        blocks,
        prepared["report"]["conversion"]["psx_adpcm_target"]["data_blocks"]
    );
    assert!(
        guide["value_domains"]["conversion.sample_rate"]
            .as_str()
            .unwrap()
            .starts_with("source")
    );
    assert!(
        guide["null_fields"]["loop_offset"]
            .as_str()
            .unwrap()
            .contains("always null for WAV")
    );
    // A one-block silent loop is written with flags 7 and still compared and
    // previewed as a loop.
    let mut tail = pcm[..28].to_vec();
    tail.extend([0i16; 28]);
    let source = load(
        &mut s,
        &root,
        "tail.wav",
        &mono_wav(32000, &tail, Some((28, 55))),
    );
    let prepared = call(
        &mut s,
        "prepare_sample",
        json!({"job_id": "silent-loop", "source": source, "encoding": "wav", "sample_rate": 32000,
            "target": "psx_adpcm", "loop_policy": "preserve", "wav": {"smpl_loop": 0}}),
    );
    let out = export(&mut s, &root, &prepared, "silent-loop");
    assert_eq!(std::fs::read(out.join("sample.bin")).unwrap()[17], 7);
    let preview = std::fs::read(out.join("target.wav")).unwrap();
    let smpl = &preview[preview.len() - 60..];
    let at = |i: usize| u32::from_le_bytes(smpl[i..i + 4].try_into().unwrap());
    assert_eq!((at(44), at(48)), (56, 83));
}

#[test]
fn undocumented_reserved_and_foreign_values_are_refused_or_reported() {
    let root = workspace("refuse");
    let mut s = Service::new(&root);
    let base = |source: &str, encoding: &str| json!({"job_id": "r", "source": source, "encoding": encoding, "sample_rate": 22050, "target": "s16le"});
    let with = |mut v: Value, key: &str, value: Value| {
        v[key] = value;
        v
    };
    // Filter 5 is undocumented.
    let mut blocks = block(0x56, 1, 3);
    let f5 = load(&mut s, &root, "f5.vag", &vag(&blocks, 22050, &[]));
    assert!(refusal(&mut s, base(&f5, "vag")).contains("filter 5 is not documented"));
    // Shift 13 is reserved: refused unless decoded as 9 on request.
    blocks[0] = 0x0D;
    let s13 = load(&mut s, &root, "s13.vag", &vag(&blocks, 22050, &[]));
    assert!(refusal(&mut s, base(&s13, "vag")).contains("reserved_shift_as_9"));
    let accepted = call(
        &mut s,
        "prepare_sample",
        with(
            with(base(&s13, "vag"), "job_id", json!("as9")),
            "psx_adpcm",
            json!({"reserved_shift_as_9": true}),
        ),
    );
    assert_eq!(
        accepted["report"]["conversion"]["changes"]["reserved_shift_interpreted"]["decoded_as_shift"],
        9
    );
    assert_eq!(accepted["report"]["psx_adpcm"]["reserved_shift_as_9"], true);
    // Unknown flag bits are retained but reported as not carried.
    let unknown = load(&mut s, &root, "unk.psx", &block(0x02, 0x81, 9));
    let r = call(
        &mut s,
        "prepare_sample",
        with(base(&unknown, "psx_adpcm"), "job_id", json!("unk")),
    );
    assert_eq!(
        r["report"]["conversion"]["changes"]["flag_bits_not_carried"]["blocks"],
        1
    );
    // A repeat without a loop-start flag uses foreign register state.
    let mut stream = block(0x02, 0, 1);
    stream.extend(block(0x02, 3, 2));
    let foreign = load(&mut s, &root, "foreign.psx", &stream);
    let msg = refusal(&mut s, base(&foreign, "psx_adpcm"));
    assert!(msg.contains("Pass loop_offset"), "{msg}");
    let declared = call(
        &mut s,
        "prepare_sample",
        with(
            with(base(&foreign, "psx_adpcm"), "job_id", json!("declared")),
            "loop_offset",
            json!(16),
        ),
    );
    assert_eq!(
        declared["report"]["conversion"]["psx_adpcm"]["loop_address_source"],
        "loop_offset"
    );
    let msg = refusal(
        &mut s,
        with(base(&foreign, "psx_adpcm"), "loop_offset", json!(32)),
    );
    assert!(msg.contains("another sample's storage"), "{msg}");
    // Blocks after the end and missing ends.
    let mut tail = block(0, 1, 0);
    tail.extend(block(0, 0, 0));
    let tail = load(&mut s, &root, "tail.psx", &tail);
    assert!(refusal(&mut s, base(&tail, "psx_adpcm")).contains("more blocks follow"));
    let open = load(&mut s, &root, "open.psx", &block(0, 0, 0));
    assert!(refusal(&mut s, base(&open, "psx_adpcm")).contains("loop-end flag"));
    // Container variants and header disagreements.
    let mut interleaved = vag(&block(0, 1, 0), 22050, &[]);
    interleaved[3] = b'i';
    let interleaved = load(&mut s, &root, "i.vag", &interleaved);
    assert!(refusal(&mut s, base(&interleaved, "vag")).contains("interleaved"));
    let rate = load(&mut s, &root, "rate.vag", &vag(&block(0, 1, 0), 11025, &[]));
    assert!(refusal(&mut s, base(&rate, "vag")).contains("reinterpret_rate"));
    let reinterpreted = call(
        &mut s,
        "prepare_sample",
        with(
            with(base(&rate, "vag"), "job_id", json!("reint")),
            "psx_adpcm",
            json!({"reinterpret_rate": true}),
        ),
    );
    assert!(reinterpreted["report"]["conversion"]["changes"]["rate_reinterpreted"].is_object());
    // Options must name a PSX side; the external codec does not handle PSX.
    let raw = load(&mut s, &root, "raw.u8", &[128; 32]);
    let msg = refusal(
        &mut s,
        with(
            base(&raw, "u8"),
            "psx_adpcm",
            json!({"reinterpret_rate": true}),
        ),
    );
    assert!(msg.contains("psx_adpcm options need"), "{msg}");
    let msg = refusal(
        &mut s,
        with(
            with(base(&raw, "u8"), "target", json!("psx_adpcm")),
            "psx_adpcm",
            json!({"name": "x"}),
        ),
    );
    assert!(msg.contains("needs target vag"), "{msg}");
    let msg = refusal(
        &mut s,
        with(
            with(base(&raw, "u8"), "target", json!("vag")),
            "codec",
            json!("c.py"),
        ),
    );
    assert!(msg.contains("built-in codec"), "{msg}");
}
