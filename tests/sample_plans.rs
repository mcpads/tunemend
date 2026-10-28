use serde_json::{Value, json};
use tunemend::service::{Service, Status};
fn call(s: &mut Service, tool: &str, args: Value) -> Value {
    let r = s.call(tool, args);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    r.result
}
#[test]
fn sample_plans_retain_resume_refuse_unaccepted_changes_and_detect_corruption() {
    let root = std::env::temp_dir().join(format!("tunemend-sample-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("raw"), [0, 1, 2, 3]).unwrap();
    // Protocol fixture isolates orchestration from the separately tested shared codec.
    std::fs::write(root.join("codec.py"),r#"import json,sys
r=json.load(sys.stdin)
json.dump({'report':{'changes':{'sample_values':{'changed_samples':1}}},'files':{'sample.bin':r['data'],'source.wav':[],'target.wav':[]}},sys.stdout)
"#).unwrap();
    let mut s = Service::new(&root);
    let source = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id":"raw","path":"raw"}),
    );
    let args = json!({"job_id":"prepare","source":source["source"],"encoding":"u8","sample_rate":8000,"target":"brr","codec":"codec.py","offset":1,"length":2});
    let prepared = call(&mut s, "prepare_sample", args.clone());
    assert!(s.call("prepare_sample", args.clone()).replayed);
    // Explicit defaults mean the same request; a different value is a conflict.
    let mut explicit = args.clone();
    explicit["loop_repeats"] = json!(0);
    explicit["python"] = json!("python3");
    let replay = s.call("prepare_sample", explicit);
    assert!(replay.replayed, "{replay:?}");
    assert_eq!(replay.result, prepared);
    let mut different = args;
    different["sample_rate"] = json!(16000);
    let conflict = s.call("prepare_sample", different);
    assert_eq!(conflict.status, Status::Blocked);
    assert_eq!(
        serde_json::to_value(conflict.diagnostics[0].code).unwrap(),
        "job_conflict"
    );
    std::fs::remove_file(root.join("raw")).unwrap();
    std::fs::remove_file(root.join("codec.py")).unwrap();
    drop(s);
    let mut s = Service::new(&root);
    let plan = &prepared["plan"];
    let inspected = call(
        &mut s,
        "inspect_sample",
        json!({"plan":plan,"detail":"full"}),
    );
    assert_eq!(inspected["report"], prepared["report"]);
    assert_eq!(
        s.call(
            "export_sample",
            json!({"job_id":"refuse","plan":plan,"destination":"out"})
        )
        .status,
        Status::Blocked
    );
    call(
        &mut s,
        "export_sample",
        json!({"job_id":"export","plan":plan,"destination":"out","accepted_changes":["sample_values"]}),
    );
    let omitted = s.call(
        "export_sample",
        json!({"job_id":"refuse","plan":plan,"destination":"out","accepted_changes":[]}),
    );
    assert!(omitted.replayed, "{omitted:?}");
    assert_eq!(
        std::fs::read(root.join("out/original.bin")).unwrap(),
        [0, 1, 2, 3]
    );
    assert_eq!(std::fs::read(root.join("out/input.bin")).unwrap(), [1, 2]);
    assert!(!root.join("out/codec.py").exists());
    assert_eq!(s.call("export_sample",json!({"job_id":"duplicate","plan":plan,"destination":"out","accepted_changes":["sample_values"]})).status,Status::Blocked);
    let hash = prepared["report"]["objects"]["input.bin"].as_str().unwrap();
    // Locate the retained content-addressed object without assuming store layout.
    fn corrupt(dir: &std::path::Path, hash: &str) -> bool {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                if corrupt(&p, hash) {
                    return true;
                }
            } else if p.file_name().unwrap() == hash {
                std::fs::write(p, b"corrupt").unwrap();
                return true;
            }
        }
        false
    }
    assert!(corrupt(&root, hash));
    assert_eq!(
        s.call("inspect_sample", json!({"plan":plan})).status,
        Status::Error
    );
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn built_in_codec_preserves_loops_and_refuses_incompatible_loop_requests() {
    let root = std::env::temp_dir().join(format!("tunemend-sample-loop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // 48 PCM16 samples looping from sample 16.
    let pcm: Vec<u8> = (0..48i16).flat_map(|i| (i * 300).to_le_bytes()).collect();
    std::fs::write(root.join("pcm"), &pcm).unwrap();
    let mut s = Service::new(&root);
    let source = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id":"pcm","path":"pcm"}),
    )["source"]
        .clone();
    let base = json!({"source":source,"encoding":"s16le","sample_rate":32000,"target":"brr","loop_offset":32,"loop_policy":"preserve"});
    let mut args = base.clone();
    args["job_id"] = json!("loop");
    let plan = call(&mut s, "prepare_sample", args);
    let report = &plan["report"];
    assert_eq!(report["loop_policy"], "preserve");
    assert_eq!(report["conversion"]["loop"]["target_loop_offset"], 9);
    assert!(report["conversion"]["changes"].get("finite_loop").is_none());
    assert!(
        report["codec_authority"]
            .as_str()
            .unwrap()
            .starts_with("built-in")
    );
    let changes: Vec<String> = report["conversion"]["changes"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    call(
        &mut s,
        "export_sample",
        json!({"job_id":"export","plan":plan["plan"],"destination":"out","accepted_changes":changes}),
    );
    let brr = std::fs::read(root.join("out/sample.bin")).unwrap();
    assert_eq!((brr.len(), brr[18] & 3), (27, 3));

    // Unaligned loops, loop baking requests and external codecs are refused.
    for (job, key, value) in [
        ("unaligned", "loop_offset", json!(34)),
        ("repeats", "loop_repeats", json!(1)),
        ("external", "codec", json!("codec.py")),
    ] {
        let mut args = base.clone();
        args["job_id"] = json!(job);
        args[key] = value;
        assert_eq!(
            s.call("prepare_sample", args).status,
            Status::Blocked,
            "{job}"
        );
    }
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

use tunemend::wav::{build, smpl_body};

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn load(s: &mut Service, root: &std::path::Path, name: &str, bytes: &[u8]) -> Value {
    std::fs::write(root.join(name), bytes).unwrap();
    call(
        s,
        "load_instrument_source",
        json!({"job_id":format!("load-{name}"),"path":name}),
    )["source"]
        .clone()
}

fn change_kinds(plan: &Value) -> Vec<String> {
    plan["report"]["conversion"]["changes"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

fn refusal(s: &mut Service, args: Value) -> String {
    let r = s.call("prepare_sample", args);
    assert_eq!(r.status, Status::Blocked, "{r:?}");
    r.diagnostics[0].message.clone()
}

fn s16(values: impl IntoIterator<Item = i16>) -> Vec<u8> {
    values.into_iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn export(
    s: &mut Service,
    job: &str,
    plan: &Value,
    changes: Vec<String>,
) -> tunemend::service::ToolResponse {
    s.call(
        "export_sample",
        json!({"job_id":job,"plan":plan["plan"],"destination":job,"accepted_changes":changes}),
    )
}

#[test]
fn wav_sources_decode_every_format_and_measure_reductions() {
    let root = workspace("wav-formats");
    let mut s = Service::new(&root);
    let ramp: Vec<i16> = (0..32).map(|i| i * 700 - 11000).collect();
    let s24: Vec<u8> = ramp
        .iter()
        .flat_map(|&v| (((v as i32) << 8) | 0x80).to_le_bytes()[..3].to_vec())
        .collect();
    let s32: Vec<u8> = ramp
        .iter()
        .flat_map(|&v| ((v as i32) << 16).to_le_bytes())
        .collect();
    let f32s: Vec<u8> = ramp
        .iter()
        .flat_map(|&v| (v as f32 / 32768.0).to_le_bytes())
        .collect();
    let f64s: Vec<u8> = ramp
        .iter()
        .flat_map(|&v| ((v as f64 + 0.25) / 32768.0).to_le_bytes())
        .collect();
    let u8s: Vec<u8> = ramp.iter().map(|&v| ((v >> 8) + 128) as u8).collect();
    // (name, format tag, bits, extensible subformat, data, expected change kinds)
    type Case<'a> = (&'a str, u16, u16, Option<u16>, Vec<u8>, Vec<&'a str>);
    let cases: Vec<Case> = vec![
        ("u8", 1, 8, None, u8s, vec![]),
        ("s16", 1, 16, None, s16(ramp.clone()), vec![]),
        ("s24", 1, 24, None, s24.clone(), vec!["bit_depth_reduction"]),
        ("s32", 1, 32, None, s32, vec![]),
        ("f32", 3, 32, None, f32s.clone(), vec![]),
        ("f64", 3, 64, None, f64s, vec!["bit_depth_reduction"]),
        (
            "ext-s24",
            0xFFFE,
            24,
            Some(1),
            s24,
            vec!["bit_depth_reduction"],
        ),
        ("ext-f32", 0xFFFE, 32, Some(3), f32s, vec![]),
    ];
    for (name, tag, bits, ext, data, expected) in cases {
        let wav = build(tag, 1, 22050, bits, ext, &data, &[]);
        let source = load(&mut s, &root, name, &wav);
        let plan = call(
            &mut s,
            "prepare_sample",
            json!({"job_id":format!("p-{name}"),"source":source,"encoding":"wav","sample_rate":22050,"target":"s16le"}),
        );
        assert_eq!(change_kinds(&plan), expected, "{name}");
        let report = &plan["report"]["conversion"]["wav"];
        assert_eq!(report["bits_per_sample"], bits, "{name}");
        assert_eq!(report["extensible"], ext.is_some(), "{name}");
        assert_eq!(report["header_sample_rate"], 22050, "{name}");
        if !expected.is_empty() {
            let reduction = &plan["report"]["conversion"]["changes"]["bit_depth_reduction"];
            assert_eq!(reduction["changed_samples"], 32, "{name}");
            // Unaccepted reductions are refused at export.
            assert_eq!(
                export(&mut s, &format!("x-{name}"), &plan, vec![]).status,
                Status::Blocked
            );
            continue;
        }
        // Exact formats deliver the decoded PCM16 values and the original file.
        let delivered = export(&mut s, &format!("e-{name}"), &plan, vec![]);
        assert_eq!(delivered.status, Status::Ok, "{delivered:?}");
        let want = if name == "u8" {
            s16(ramp.iter().map(|&v| (v >> 8) << 8))
        } else {
            s16(ramp.clone())
        };
        assert_eq!(
            std::fs::read(root.join(format!("e-{name}/sample.bin"))).unwrap(),
            want,
            "{name}"
        );
        assert_eq!(
            std::fs::read(root.join(format!("e-{name}/input.bin"))).unwrap(),
            wav
        );
    }
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn wav_channels_rates_and_malformed_files_need_explicit_choices() {
    let root = workspace("wav-choices");
    let mut s = Service::new(&root);
    let frames: Vec<i16> = (0..16).flat_map(|i| [i * 100, -i * 100 - 1]).collect();
    let stereo = build(1, 2, 32000, 16, None, &s16(frames), &[]);
    let source = load(&mut s, &root, "stereo", &stereo);
    let base = json!({"source":source,"encoding":"wav","sample_rate":32000,"target":"s16le"});
    let with = |job: &str, extra: Value| {
        let mut args = base.clone();
        args["job_id"] = json!(job);
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        args
    };
    assert!(refusal(&mut s, with("none", json!({}))).contains("2 channels"));
    let right = call(
        &mut s,
        "prepare_sample",
        with("right", json!({"wav":{"channel":1}})),
    );
    assert!(change_kinds(&right).is_empty());
    assert_eq!(
        right["report"]["conversion"]["wav"]["selection"],
        json!({"channel": 1})
    );
    let mean = call(
        &mut s,
        "prepare_sample",
        with("mean", json!({"wav":{"downmix":"mean"}})),
    );
    assert_eq!(
        change_kinds(&mean),
        ["bit_depth_reduction", "channel_downmix"]
    );
    assert_eq!(
        export(
            &mut s,
            "mean-partial",
            &mean,
            vec!["channel_downmix".into()]
        )
        .status,
        Status::Blocked
    );
    assert_eq!(
        export(&mut s, "mean-out", &mean, change_kinds(&mean)).status,
        Status::Ok
    );
    // The header rate must match unless reinterpretation is explicit.
    assert!(
        refusal(
            &mut s,
            with("rate", json!({"wav":{"channel":0},"sample_rate":16000}))
        )
        .contains("declares 32000 Hz")
    );
    let slow = call(
        &mut s,
        "prepare_sample",
        with(
            "slow",
            json!({"wav":{"channel":0,"reinterpret_rate":true},"sample_rate":16000}),
        ),
    );
    assert_eq!(
        slow["report"]["conversion"]["changes"]["rate_reinterpreted"]["pitch_change_cents"],
        -1200.0
    );
    for (job, extra, expected) in [
        (
            "both",
            json!({"wav":{"channel":0,"downmix":"mean"}}),
            "not both",
        ),
        ("sum", json!({"wav":{"downmix":"sum"}}), "\"mean\""),
        (
            "channel",
            json!({"wav":{"channel":2}}),
            "outside the file's 2 channels",
        ),
        (
            "offset",
            json!({"wav":{"channel":0},"loop_offset":4}),
            "loop_offset is for raw slices",
        ),
        (
            "codec",
            json!({"wav":{"channel":0},"codec":"codec.py"}),
            "built-in codec",
        ),
        (
            "finite-loop",
            json!({"wav":{"channel":0,"loop_start_frame":4}}),
            "loop_policy=preserve",
        ),
        (
            "raw",
            json!({"encoding":"s16le","wav":{"channel":0}}),
            "wav options need encoding=wav",
        ),
    ] {
        assert!(
            refusal(&mut s, with(job, extra)).contains(expected),
            "{job}"
        );
    }
    // Malformed files are refused with a location; the original stays loaded.
    let mut truncated = stereo.clone();
    truncated.truncate(stereo.len() - 3);
    let broken = load(&mut s, &root, "truncated", &truncated);
    let message = refusal(
        &mut s,
        json!({"job_id":"truncated","source":broken,"encoding":"wav","sample_rate":32000,"target":"s16le"}),
    );
    assert!(
        message.contains("byte 4") && message.contains("truncated"),
        "{message}"
    );
    let mut bad_chunk = stereo.clone();
    bad_chunk[40..44].copy_from_slice(&1000u32.to_le_bytes());
    bad_chunk[4..8].copy_from_slice(&((stereo.len() - 8) as u32).to_le_bytes());
    let bad = load(&mut s, &root, "bad-chunk", &bad_chunk);
    let message = refusal(
        &mut s,
        json!({"job_id":"bad","source":bad,"encoding":"wav","sample_rate":32000,"target":"s16le"}),
    );
    assert!(
        message.contains("chunk 'data' at byte 36 declares 1000 bytes"),
        "{message}"
    );
    // An empty wav object means the same request as an omitted one.
    let mono = build(1, 1, 32000, 16, None, &s16([1, 2, 3]), &[]);
    let mono = load(&mut s, &root, "mono", &mono);
    let args = json!({"job_id":"mono","source":mono,"encoding":"wav","sample_rate":32000,"target":"s16le"});
    let first = call(&mut s, "prepare_sample", args.clone());
    let mut explicit = args;
    explicit["wav"] = json!({"smpl_root": false, "reinterpret_rate": false});
    let replay = s.call("prepare_sample", explicit);
    assert!(replay.replayed, "{replay:?}");
    assert_eq!(replay.result, first);
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn smpl_loops_and_roots_are_reported_and_used_only_when_chosen_and_survive_restart() {
    let root = workspace("wav-smpl");
    let mut s = Service::new(&root);
    let frames: Vec<i16> = (0..64).map(|i| ((i % 32) * 600 - 9000) as i16).collect();
    let smpl = smpl_body(57, 1 << 30, &[(0, 32, 63, 0), (0, 16, 40, 0)]);
    let wav = build(
        1,
        1,
        32000,
        16,
        None,
        &s16(frames),
        &[(b"LIST", b"INFOtest".to_vec()), (b"smpl", smpl)],
    );
    let source = load(&mut s, &root, "smpl.wav", &wav);
    let base = json!({"source":source,"encoding":"wav","sample_rate":32000,"target":"brr"});
    let with = |job: &str, extra: Value| {
        let mut args = base.clone();
        args["job_id"] = json!(job);
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        args
    };
    // Reported as information without being used.
    let info = call(&mut s, "prepare_sample", with("info", json!({})));
    let conversion = &info["report"]["conversion"];
    assert_eq!(
        conversion["wav"]["smpl"]["loops"].as_array().unwrap().len(),
        2
    );
    assert_eq!(conversion["wav"]["smpl"]["unity_note"], 57);
    assert_eq!(conversion["wav"]["uninterpreted_chunks"], json!(["LIST"]));
    assert!(conversion.get("loop").is_none());
    assert_eq!(conversion["tuning"]["root"], Value::Null);
    assert_eq!(
        conversion["tuning"]["snes_pitch"]["original_pitch"]["value"],
        4096
    );
    // Opted in: the loop is preserved and the root comes from smpl.
    let used = call(
        &mut s,
        "prepare_sample",
        with(
            "used",
            json!({"loop_policy":"preserve","wav":{"smpl_loop":0,"smpl_root":true}}),
        ),
    );
    let conversion = &used["report"]["conversion"];
    assert_eq!(conversion["loop"]["target_loop_offset"], 18);
    assert_eq!(conversion["loop"]["loop_source"], "smpl loop 0");
    assert_eq!(
        conversion["tuning"]["root"],
        json!({"note": 57, "cents": 25.0, "source": "smpl"})
    );
    // 25 cents sharp plays in tune at 4096 * 2^(-25/1200).
    assert_eq!(
        conversion["tuning"]["snes_pitch"]["root_in_tune"]["value"],
        4037
    );
    assert_eq!(
        used["report"]["wav"],
        json!({"smpl_loop": 0, "smpl_root": true})
    );
    for (job, extra, expected) in [
        (
            "tail",
            json!({"loop_policy":"preserve","wav":{"smpl_loop":1}}),
            "ends at frame 40",
        ),
        (
            "missing",
            json!({"loop_policy":"preserve","wav":{"smpl_loop":2}}),
            "does not exist",
        ),
        (
            "roots",
            json!({"wav":{"smpl_root":true},"root":{"note":60}}),
            "exclusive",
        ),
        (
            "unaligned",
            json!({"loop_policy":"preserve","wav":{"loop_start_frame":33}}),
            "16-sample-aligned",
        ),
    ] {
        assert!(
            refusal(&mut s, with(job, extra)).contains(expected),
            "{job}"
        );
    }
    // Restart without the source file: inspect, export and the retained bytes.
    std::fs::remove_file(root.join("smpl.wav")).unwrap();
    drop(s);
    let mut s = Service::new(&root);
    let inspected = call(
        &mut s,
        "inspect_sample",
        json!({"plan": used["plan"], "detail": "full"}),
    );
    assert_eq!(inspected["report"], used["report"]);
    let delivered = export(&mut s, "delivered", &used, change_kinds(&used));
    assert_eq!(delivered.status, Status::Ok, "{delivered:?}");
    assert_eq!(
        std::fs::read(root.join("delivered/input.bin")).unwrap(),
        wav
    );
    let brr = std::fs::read(root.join("delivered/sample.bin")).unwrap();
    assert_eq!((brr.len(), brr[27] & 3), (36, 3));
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resampling_is_a_measured_change_and_scales_preserved_loops() {
    let root = workspace("resample");
    let mut s = Service::new(&root);
    let pcm = s16((0..64).map(|i| ((i % 32) * 500 - 8000) as i16));
    let source = load(&mut s, &root, "pcm", &pcm);
    let base = json!({"source":source,"encoding":"s16le","sample_rate":32000,"target":"brr"});
    let with = |job: &str, extra: Value| {
        let mut args = base.clone();
        args["job_id"] = json!(job);
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        args
    };
    let same = call(
        &mut s,
        "prepare_sample",
        with("same", json!({"target_sample_rate":32000})),
    );
    assert!(
        same["report"]["conversion"]["changes"]
            .get("resampled")
            .is_none()
    );
    let finite = call(
        &mut s,
        "prepare_sample",
        with("finite", json!({"target_sample_rate":16000})),
    );
    let conversion = &finite["report"]["conversion"];
    let resampled = &conversion["changes"]["resampled"];
    assert_eq!(
        (
            resampled["source_rate"].as_u64(),
            resampled["target_rate"].as_u64()
        ),
        (Some(32000), Some(16000))
    );
    assert_eq!(resampled["method"], "kaiser_windowed_sinc");
    assert_eq!(
        (
            conversion["source_samples"].as_u64(),
            conversion["target_samples"].as_u64()
        ),
        (Some(64), Some(32))
    );
    assert_eq!(conversion["target_sample_rate"], 16000);
    assert_eq!(
        conversion["tuning"]["snes_pitch"]["original_pitch"]["hex"],
        "0x0800"
    );
    let without: Vec<String> = change_kinds(&finite)
        .into_iter()
        .filter(|k| k != "resampled")
        .collect();
    assert_eq!(
        export(&mut s, "finite-partial", &finite, without).status,
        Status::Blocked
    );
    assert_eq!(
        export(&mut s, "finite-out", &finite, change_kinds(&finite)).status,
        Status::Ok
    );
    let target_wav = std::fs::read(root.join("finite-out/target.wav")).unwrap();
    assert_eq!(
        u32::from_le_bytes(target_wav[24..28].try_into().unwrap()),
        16000
    );
    // The preserved loop at sample 32 becomes sample 16 at half the rate.
    let looped = call(
        &mut s,
        "prepare_sample",
        with(
            "loop",
            json!({"target_sample_rate":16000,"loop_policy":"preserve","loop_offset":64}),
        ),
    );
    let conversion = &looped["report"]["conversion"];
    assert_eq!(conversion["loop"]["target_loop_start_sample"], 16);
    assert_eq!(conversion["loop"]["target_loop_offset"], 9);
    assert_eq!(
        conversion["changes"]["resampled"]["loop"]["target_start"],
        16
    );
    // The target preview holds first pass plus one traversal with a smpl loop.
    assert_eq!(
        export(&mut s, "loop-out", &looped, change_kinds(&looped)).status,
        Status::Ok
    );
    let preview = std::fs::read(root.join("loop-out/target.wav")).unwrap();
    let preview = tunemend::wav::parse(&preview).unwrap();
    let smpl = preview.smpl.unwrap().unwrap();
    assert_eq!(
        (preview.frames, smpl.loops[0].start, smpl.loops[0].end),
        (48, 32, 47)
    );
    for (job, extra, expected) in [
        (
            "brr-align",
            json!({"target_sample_rate":24000,"loop_policy":"preserve","loop_offset":64}),
            "16-sample-aligned",
        ),
        (
            "whole",
            json!({"target_sample_rate":44100,"loop_policy":"preserve","loop_offset":64}),
            "whole samples",
        ),
        (
            "limit",
            json!({"target_sample_rate":192000,"sample_rate":1}),
            "sample limit",
        ),
        (
            "range",
            json!({"target_sample_rate":0}),
            "target_sample_rate",
        ),
        ("note", json!({"root":{"note":128}}), "root needs note"),
    ] {
        assert!(
            refusal(&mut s, with(job, extra)).contains(expected),
            "{job}"
        );
    }
    // An explicit root is recorded; BRR gets the in-tune register.
    let rooted = call(
        &mut s,
        "prepare_sample",
        with("root", json!({"root":{"note":69,"cents":0}})),
    );
    let tuning = &rooted["report"]["conversion"]["tuning"];
    assert_eq!(
        tuning["root"],
        json!({"note": 69, "cents": 0.0, "source": "explicit"})
    );
    assert_eq!(tuning["snes_pitch"]["root_in_tune"]["value"], 4096);
    assert_eq!(rooted["report"]["root"], json!({"note": 69}));
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sample_reports_come_with_a_reading_guide_and_exports_name_their_files() {
    let root = std::env::temp_dir().join(format!("tunemend-sample-guide-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let pcm: Vec<u8> = (0..48i16)
        .flat_map(|i| ((i % 12) * 2000 - 11000).to_le_bytes())
        .collect();
    std::fs::write(root.join("raw"), &pcm).unwrap();
    let mut s = Service::new(&root);
    let source = call(
        &mut s,
        "load_instrument_source",
        json!({"job_id":"raw","path":"raw"}),
    );
    let prepared = call(
        &mut s,
        "prepare_sample",
        json!({"job_id":"loop","source":source["source"],"encoding":"s16le","sample_rate":32000,
            "target":"brr","loop_policy":"preserve","loop_offset":32}),
    );
    let guide = &prepared["report_guide"];
    let brr = &guide["brr_target"];
    assert_eq!(brr["blocks"], 3);
    assert_eq!(brr["first_block_filter_0"], true);
    assert_eq!(brr["loop_start_block"], 1);
    assert_eq!(brr["loop_block_filter_0"], true);
    assert_eq!(brr["end_block"], 2);
    assert_eq!(brr["end_flags"], 3);
    let keyed: u64 = guide["histograms"]["brr_target.filters"]
        .as_object()
        .unwrap()
        .values()
        .map(|n| n.as_u64().unwrap())
        .sum();
    assert_eq!(keyed, 3);
    assert!(
        guide["null_fields"]["length"]
            .as_str()
            .unwrap()
            .contains("end of the retained source")
    );
    // The guide is not stored: inspect derives the same one, and the plan
    // report is unchanged by it.
    let inspected = call(
        &mut s,
        "inspect_sample",
        json!({"plan": prepared["plan"], "detail": "full"}),
    );
    assert_eq!(inspected["report_guide"], *guide);
    assert!(prepared["report"].get("report_guide").is_none());
    let exported = call(
        &mut s,
        "export_sample",
        json!({"job_id":"export","plan":prepared["plan"],"destination":"out/brr","accepted_changes":["sample_values"]}),
    );
    let written = exported["files"]["sample.bin"].as_str().unwrap();
    assert!(
        std::path::Path::new(written).ends_with("out/brr/sample.bin"),
        "{written}"
    );
    assert!(std::path::Path::new(written).is_file());
    assert!(exported["files"]["report.json"].is_string());
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

/// A 44100 Hz mono C4 tone (261.63 Hz, with its second harmonic) of 22050
/// frames whose smpl loop starts at `start` and runs to the last frame.
fn looping_tone(start: u32) -> Vec<u8> {
    let frames: Vec<i16> = (0..22050)
        .map(|i| {
            let t = f64::from(i) / 44100.0;
            let w = std::f64::consts::TAU * 261.625_565_3 * t;
            (12000.0 * w.sin() + 4000.0 * (2.0 * w).sin()).round() as i16
        })
        .collect();
    let smpl = smpl_body(60, 0, &[(0, start, 22049, 0)]);
    build(1, 1, 44100, 16, None, &s16(frames), &[(b"smpl", smpl)])
}

#[test]
fn preserved_loops_are_diagnosed_and_unaligned_loops_name_fitting_starts_and_rates() {
    let root = workspace("loop-diagnostics");
    let mut s = Service::new(&root);
    let source = load(&mut s, &root, "tone.wav", &looping_tone(6713));
    // The source summary shows what prepare_sample will read.
    let summary = call(
        &mut s,
        "inspect_instrument_source",
        json!({"source": source}),
    );
    let facts = &summary["discovery"]["formats"][0]["wav"];
    assert_eq!(facts["sample_rate"], 44100);
    assert_eq!(facts["channels"], 1);
    assert_eq!(facts["bits_per_sample"], 16);
    assert_eq!(facts["frames"], 22050);
    assert_eq!(facts["duration_seconds"], 0.5);
    assert_eq!(facts["smpl"]["unity_note"], 60);
    let first = &facts["smpl"]["loops"][0];
    assert_eq!(first["start_frame"], 6713);
    assert_eq!(first["length_frames"], 15337);
    assert_eq!(first["smpl_loop"], "usable with loop_policy preserve");
    assert_eq!(first["aligned_without_resampling"]["brr_16_samples"], false);

    let prepare = |s: &mut Service, job: &str, source: &Value, rate: Option<u32>, target: &str| {
        let mut args = json!({"job_id": job, "source": source, "encoding": "wav",
            "sample_rate": 44100, "wav": {"smpl_loop": 0, "smpl_root": true},
            "loop_policy": "preserve", "target": target});
        if let Some(rate) = rate {
            args["target_sample_rate"] = json!(rate);
        }
        s.call("prepare_sample", args)
    };
    // At the source rate the loop does not fit BRR or PSX ADPCM blocks: the
    // refusal names fitting starts, whole-period starts and rates.
    for (target, expected) in [("brr", "16-sample-aligned"), ("vag", "28-sample-aligned")] {
        let refused = prepare(&mut s, target, &source, None, target);
        assert_eq!(refused.status, Status::Blocked, "{refused:?}");
        let message = &refused.diagnostics[0].message;
        for part in [
            expected,
            "Recoveries: start the loop at frame",
            "wav.loop_start_frame",
            "periods of its 261.63 Hz fundamental",
            "set target_sample_rate",
            "loop_policy=finite",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }
    }
    let message = prepare(&mut s, "brr-start", &source, None, "brr").diagnostics[0]
        .message
        .clone();
    assert!(
        message.contains("wav.loop_start_frame 6704 or 6720"),
        "{message}"
    );
    // A rate that does not scale the loop to whole samples names source frames.
    let message = prepare(&mut s, "brr-32000", &source, Some(32000), "brr").diagnostics[0]
        .message
        .clone();
    assert!(message.contains("must scale to whole samples"), "{message}");
    assert!(
        message.contains("multiples of 441 frames land on whole 16-sample blocks at 32000 Hz"),
        "{message}"
    );

    // 28800 Hz fits (multiples of 49 frames): 6713 spans 90.988 periods.
    let whole = prepare(&mut s, "whole", &source, Some(28800), "brr");
    assert_eq!(whole.status, Status::Ok, "{whole:?}");
    let d = &whole.result["report_guide"]["loop_diagnostics"];
    assert!(
        (d["fundamental_hz"].as_f64().unwrap() - 261.63).abs() < 0.05,
        "{d}"
    );
    assert_eq!(d["periods_in_loop"], 90.988);
    assert_eq!(d["loop_samples"], 10016);
    assert!(
        d["sustain_pitch"]["cents_from_root"]
            .as_f64()
            .unwrap()
            .abs()
            < 1.0
    );
    assert_eq!(d["flags"], json!([]), "{d}");
    assert!(d.get("whole_period_starts").is_none());

    // 49 frames earlier the loop spans 91.279 periods: the seam jumps in
    // phase, and the guide names 6713 as a whole-period start at this rate.
    let early = load(&mut s, &root, "early.wav", &looping_tone(6664));
    let jumped = prepare(&mut s, "jumped", &early, Some(28800), "brr");
    assert_eq!(jumped.status, Status::Ok, "{jumped:?}");
    let d = &jumped.result["report_guide"]["loop_diagnostics"];
    assert!(
        (d["phase_jump_periods"].as_f64().unwrap() - 0.279).abs() < 0.01,
        "{d}"
    );
    let flags = d["flags"].to_string();
    assert!(flags.contains("phase jumps"), "{flags}");
    assert!(flags.contains("waveform breaks at the seam"), "{flags}");
    assert!(
        d["seam"]["waveform_error"].as_f64().unwrap()
            > 10.0 * d["seam"]["waveform_error_inside_loop"].as_f64().unwrap()
    );
    let starts = &d["whole_period_starts"];
    assert_eq!(starts["set_with"], "wav.loop_start_frame");
    assert_eq!(starts["starts"][0]["source_start"], 6713, "{starts}");
    assert!(!starts["other_rates"].as_array().unwrap().is_empty());

    // inspect_sample is compact by default; detail full is the stored report.
    let plan = &jumped.result["plan"];
    let compact = call(&mut s, "inspect_sample", json!({"plan": plan}));
    assert!(compact.get("report").is_none());
    assert_eq!(
        compact["accepted_changes"],
        json!(change_kinds(&jumped.result))
    );
    assert_eq!(compact["loop_diagnostics"], *d);
    assert_eq!(compact["conversion"]["target_sample_rate"], 28800);
    let full = call(
        &mut s,
        "inspect_sample",
        json!({"plan": plan, "detail": "full"}),
    );
    assert_eq!(full["report"], jumped.result["report"]);
    assert_eq!(full["report_guide"], jumped.result["report_guide"]);
    assert!(compact.to_string().len() * 3 < full.to_string().len() * 2);
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}
