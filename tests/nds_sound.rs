//! Nintendo DS sound data through inspect_nds_sound, export_nds_sample and
//! replace_nds_wave on original synthetic SWAR, SWAV, SBNK, STRM and SDAT
//! files: every SWAV encoding looped and one-shot, stereo streams with
//! per-block ADPCM headers, SDAT bank-to-archive relations, located
//! refusals, exact retention and SWAV replacement with offset updates.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tunemend::nds_adpcm;
use tunemend::service::{Service, Status};

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-nds-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}

fn blocked(s: &mut Service, tool: &str, args: Value) -> (String, String) {
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

fn header(id: &[u8; 4], size: usize, blocks: u16) -> Vec<u8> {
    let mut h = id.to_vec();
    h.extend([0xFF, 0xFE, 0x00, 0x01]);
    h.extend((size as u32).to_le_bytes());
    h.extend(16u16.to_le_bytes());
    h.extend(blocks.to_le_bytes());
    h
}

fn finish(mut b: Vec<u8>) -> Vec<u8> {
    let len = b.len() as u32;
    b[8..12].copy_from_slice(&len.to_le_bytes());
    b
}

fn tone(n: usize, period: f64, amp: f64) -> Vec<i32> {
    (0..n)
        .map(|i| ((i as f64 * std::f64::consts::TAU / period).sin() * amp).round() as i32)
        .collect()
}

/// A 12-byte sample info plus data; `loop_words` and length from the data.
fn wave_block(kind: u8, looped: bool, rate: u16, loop_words: u16, data: &[u8]) -> Vec<u8> {
    assert_eq!(data.len() % 4, 0);
    let mut b = vec![kind, looped as u8];
    b.extend(rate.to_le_bytes());
    b.extend(((16_756_991.0 / rate as f64) as u16).to_le_bytes());
    b.extend(loop_words.to_le_bytes());
    b.extend((data.len() as u32 / 4 - loop_words as u32).to_le_bytes());
    b.extend(data);
    b
}

fn adpcm_data(pcm: &[i32]) -> Vec<u8> {
    assert_eq!(pcm.len() % 8, 0);
    nds_adpcm::encode(pcm).bytes
}

fn swar(blocks: &[Vec<u8>], gap: usize) -> Vec<u8> {
    let mut b = header(b"SWAR", 0, 1);
    b.extend(b"DATA");
    b.extend([0u8; 4]);
    b.extend([0u8; 32]);
    b.extend((blocks.len() as u32).to_le_bytes());
    let mut at = 0x3C + 4 * blocks.len() + gap;
    let mut offsets = vec![];
    for block in blocks {
        offsets.push(at as u32);
        at += block.len();
    }
    for o in offsets {
        b.extend(o.to_le_bytes());
    }
    b.extend(vec![0xAA; gap]);
    for block in blocks {
        b.extend(block);
    }
    let size = b.len() as u32 - 0x10;
    b[0x14..0x18].copy_from_slice(&size.to_le_bytes());
    finish(b)
}

/// Four waves: PCM8 one-shot, PCM16 looped, IMA-ADPCM looped and one-shot.
fn four_waves() -> (Vec<u8>, Vec<Vec<i32>>) {
    let pcm8: Vec<i32> = tone(64, 16.0, 100.0);
    let pcm8_bytes: Vec<u8> = pcm8.iter().map(|&v| v as i8 as u8).collect();
    let pcm16 = tone(40, 10.0, 9000.0);
    let pcm16_bytes: Vec<u8> = pcm16
        .iter()
        .flat_map(|&v| (v as i16).to_le_bytes())
        .collect();
    let a = tone(96, 24.0, 15000.0);
    let b = tone(48, 12.0, 6000.0);
    let blocks = vec![
        wave_block(0, false, 8000, 0, &pcm8_bytes),
        wave_block(1, true, 16000, 4, &pcm16_bytes),
        wave_block(2, true, 22050, 1 + 32 / 8, &adpcm_data(&a)),
        wave_block(2, false, 11025, 1, &adpcm_data(&b)),
    ];
    let decoded = vec![
        pcm8.iter().map(|v| v * 256).collect(),
        pcm16,
        nds_adpcm::decode(&adpcm_data(&a), 96).unwrap().pcm,
        nds_adpcm::decode(&adpcm_data(&b), 48).unwrap().pcm,
    ];
    (swar(&blocks, 8), decoded)
}

fn wav_samples(path: &Path) -> (Vec<i32>, Option<(u32, u32)>, u32) {
    let bytes = std::fs::read(path).unwrap();
    let wav = tunemend::wav::parse(&bytes).unwrap();
    let pcm = wav
        .data
        .chunks(2)
        .map(|w| i16::from_le_bytes([w[0], w[1]]) as i32)
        .collect();
    let lp = match &wav.smpl {
        Some(Ok(s)) => s.loops.first().map(|l| (l.start, l.end)),
        _ => None,
    };
    (pcm, lp, wav.sample_rate)
}

#[test]
fn swar_waves_of_every_encoding_inspect_decode_and_export() {
    let root = workspace("swar");
    let mut s = Service::new(&root);
    let (bytes, decoded) = four_waves();
    let src = load(&mut s, &root, "set.swar", &bytes);
    let loaded = call(&mut s, "inspect_instrument_source", json!({"source": src}));
    assert_eq!(loaded["discovery"]["formats"][0]["format"], "nds_swar");

    let summary = call(&mut s, "inspect_nds_sound", json!({"source": src}));
    assert_eq!(summary["waves"]["count"], 4);
    assert_eq!(
        summary["waves"]["encodings"],
        json!({"ima_adpcm": 2, "pcm16": 1, "pcm8": 1})
    );
    assert_eq!(summary["waves"]["looped"], 2);
    assert_eq!(
        summary["layout"]["unreferenced_ranges"][0],
        json!({"offset": 0x4C, "end": 0x54, "bytes": 8})
    );
    assert_eq!(summary["header"]["version"], "0100h");

    let waves = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "view": "waves", "limit": 2}),
    );
    assert_eq!(waves["waves"].as_array().unwrap().len(), 2);
    assert_eq!(waves["next_cursor"], 2);
    assert_eq!(waves["waves"][1]["loop_start_sample"], 8);
    assert_eq!(waves["waves"][1]["timer"]["raw"], 1047);

    let expect = [(64, None), (40, Some(8)), (96, Some(32)), (48, None)];
    for (i, (n, lp)) in expect.into_iter().enumerate() {
        let w = call(
            &mut s,
            "inspect_nds_sound",
            json!({"source": src, "view": "wave", "index": i}),
        );
        assert_eq!(w["wave"]["samples"], n, "wave {i}");
        assert_eq!(w["wave"]["loop_start_sample"], json!(lp));
        let pcm: Vec<u8> = decoded[i]
            .iter()
            .flat_map(|&v| (v as i16).to_le_bytes())
            .collect();
        assert_eq!(
            w["decoded"]["pcm16_sha256"],
            tunemend::store::sha256_hex(&pcm)
        );
        let out = call(
            &mut s,
            "export_nds_sample",
            json!({"job_id": format!("x{i}"), "source": src, "wave": i, "destination": format!("out/w{i}")}),
        );
        let (pcm, smpl, rate) = wav_samples(&root.join(format!("out/w{i}/decoded.wav")));
        assert_eq!(pcm, decoded[i]);
        assert_eq!(smpl, lp.map(|l| (l as u32, n as u32 - 1)));
        assert_eq!(rate, [8000, 16000, 22050, 11025][i]);
        let info = w["wave"]["info_offset"].as_u64().unwrap() as usize;
        let original = std::fs::read(root.join(format!("out/w{i}/original.bin"))).unwrap();
        assert_eq!(original, bytes[info..info + original.len()]);
        assert_eq!(out["report"]["decoded_source"].as_str().unwrap().len(), 64);
    }
    // ADPCM loop state is the first-pass state at the loop start.
    let w2 = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "view": "wave", "index": 2}),
    );
    assert_eq!(w2["codec"]["loop_state"]["pcm"], decoded[2][31]);
    assert_eq!(w2["codec"]["nibble_order"], "low nibble first");

    // The decoded WAV re-enters prepare_sample with its loop.
    let out = call(
        &mut s,
        "export_nds_sample",
        json!({"job_id": "audition", "source": src, "wave": 2, "audition_loops": 3, "destination": "out/a2"}),
    );
    let (audition, _, _) = wav_samples(&root.join("out/a2/audition.wav"));
    assert_eq!(audition.len(), 96 + 3 * 64);
    assert_eq!(audition[96..160], decoded[2][32..]);
    let mut args = out["report"]["prepare_sample"]["arguments"].clone();
    args["job_id"] = json!("plan");
    args["target"] = json!("s16le");
    let plan = call(&mut s, "prepare_sample", args);
    assert_eq!(
        plan["report"]["conversion"]["loop"]["loop_start_sample"],
        32
    );

    // PCM8/PCM16 bytes are direct prepare_sample slices of the original.
    let w1 = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "view": "wave", "index": 1}),
    );
    let mut args = w1["prepare_sample"]["arguments"].clone();
    args["job_id"] = json!("plan16");
    args["target"] = json!("s16le");
    let plan = call(&mut s, "prepare_sample", args);
    assert_eq!(plan["report"]["conversion"]["loop"]["loop_start_sample"], 8);
    assert_eq!(plan["report"]["conversion"]["sample_values_exact"], true);

    let (loc, msg) = blocked(
        &mut s,
        "export_nds_sample",
        json!({"job_id": "one", "source": src, "wave": 0, "audition_loops": 2, "destination": "out/o"}),
    );
    assert_eq!(loc, "/audition_loops");
    assert!(msg.contains("one-shot"));
    let (loc, _) = blocked(
        &mut s,
        "export_nds_sample",
        json!({"job_id": "nowave", "source": src, "destination": "out/n"}),
    );
    assert_eq!(loc, "/wave");
    let (loc, _) = blocked(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "view": "wave", "index": 9}),
    );
    assert_eq!(loc, "/index");
    let (loc, _) = blocked(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "view": "blocks"}),
    );
    assert_eq!(loc, "/view");

    // Restart: a new service reads the retained original and replays jobs.
    drop(s);
    let mut s = Service::new(&root);
    let again = s.call(
        "export_nds_sample",
        json!({"job_id": "x0", "source": src, "wave": 0, "destination": "out/w0"}),
    );
    assert!(again.replayed);
    let exported = call(
        &mut s,
        "export_instrument_source",
        json!({"job_id": "copy", "source": src, "destination": "out/copy"}),
    );
    assert!(exported["readback_checked"].as_bool().unwrap());
    assert_eq!(
        std::fs::read(root.join("out/copy/original.bin")).unwrap(),
        bytes
    );
}

#[test]
fn standalone_swav_and_malformed_files_refuse_with_locations() {
    let root = workspace("refuse");
    let mut s = Service::new(&root);
    let pcm = tone(16, 8.0, 3000.0);
    let block = wave_block(2, true, 32000, 1, &adpcm_data(&pcm));
    let mut swav = header(b"SWAV", 0, 1);
    swav.extend(b"DATA");
    swav.extend(((block.len() + 8) as u32).to_le_bytes());
    swav.extend(&block);
    let swav = finish(swav);
    let src = load(&mut s, &root, "one.swav", &swav);
    let sum = call(&mut s, "inspect_nds_sound", json!({"source": src}));
    assert_eq!(sum["wave"]["loop_start_sample"], 0);
    assert_eq!(sum["swav"]["data_block"]["size_matches_file"], true);

    let (bytes, _) = four_waves();
    let cases: Vec<(Vec<u8>, &str, usize)> = {
        let mut v = vec![];
        let mut t = bytes.clone();
        t.truncate(t.len() - 4);
        v.push((t, "file_size", 8));
        let mut t = bytes.clone();
        t[4] = 0xFE;
        t[5] = 0xFF;
        v.push((t, "byte_order", 4));
        let mut t = bytes.clone();
        t[0x38..0x3C].copy_from_slice(&100_000u32.to_le_bytes());
        v.push((t, "count", 0x38));
        v
    };
    for (i, (b, field, offset)) in cases.into_iter().enumerate() {
        let id = load(&mut s, &root, &format!("bad{i}.swar"), &b);
        let r = s.call("inspect_nds_sound", json!({"source": id}));
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(r.diagnostics[0].location, format!("/source/bytes/{offset}"));
        assert_eq!(r.result["field"], field);
        assert_eq!(r.result["retained"], true);
        // Recognition failure keeps the original.
        let kept = call(
            &mut s,
            "inspect_instrument_source",
            json!({"source": id, "view": "bytes", "limit": 4}),
        );
        assert_eq!(kept["total"], b.len());
    }
    // A refused wave does not hide the others.
    let mut t = bytes.clone();
    let first = u32::from_le_bytes(t[0x3C..0x40].try_into().unwrap()) as usize;
    t[first] = 3;
    let third = u32::from_le_bytes(t[0x44..0x48].try_into().unwrap()) as usize;
    t[third + 12 + 2] = 89;
    let id = load(&mut s, &root, "partial.swar", &t);
    let sum = call(&mut s, "inspect_nds_sound", json!({"source": id}));
    assert_eq!(sum["waves"]["refused"], 1);
    assert_eq!(sum["refusals"][0]["error_offset"], first);
    let (loc, msg) = blocked(
        &mut s,
        "inspect_nds_sound",
        json!({"source": id, "view": "wave", "index": 0}),
    );
    assert_eq!(loc, format!("/source/bytes/{first}"));
    assert!(msg.contains("wave type 3"), "{msg}");
    let (loc, msg) = blocked(
        &mut s,
        "inspect_nds_sound",
        json!({"source": id, "view": "wave", "index": 2}),
    );
    assert_eq!(loc, format!("/source/bytes/{}", third + 12));
    assert!(msg.contains("outside 0..88"), "{msg}");
}

fn note_def(swav: u16, slot: u16, note: u8) -> Vec<u8> {
    let mut d = swav.to_le_bytes().to_vec();
    d.extend(slot.to_le_bytes());
    d.extend([note, 127, 100, 90, 110, 64]);
    d
}

fn sbnk() -> Vec<u8> {
    let records = 5;
    let mut data = vec![];
    let base = 0x3C + 4 * records;
    let mut table = vec![];
    // 0: empty; 1: single; 2: drum set 36..37; 3: key split; 4: undocumented.
    table.push((0u8, 0u16));
    table.push((1, (base + data.len()) as u16));
    data.extend(note_def(1, 0, 60));
    table.push((16, (base + data.len()) as u16));
    data.extend([36, 37]);
    for k in 0..2 {
        data.extend(1u16.to_le_bytes());
        data.extend(note_def(2 + k, 0, 36 + k as u8));
    }
    table.push((17, (base + data.len()) as u16));
    data.extend([50, 127, 0, 0, 0, 0, 0, 0]);
    for k in 0..2 {
        data.extend(1u16.to_le_bytes());
        data.extend(note_def(k, 1, 48 + k as u8 * 24));
    }
    table.push((99, 0));
    data.extend([0xEE, 0xEE]);
    let mut b = header(b"SBNK", 0, 1);
    b.extend(b"DATA");
    b.extend([0u8; 4]);
    b.extend([0u8; 32]);
    b.extend((records as u32).to_le_bytes());
    for (k, o) in table {
        b.push(k);
        b.extend(o.to_le_bytes());
        b.push(0);
    }
    b.extend(data);
    let size = b.len() as u32 - 0x10;
    b[0x14..0x18].copy_from_slice(&size.to_le_bytes());
    finish(b)
}

#[test]
fn sbnk_records_expose_types_definitions_and_unreferenced_bytes() {
    let root = workspace("sbnk");
    let mut s = Service::new(&root);
    let bank = sbnk();
    let src = load(&mut s, &root, "bank.sbnk", &bank);
    let sum = call(&mut s, "inspect_nds_sound", json!({"source": src}));
    assert_eq!(
        sum["instruments"]["types"],
        json!({"drum_set": 1, "empty": 1, "key_split": 1, "single": 1, "undocumented": 1})
    );
    let end = bank.len();
    assert_eq!(
        sum["coverage"]["unreferenced_ranges"][0],
        json!({"offset": end - 2, "end": end, "bytes": 2})
    );
    let ins = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "view": "instruments"}),
    );
    let single = &ins["instruments"][1];
    assert_eq!(single["definition"]["base_note"], 60);
    assert_eq!(single["definition"]["swav"], 1);
    assert_eq!(single["definition"]["attack"], 127);
    let drum = &ins["instruments"][2];
    assert_eq!(drum["entries"][1]["note"], 37);
    assert_eq!(drum["entries"][1]["unknown"], 1);
    let split = &ins["instruments"][3];
    assert_eq!(
        split["regions"][1],
        json!({"low_note": 51, "high_note": 127, "unknown_offset": split["regions"][1]["unknown_offset"], "unknown": 1, "definition": split["regions"][1]["definition"]})
    );
    assert_eq!(split["regions"][1]["definition"]["swar_slot"], 1);
    assert_eq!(ins["instruments"][4]["type_name"], "undocumented");

    // A key split with a nonzero end after an unused one refuses that record only.
    let mut bad = bank.clone();
    let at = u16::from_le_bytes([bad[0x3C + 13], bad[0x3C + 14]]) as usize;
    bad[at + 3] = 90;
    let id = load(&mut s, &root, "bad.sbnk", &bad);
    let sum = call(&mut s, "inspect_nds_sound", json!({"source": id}));
    assert_eq!(sum["instruments"]["refused"], 1);
    assert_eq!(sum["refusals"][0]["error_offset"], at + 3);
}

/// An IMA-ADPCM STRM with per-block headers; `last` samples in the last block.
fn strm(
    channels: usize,
    blocks: usize,
    block_samples: usize,
    last: usize,
    looped: Option<u32>,
) -> (Vec<u8>, Vec<Vec<i32>>) {
    let block_bytes = 4 + block_samples / 2;
    let last_bytes = 4 + last.div_ceil(2);
    let mut data: Vec<u8> = vec![];
    let mut decoded = vec![vec![]; channels];
    for k in 0..blocks {
        for (c, out) in decoded.iter_mut().enumerate() {
            let n = if k + 1 == blocks { last } else { block_samples };
            let target = tone(n, 20.0 + c as f64 * 7.0, 8000.0 + k as f64 * 100.0);
            let e = nds_adpcm::encode(&target);
            out.extend(nds_adpcm::decode(&e.bytes, n).unwrap().pcm);
            data.extend(&e.bytes);
        }
    }
    let total = (blocks - 1) * block_samples + last;
    let mut b = header(b"STRM", 0, 2);
    b.extend(b"HEAD");
    b.extend(0x50u32.to_le_bytes());
    b.extend([2, looped.is_some() as u8, channels as u8, 0]);
    b.extend(22050u16.to_le_bytes());
    b.extend(23u16.to_le_bytes());
    for v in [
        looped.unwrap_or(0),
        total as u32,
        0x68,
        blocks as u32,
        block_bytes as u32,
        block_samples as u32,
        last_bytes as u32,
        last as u32,
    ] {
        b.extend(v.to_le_bytes());
    }
    b.extend([0u8; 32]);
    b.extend(b"DATA");
    let pad = (4 - data.len() % 4) % 4;
    b.extend(((data.len() + pad + 8) as u32).to_le_bytes());
    b.extend(data);
    b.extend(vec![0; pad]);
    (finish(b), decoded)
}

#[test]
fn stereo_strm_blocks_decode_per_channel_and_export_with_loop() {
    let root = workspace("strm");
    let mut s = Service::new(&root);
    let (bytes, decoded) = strm(2, 3, 56, 13, Some(70));
    let src = load(&mut s, &root, "song.strm", &bytes);
    let sum = call(&mut s, "inspect_nds_sound", json!({"source": src}));
    assert_eq!(sum["strm"]["samples"]["value"], 125);
    assert_eq!(sum["strm"]["loop_offset"]["block"], 1);
    assert_eq!(sum["strm"]["loop_offset"]["sample_in_block"], 14);
    assert_eq!(sum["strm"]["layout"]["unused_bytes_after_data"], 2);
    let blocks = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "view": "blocks", "index": 1}),
    );
    assert_eq!(blocks["total"], 3);
    assert_eq!(blocks["blocks"][2]["offset"], 0x68 + 2 * 2 * 32 + 11);
    let out = call(
        &mut s,
        "export_nds_sample",
        json!({"job_id": "r", "source": src, "channel": 1, "audition_loops": 1, "destination": "out/r"}),
    );
    let (pcm, smpl, rate) = wav_samples(&root.join("out/r/decoded.wav"));
    assert_eq!(pcm, decoded[1]);
    assert_eq!((smpl, rate), (Some((70, 124)), 22050));
    assert_eq!(
        std::fs::read(root.join("out/r/original.bin")).unwrap(),
        bytes
    );
    assert_eq!(out["report"]["block_headers"], 3);
    let (loc, _) = blocked(
        &mut s,
        "export_nds_sample",
        json!({"job_id": "c2", "source": src, "channel": 2, "destination": "out/c"}),
    );
    assert_eq!(loc, "/channel");

    // Inconsistent layouts refuse at the field.
    let mut bad = bytes.clone();
    bad[0x34..0x38].copy_from_slice(&57u32.to_le_bytes());
    let id = load(&mut s, &root, "bad.strm", &bad);
    let (loc, msg) = blocked(&mut s, "inspect_nds_sound", json!({"source": id}));
    assert_eq!(loc, "/source/bytes/52");
    assert!(msg.contains("samples per block"), "{msg}");
    let mut bad = bytes.clone();
    bad[0x1A] = 3;
    let id = load(&mut s, &root, "three.strm", &bad);
    let (loc, _) = blocked(&mut s, "inspect_nds_sound", json!({"source": id}));
    assert_eq!(loc, "/source/bytes/26");
}

/// An SDAT with FAT members 0 SWAR, 1 SBNK, 2 STRM; INFO bank 0 uses wave
/// archive 0 (member 0) in slot 0; SYMB names the bank and archive.
fn sdat(members: &[Vec<u8>]) -> Vec<u8> {
    // SYMB
    let mut symb = b"SYMB".to_vec();
    symb.extend([0u8; 4]);
    let lists_at = symb.len();
    symb.extend([0u8; 32]);
    symb.extend([0u8; 24]);
    let set = |symb: &mut Vec<u8>, list: usize, names: &[&str]| {
        let at = symb.len();
        symb[lists_at + 4 * list..lists_at + 4 * list + 4]
            .copy_from_slice(&(at as u32).to_le_bytes());
        symb.extend((names.len() as u32).to_le_bytes());
        let ptrs = symb.len();
        symb.extend(vec![0u8; 4 * names.len()]);
        for (i, n) in names.iter().enumerate() {
            let o = symb.len() as u32;
            symb[ptrs + 4 * i..ptrs + 4 * i + 4].copy_from_slice(&o.to_le_bytes());
            symb.extend(n.as_bytes());
            symb.push(0);
        }
    };
    set(&mut symb, 2, &["BANK_TEST"]);
    set(&mut symb, 3, &["WAVE_TEST"]);
    set(&mut symb, 7, &["STRM_TEST"]);
    while !symb.len().is_multiple_of(4) {
        symb.push(0);
    }
    let n = symb.len() as u32;
    symb[4..8].copy_from_slice(&n.to_le_bytes());
    // INFO
    let mut info = b"INFO".to_vec();
    info.extend([0u8; 4]);
    info.extend([0u8; 32]);
    info.extend([0u8; 24]);
    let list = |info: &mut Vec<u8>, l: usize, entry: Vec<u8>| {
        let at = info.len();
        info[8 + 4 * l..12 + 4 * l].copy_from_slice(&(at as u32).to_le_bytes());
        info.extend(1u32.to_le_bytes());
        let e = (info.len() + 4) as u32;
        info.extend(e.to_le_bytes());
        info.extend(entry);
    };
    let mut bank = 1u16.to_le_bytes().to_vec();
    bank.extend([0, 0]);
    bank.extend(0u16.to_le_bytes());
    bank.extend([0xFF; 6]);
    list(&mut info, 2, bank);
    list(&mut info, 3, vec![0, 0, 0, 0]);
    list(&mut info, 7, vec![2, 0, 0, 0, 100, 64, 0, 0, 0, 0, 0, 0]);
    let n = info.len() as u32;
    info[4..8].copy_from_slice(&n.to_le_bytes());
    // FAT and FILE
    let symb_at = 0x40;
    let info_at = symb_at + symb.len();
    let fat_at = info_at + info.len();
    let fat_len = 12 + 16 * members.len();
    let file_at = fat_at + fat_len;
    let mut file = b"FILE".to_vec();
    file.extend([0u8; 4]);
    file.extend((members.len() as u32).to_le_bytes());
    file.extend([0u8; 4]);
    let mut fat = b"FAT ".to_vec();
    fat.extend((fat_len as u32).to_le_bytes());
    fat.extend((members.len() as u32).to_le_bytes());
    for m in members {
        let at = file_at + file.len();
        fat.extend((at as u32).to_le_bytes());
        fat.extend((m.len() as u32).to_le_bytes());
        fat.extend([0u8; 8]);
        file.extend(m);
        while !file.len().is_multiple_of(4) {
            file.push(0);
        }
    }
    let n = file.len() as u32;
    file[4..8].copy_from_slice(&n.to_le_bytes());
    let mut b = header(b"SDAT", 0, 4);
    b[12..14].copy_from_slice(&0x40u16.to_le_bytes());
    for (o, l) in [
        (symb_at, symb.len()),
        (info_at, info.len()),
        (fat_at, fat_len),
        (file_at, file.len()),
    ] {
        b.extend((o as u32).to_le_bytes());
        b.extend((l as u32).to_le_bytes());
    }
    b.extend([0u8; 16]);
    b.extend(symb);
    b.extend(info);
    b.extend(fat);
    b.extend(file);
    finish(b)
}

#[test]
fn sdat_members_resolve_bank_wave_archives_and_replace_in_place() {
    let root = workspace("sdat");
    let mut s = Service::new(&root);
    let (archive, decoded) = four_waves();
    let (stream, _) = strm(1, 2, 16, 9, None);
    let bytes = sdat(&[archive.clone(), sbnk(), stream]);
    let src = load(&mut s, &root, "sound.sdat", &bytes);
    let sum = call(&mut s, "inspect_nds_sound", json!({"source": src}));
    assert_eq!(
        sum["members"]["swar"],
        json!({"paths": 1, "distinct_byte_streams": 1})
    );
    let members = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "view": "members"}),
    );
    assert_eq!(members["members"][1]["kind"], "sbnk");
    assert_eq!(members["members"][1]["references"][0]["name"], "BANK_TEST");
    assert_eq!(
        members["members"][1]["references"][0]["wave_archive_members"],
        json!([0, null, null, null])
    );
    assert_eq!(members["members"][0]["references"][0]["name"], "WAVE_TEST");
    assert_eq!(members["members"][2]["references"][0]["volume"], 100);

    let ins = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "member": 1, "view": "instruments"}),
    );
    let single = &ins["instruments"][1]["wave_references"][0];
    assert_eq!(single["wave_archive"]["member"], 0);
    assert_eq!(single["wave_archive"]["waves"], 4);
    assert_eq!(single["wave_archive"]["swav_in_range"], true);
    // Slot 1 of the key split has no archive in the bank entry.
    assert_eq!(
        ins["instruments"][3]["wave_references"][1]["wave_archive"],
        Value::Null
    );

    let member_offset = members["members"][0]["offset"].as_u64().unwrap() as usize;
    let w = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "member": 0, "view": "wave", "index": 1}),
    );
    assert_eq!(w["member_offset"], member_offset);
    assert_eq!(
        w["prepare_sample"]["arguments"]["offset"],
        member_offset + w["wave"]["data"]["offset"].as_u64().unwrap() as usize
    );
    call(
        &mut s,
        "export_nds_sample",
        json!({"job_id": "m", "source": src, "member": 0, "wave": 3, "destination": "out/m"}),
    );
    assert_eq!(wav_samples(&root.join("out/m/decoded.wav")).0, decoded[3]);
    let (loc, _) = blocked(
        &mut s,
        "inspect_nds_sound",
        json!({"source": src, "member": 7}),
    );
    assert_eq!(loc, "/member");
    let (loc, _) = blocked(
        &mut s,
        "export_nds_sample",
        json!({"job_id": "b", "source": src, "member": 1, "destination": "out/b"}),
    );
    assert_eq!(loc, "/source");

    // Replacement through a member writes a standalone SWAR copy.
    let wav = tunemend::wav::build(
        1,
        1,
        22050,
        16,
        None,
        &tone(40, 10.0, 5000.0)
            .iter()
            .flat_map(|&v| (v as i16).to_le_bytes())
            .collect::<Vec<_>>(),
        &[],
    );
    let rep = load(&mut s, &root, "new.wav", &wav);
    let r = call(
        &mut s,
        "replace_nds_wave",
        json!({"job_id": "rm", "source": src, "member": 0, "wave": 0, "replacement": rep, "encoding": "pcm16", "destination": "out/rm"}),
    );
    assert_eq!(r["readback"]["unchanged_waves_identical"], 3);
    assert!(r["changes"].as_object().unwrap().is_empty(), "{r}");
}

#[test]
fn replacing_a_wave_moves_later_blocks_and_keeps_the_others_exact() {
    let root = workspace("replace");
    let mut s = Service::new(&root);
    let (bytes, decoded) = four_waves();
    let src = load(&mut s, &root, "set.swar", &bytes);
    let target = tone(200, 25.0, 12000.0);
    let pcm: Vec<u8> = target
        .iter()
        .flat_map(|&v| (v as i16).to_le_bytes())
        .collect();
    let smpl = tunemend::wav::smpl_body(60, 0, &[(0, 40, 199, 0)]);
    let wav = tunemend::wav::build(1, 1, 32000, 16, None, &pcm, &[(b"smpl", smpl)]);
    let rep = load(&mut s, &root, "loop.wav", &wav);

    let args = json!({"job_id": "r1", "source": src, "wave": 1, "replacement": rep, "encoding": "ima_adpcm",
        "smpl_loop": 0, "destination": "out/r1"});
    let r = s.call("replace_nds_wave", args.clone());
    assert_eq!(r.status, Status::Blocked);
    assert!(
        r.diagnostics[0].message.starts_with("sample_values"),
        "{r:?}"
    );
    assert!(!root.join("out/r1").exists());

    let mut ok = args.clone();
    ok["job_id"] = json!("r2");
    ok["accepted_changes"] = json!(["sample_values"]);
    ok["destination"] = json!("out/r2");
    let r = call(&mut s, "replace_nds_wave", ok.clone());
    assert_eq!(r["new_wave"]["loop_start_sample"], 40);
    assert_eq!(r["new_wave"]["samples"], 200);
    assert_eq!(r["new_wave"]["wave_type"]["encoding"], "ima_adpcm");
    assert_eq!(r["readback"]["unchanged_waves_identical"], 3);
    assert_eq!(r["storage"]["following_blocks_moved"], 2);
    let delta = r["storage"]["delta_bytes"].as_i64().unwrap();
    let written = std::fs::read(root.join("out/r2/replaced.swar")).unwrap();
    assert_eq!(written.len() as i64, bytes.len() as i64 + delta);
    let fields: Vec<&str> = r["storage"]["header_changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["file_size", "data_size", "offset[2]", "offset[3]"]);
    // The derived SWAR reads back; other waves decode exactly as before.
    let derived = r["derived_source"].as_str().unwrap();
    for (i, d) in decoded.iter().enumerate() {
        if i == 1 {
            continue;
        }
        let w = call(
            &mut s,
            "inspect_nds_sound",
            json!({"source": derived, "view": "wave", "index": i}),
        );
        let pcm: Vec<u8> = d.iter().flat_map(|&v| (v as i16).to_le_bytes()).collect();
        assert_eq!(
            w["decoded"]["pcm16_sha256"],
            tunemend::store::sha256_hex(&pcm)
        );
    }
    let w = call(
        &mut s,
        "inspect_nds_sound",
        json!({"source": derived, "view": "wave", "index": 1}),
    );
    assert_eq!(w["wave"]["sample_rate"]["raw"], 32000);
    assert_eq!(w["wave"]["timer"]["raw"], 523);
    assert_eq!(
        w["decoded"]["pcm16_sha256"],
        r["readback"]["decoded"]["pcm16_sha256"]
    );
    let replay = s.call("replace_nds_wave", ok);
    assert!(replay.replayed);

    // Loops must start and repeat on whole words.
    let mut bad = args.clone();
    bad["job_id"] = json!("r3");
    bad["smpl_loop"] = Value::Null;
    bad["loop_start_frame"] = json!(42);
    bad["encoding"] = json!("pcm8");
    let (loc, msg) = blocked(&mut s, "replace_nds_wave", bad);
    assert_eq!(loc, "/loop_start_frame");
    assert!(
        msg.contains("Fitting loop starts for this WAV's 200 frames: 40, 44"),
        "{msg}"
    );
    // One-shots pad to a word, which must be accepted.
    let odd = tunemend::wav::build(1, 1, 8000, 16, None, &pcm[..2 * 199], &[]);
    let odd = load(&mut s, &root, "odd.wav", &odd);
    let (_, msg) = blocked(
        &mut s,
        "replace_nds_wave",
        json!({"job_id": "r4", "source": src, "wave": 0, "replacement": odd, "encoding": "pcm16", "destination": "out/r4"}),
    );
    assert!(msg.starts_with("padding"), "{msg}");

    // Two entries sharing one block refuse.
    let mut shared = bytes.clone();
    let first = shared[0x3C..0x40].to_vec();
    shared[0x40..0x44].copy_from_slice(&first);
    let shared = load(&mut s, &root, "shared.swar", &shared);
    let (loc, msg) = blocked(
        &mut s,
        "replace_nds_wave",
        json!({"job_id": "r5", "source": shared, "wave": 1, "replacement": rep, "encoding": "pcm16", "smpl_loop": 0, "destination": "out/r5"}),
    );
    assert_eq!(loc, "/wave");
    assert!(msg.contains("share one sample block"), "{msg}");
}

/// A PCM STRM (`kind` 0 PCM8 or 1 PCM16), channels interleaved per block,
/// looping from sample 3.
fn pcm_strm(kind: u8, samples: &[Vec<i32>], block_samples: usize) -> Vec<u8> {
    let width = if kind == 0 { 1 } else { 2 };
    let total = samples[0].len();
    let blocks = total.div_ceil(block_samples);
    let last = total - (blocks - 1) * block_samples;
    let mut data: Vec<u8> = vec![];
    for k in 0..blocks {
        for channel in samples {
            let n = if k + 1 == blocks { last } else { block_samples };
            for &v in &channel[k * block_samples..k * block_samples + n] {
                if kind == 0 {
                    data.push((v >> 8) as i8 as u8);
                } else {
                    data.extend((v as i16).to_le_bytes());
                }
            }
        }
    }
    let mut b = header(b"STRM", 0, 2);
    b.extend(b"HEAD");
    b.extend(0x50u32.to_le_bytes());
    b.extend([kind, 1, samples.len() as u8, 0]);
    b.extend(32000u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    for v in [
        3u32,
        total as u32,
        0x68,
        blocks as u32,
        (block_samples * width) as u32,
        block_samples as u32,
        (last * width) as u32,
        last as u32,
    ] {
        b.extend(v.to_le_bytes());
    }
    b.extend([0u8; 32]);
    b.extend(b"DATA");
    b.extend(((data.len() + 8) as u32).to_le_bytes());
    b.extend(data);
    finish(b)
}

#[test]
fn pcm_streams_decode_per_block_and_loop_from_their_sample() {
    let root = workspace("pcm-strm");
    let mut s = Service::new(&root);
    let left: Vec<i32> = tone(22, 7.0, 20000.0)
        .iter()
        .map(|v| v / 256 * 256)
        .collect();
    let right: Vec<i32> = tone(22, 5.0, 9000.0)
        .iter()
        .map(|v| v / 256 * 256)
        .collect();
    for kind in [0u8, 1] {
        let bytes = pcm_strm(kind, &[left.clone(), right.clone()], 8);
        let src = load(&mut s, &root, &format!("pcm{kind}.strm"), &bytes);
        let sum = call(&mut s, "inspect_nds_sound", json!({"source": src}));
        assert_eq!(
            sum["strm"]["wave_type"]["encoding"],
            ["pcm8", "pcm16"][kind as usize]
        );
        assert_eq!(sum["strm"]["last_block_samples"]["value"], 6);
        call(
            &mut s,
            "export_nds_sample",
            json!({"job_id": format!("p{kind}"), "source": src, "channel": 1, "audition_loops": 1,
                "destination": format!("out/p{kind}")}),
        );
        let (pcm, smpl, rate) = wav_samples(&root.join(format!("out/p{kind}/decoded.wav")));
        assert_eq!(pcm, right);
        assert_eq!((smpl, rate), (Some((3, 21)), 32000));
        let (audition, _, _) = wav_samples(&root.join(format!("out/p{kind}/audition.wav")));
        assert_eq!(audition[22..], right[3..]);
        let (loc, _) = blocked(
            &mut s,
            "inspect_nds_sound",
            json!({"source": src, "view": "blocks"}),
        );
        assert_eq!(loc, "/view");
    }
}
