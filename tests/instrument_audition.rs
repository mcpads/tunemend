//! S-DSP auditions of saved bank slots and BRR sample plans, with synthetic data.
use serde_json::{Value, json};
use std::path::Path;
use tunemend::service::{Service, Status};

fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let response = s.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{tool}: {response:?}");
    serde_json::to_value(response).unwrap()
}

fn module(address: u16, body: &[u8]) -> Vec<u8> {
    [
        address.to_le_bytes().as_slice(),
        (body.len() as u16).to_le_bytes().as_slice(),
        body,
        &[255, 255],
    ]
    .concat()
}

fn workspace(name: &str) -> std::path::PathBuf {
    let root =
        std::env::temp_dir().join(format!("tunemend-audition-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn wav_frames(path: &Value) -> Vec<[i16; 2]> {
    let bytes = std::fs::read(Path::new(path.as_str().unwrap())).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 2);
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 32000);
    bytes[44..]
        .chunks(4)
        .map(|c| {
            [
                i16::from_le_bytes([c[0], c[1]]),
                i16::from_le_bytes([c[2], c[3]]),
            ]
        })
        .collect()
}

#[test]
fn plan_audition_reports_registers_replays_and_refuses_bad_requests() {
    let root = workspace("plan");
    // Two periods of a 32-sample sine, looping from sample 32.
    let pcm: Vec<u8> = (0..64)
        .flat_map(|i| {
            { (((i as f64) * std::f64::consts::TAU / 32.0).sin() * 12000.0) as i16 }.to_le_bytes()
        })
        .collect();
    std::fs::write(root.join("pcm"), &pcm).unwrap();
    let mut s = Service::new(&root);
    let source = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id":"pcm","path":"pcm"}),
    )["result"]["source"]
        .clone();
    let plan = call(
        &mut s,
        "prepare_sample",
        json!({"job_id":"plan","source":source,"encoding":"s16le","sample_rate":32000,
            "target":"brr","loop_offset":64,"loop_policy":"preserve"}),
    )["result"]["plan"]
        .clone();
    let args = json!({"job_id":"a1","plan":plan,
        "pitch":{"kind":"note","note":72,"root_note":60,"root_register":4096},
        "envelope":{"kind":"registers","adsr1":0x8f,"adsr2":0xe0,"gain":0},
        "key_on_samples":2000,"release_samples":600});
    let first = call(&mut s, "render_instrument", args.clone());
    let result = &first["result"];
    assert_eq!(result["dsp"]["pitch_register"], 8192);
    assert_eq!(result["dsp"]["envelope_mode"], "adsr");
    assert_eq!(result["dsp"]["aram"]["looping"], true);
    assert_eq!(result["source"]["loop_offset"], 18);
    assert_eq!(result["dsp"]["timing"]["first_audible_sample"], 6);
    assert!(result["dsp"]["loops"]["count"].as_u64().unwrap() > 10);
    let frames = wav_frames(&first["artifacts"][0]["path"]);
    assert_eq!(frames.len(), 2600);
    // Default volumes are equal, so both channels match; release ends in silence.
    assert!(frames.iter().all(|f| f[0] == f[1]));
    assert!(frames[1000..1100].iter().any(|f| f[0].abs() > 1000));
    assert_eq!(frames[2599], [0, 0]);
    // The written WAV is measurable as returned: a workspace-relative path.
    let path = result["wav"]["path"].as_str().unwrap();
    assert!(!Path::new(path).is_absolute(), "{path}");
    assert_eq!(result["measure_with"]["audio"]["path"], path);
    let measured = call(&mut s, "measure_audio", result["measure_with"].clone());
    assert_eq!(measured["result"]["frames"], 2600);
    assert_eq!(
        measured["result"]["input"]["sha256"],
        result["wav"]["sha256"]
    );
    // The same job replays; equal defaults hash equally.
    let mut explicit = args.clone();
    explicit["volume"] = json!({"left":64,"right":64});
    explicit["counter_start"] = json!(0);
    let replay = call(&mut s, "render_instrument", explicit);
    assert_eq!(replay["replayed"], true);
    // A declared observed clock changes only the WAV header.
    let mut clock = args.clone();
    clock["job_id"] = json!("a2");
    clock["wav_sample_rate"] = json!(32040);
    let declared = call(&mut s, "render_instrument", clock);
    let bytes = std::fs::read(declared["artifacts"][0]["path"].as_str().unwrap()).unwrap();
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 32040);
    let nominal = std::fs::read(first["artifacts"][0]["path"].as_str().unwrap()).unwrap();
    assert_eq!(bytes[44..], nominal[44..]);
    // A plan without a recorded root cannot resolve plan_note.
    let mut plan_note = args.clone();
    plan_note["job_id"] = json!("pn0");
    plan_note["pitch"] = json!({"kind":"plan_note","note":72});
    assert_eq!(
        s.call("render_instrument", plan_note.clone()).status,
        Status::Blocked
    );
    // With a root (C4 at 16 kHz), C5 plays at the declared rate: 16000*4096/32000*2.
    let rooted = call(
        &mut s,
        "prepare_sample",
        json!({"job_id":"rooted","source":source,"encoding":"s16le","sample_rate":16000,
            "target":"brr","loop_offset":64,"loop_policy":"preserve","root":{"note":60,"cents":0}}),
    )["result"]["plan"]
        .clone();
    plan_note["job_id"] = json!("pn1");
    plan_note["plan"] = rooted;
    plan_note["destination"] = json!("out/c5");
    let played = call(&mut s, "render_instrument", plan_note);
    assert_eq!(played["result"]["dsp"]["pitch_register"], 4096);
    assert_eq!(played["result"]["source"]["sample_rate"], 16000);
    // The optional folder holds the same WAV as the store and the report.
    let stored = std::fs::read(played["artifacts"][0]["path"].as_str().unwrap()).unwrap();
    assert_eq!(
        std::fs::read(root.join("out/c5/audition.wav")).unwrap(),
        stored
    );
    assert!(root.join("out/c5/audition.json").exists());
    assert_eq!(played["result"]["exported"]["wav"], "out/c5/audition.wav");
    // audition.json holds the audition report; workspace paths stay in the response.
    let written: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/c5/audition.json")).unwrap()).unwrap();
    assert!(written.get("measure_with").is_none() && written["wav"].get("path").is_none());
    let mut changed = args.clone();
    changed["key_on_samples"] = json!(10);
    assert_eq!(s.call("render_instrument", changed).status, Status::Blocked);
    // Bank envelopes need a bank; pitch must fit 14 bits; selections exclude each other.
    for (job, patch) in [
        ("b1", json!({"envelope":{"kind":"bank_table","index":0}})),
        (
            "b2",
            json!({"pitch":{"kind":"note","note":127,"root_note":60,"root_register":4096}}),
        ),
        ("b3", json!({"bank":"0".repeat(64),"slot":0})),
        ("b4", json!({"pitch":{"kind":"register","value":0x4000}})),
        ("b5", json!({"wav_sample_rate":44100})),
    ] {
        let mut bad = args.clone();
        bad["job_id"] = json!(job);
        for (k, v) in patch.as_object().unwrap() {
            bad[k] = v.clone();
        }
        assert_eq!(
            s.call("render_instrument", bad).status,
            Status::Blocked,
            "{job}"
        );
    }
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bank_slot_audition_uses_exact_brr_loop_and_declared_envelope_pair() {
    let root = workspace("bank");
    // Slot 1: three blocks looping from the second block.
    let mut sample = Vec::new();
    for (i, nibble) in [0x13u8, 0x7f, 0x9a].into_iter().enumerate() {
        sample.push(0xb0 | if i == 2 { 3 } else { 0 });
        sample.extend([nibble; 8]);
    }
    let mut bank = vec![0u8; 0x40];
    bank[4..8].copy_from_slice(&[0x40, 0x50, 0x49, 0x50]);
    let mut resident = vec![0u8; 0x100];
    resident[..4].copy_from_slice(&[0x8f, 0xe0, 0x00, 0x00]);
    std::fs::write(
        root.join("bank.bin"),
        module(0x5000, &[bank, sample.clone()].concat()),
    )
    .unwrap();
    std::fs::write(root.join("template.bin"), module(0x4000, &[0; 16])).unwrap();
    std::fs::write(root.join("driver.bin"), module(0x0400, &[1, 2, 3])).unwrap();
    std::fs::write(root.join("resident.bin"), module(0x2000, &resident)).unwrap();
    let mut s = Service::new(&root);
    let id = call(
        &mut s,
        "load_instrument_bank",
        json!({"job_id":"bank","template":"template.bin","bank":"bank.bin","driver":"driver.bin",
            "resident":"resident.bin","layout":{"directory":0x5000,"slots":2,"tuning":0x4000,
            "envelopes":0x2000,"envelope_count":2,"sample_memory_end":0x6000}}),
    )["result"]["bank"]
        .clone();
    let args = json!({"job_id":"slot","bank":id,"slot":1,"pitch":{"kind":"register","value":0x0c00},
        "envelope":{"kind":"bank_table","index":0},"key_on_samples":800,"release_samples":400,
        "volume":{"left":100,"right":-50}});
    let response = call(&mut s, "render_instrument", args.clone());
    let result = &response["result"];
    assert_eq!(result["source"]["sample"]["loop_offset"], 9);
    assert_eq!(result["dsp"]["adsr1"], "0x8F");
    assert_eq!(result["dsp"]["adsr2"], "0xE0");
    assert_eq!(result["dsp"]["aram"]["bytes"], 27);
    assert!(
        result["envelope"]["interpretation"]
            .as_str()
            .unwrap()
            .contains("driver unreviewed")
    );
    let frames = wav_frames(&response["artifacts"][0]["path"]);
    // Opposite-signed voice volumes invert the right channel.
    assert!(frames.iter().any(|f| f[0] > 0 && f[1] < 0));
    // Index 1 has ADSR1 bit 7 clear; 128 needs a reviewed driver.
    for (job, index) in [("e1", 1), ("e2", 128), ("e3", 200)] {
        let mut bad = args.clone();
        bad["job_id"] = json!(job);
        bad["envelope"] = json!({"kind":"bank_table","index":index});
        assert_eq!(
            s.call("render_instrument", bad).status,
            Status::Blocked,
            "{job}"
        );
    }
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}
