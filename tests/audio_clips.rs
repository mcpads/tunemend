//! Audio clips on voices: placement, gain and fades, trims, channels,
//! resampling, loops, excerpts, mixing, refusals and bundle round trips.
//! Synthetic WAVs only.
#![allow(clippy::needless_range_loop)]
mod common;

use serde_json::{Value, json};
use tunemend::sample_resample::stream_values;
use tunemend::service::{Service, Status, ToolResponse};
use tunemend::wav::build;

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "tunemend-audio-clips-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{tool}: {response:#?}");
    response
}

fn codes(response: &ToolResponse) -> Vec<(String, String)> {
    response
        .diagnostics
        .iter()
        .map(|d| {
            (
                serde_json::to_value(d.code)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned(),
                d.location.clone(),
            )
        })
        .collect()
}

/// Stereo PCM16 frames of a WAV written by Tunemend.
fn frames(path: &str) -> Vec<[i16; 2]> {
    let bytes = std::fs::read(path).unwrap();
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

/// What a render writes for a clip value `v` in PCM16 units.
fn written(v: f64) -> i16 {
    let x = (v / f64::from(i16::MAX)) as f32;
    (x.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// Deterministic test signal in PCM16 units.
fn signal(n: usize, seed: i32) -> Vec<i16> {
    (0..n as i32)
        .map(|i| (((i * 7919 + seed * 104_729) % 20_000) - 10_000) as i16)
        .collect()
}

/// A PCM16 WAV of interleaved `channels`, retained as a sound source.
fn source(
    service: &mut Service,
    root: &std::path::Path,
    name: &str,
    rate: u32,
    channels: &[Vec<i16>],
) -> String {
    let frames = channels[0].len();
    let data: Vec<u8> = (0..frames)
        .flat_map(|f| channels.iter().flat_map(move |c| c[f].to_le_bytes()))
        .collect();
    let wav = build(1, channels.len() as u16, rate, 16, None, &data, &[]);
    std::fs::write(root.join(name), wav).unwrap();
    ok(
        service,
        "load_instrument_source",
        json!({"job_id":format!("load-{name}"),"path":name}),
    )
    .result["source"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// 480 PPQ; 120 BPM, or 120 then 240 BPM from tick 960. Voice `fx` has no notes.
fn create(
    service: &mut Service,
    project: &str,
    end: u64,
    loop_start: Option<u64>,
    tempo_change: bool,
) {
    let mut tempo = vec![json!({"tick":0,"microseconds_per_quarter":500000})];
    if tempo_change {
        tempo.push(json!({"tick":960,"microseconds_per_quarter":250000}));
    }
    let mut playback = json!({"end": end});
    if let Some(start) = loop_start {
        playback["loop_start"] = json!(start);
    }
    ok(
        service,
        "create_project",
        json!({"job_id":format!("create-{project}"),"project":project,"timebase":{"kind":"music","ppq":480},
            "composition":{"tempo_map":tempo,"voices":[{"id":"fx"},{"id":"lead"}],
                "notes":[{"id":"n1","voice":"lead","start":0,"duration":240,"pitch":72,"velocity":90}],
                "playback":playback}}),
    );
}

fn edit(
    service: &mut Service,
    project: &str,
    base: &str,
    job: &str,
    operations: Value,
    extra: Value,
) -> ToolResponse {
    let mut request = json!({"job_id":job,"base_revision":base,"operations":operations});
    for (k, v) in extra.as_object().unwrap() {
        request[k] = v.clone();
    }
    service.call("edit_project", json!({"project":project,"request":request}))
}

fn clip(id: &str, source: &str, start: u64, duration: u64) -> Value {
    json!({"id":id,"voice":"fx","source":source,"start":start,"duration":duration,
        "channels":{"mode":"channel","channel":0}})
}

fn render(service: &mut Service, project: &str, job: &str, extra: Value) -> ToolResponse {
    let mut input = json!({"job_id":format!("render-{job}"),"project":project,"layout":{"kind":"full","loop_passes":1},"voices":["fx"]});
    for (k, v) in extra.as_object().unwrap() {
        input[k] = v.clone();
    }
    ok(service, "render_preview", input)
}

#[test]
fn a_clip_lands_on_its_exact_sample_after_a_tempo_change_with_gain_fades_and_trim() {
    let root = workspace("place");
    let mut s = Service::new(&root);
    let values = signal(4000, 1);
    let id = source(
        &mut s,
        &root,
        "mono.wav",
        44100,
        std::slice::from_ref(&values),
    );
    create(&mut s, "p", 2880, None, true);
    // Tick 1440 is 1 s + 480 ticks at 240 BPM = 1.25 s = sample 55125.
    let added = edit(
        &mut s,
        "p",
        "r1",
        "add",
        json!([{"op":"add_clip","clip":clip("c1",&id,1440,480)}]),
        json!({}),
    );
    assert_eq!(added.status, Status::Ok, "{added:#?}");
    assert_eq!(added.result["clips"][0]["source_frames"], 4000);
    assert_eq!(added.result["clips"][0]["source_exhausted"], true);
    let first = render(&mut s, "p", "plain", json!({}));
    let out = frames(&first.artifacts[0].path);
    assert!(out[..55125].iter().all(|f| f == &[0, 0]));
    for (k, v) in values.iter().enumerate() {
        assert_eq!(out[55125 + k], [*v, *v], "sample {k}");
    }
    assert!(out[55125 + 4000..].iter().all(|f| f == &[0, 0]));
    let report = &first.result["clips"]["played"][0];
    assert_eq!(report["placements"][0]["render_sample"], 55125);
    assert_eq!(report["rate"]["resampled"], false);
    assert_eq!(report["placements"][0]["source_exhausted"], true);
    assert_eq!(
        first.result["render"]["played_by"],
        json!([{"voice":"fx","kind":"audio_clip","source":id,"clip":"c1"}])
    );

    // Batch output must retain the same clip provenance/normalization as preview.
    let batch = ok(
        &mut s,
        "render_batch",
        json!({"job_id":"clip-report", "project":"p", "format":"float32"}),
    );
    assert_eq!(
        batch.result["spans"][0]["clips"],
        first.result["clips"]["played"]
    );
    assert!(batch.result["spans"][0]["clips"][0]["source_normalization"].is_object());

    // Gain -6.0 dB, 1 ms fades (45 samples at 44.1 kHz) and a trim of frames 100..1100.
    let shaped = edit(
        &mut s,
        "p",
        "r2",
        "shape",
        json!([{"op":"update_clip","clip":{"id":"c1",
        "gain_db":-6.0,"fade_in_ms":1.0,"fade_out_ms":1.0,"source_start":100,"source_end":1100}}]),
        json!({}),
    );
    assert_eq!(shaped.status, Status::Ok, "{shaped:#?}");
    let out = frames(&render(&mut s, "p", "shaped", json!({})).artifacts[0].path);
    let gain = 10f64.powf(-6.0 / 20.0);
    for k in 0..1000usize {
        let mut g = gain;
        if k < 45 {
            g *= k as f64 / 45.0;
        }
        if k + 45 >= 1000 {
            g *= (1000 - k) as f64 / 45.0;
        }
        let expected = written(f64::from(values[100 + k]) * g);
        assert_eq!(out[55125 + k], [expected, expected], "shaped sample {k}");
    }
    assert_eq!(out[55125], [0, 0]);
    assert!(out[55125 + 1000..].iter().all(|f| f == &[0, 0]));
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn channel_modes_overlap_summing_and_resampling_are_explicit_and_deterministic() {
    let root = workspace("channels");
    let mut s = Service::new(&root);
    let (left, right) = (signal(2000, 2), signal(2000, 3));
    let stereo = source(
        &mut s,
        &root,
        "stereo.wav",
        44100,
        &[left.clone(), right.clone()],
    );
    let mono = source(&mut s, &root, "mono.wav", 22050, &[signal(1000, 4)]);
    create(&mut s, "p", 1920, None, false);
    let with = |mode: Value| json!({"id":"s","voice":"fx","source":stereo,"start":0,"duration":960,"channels":mode});
    // Each mode replaces the clip; the render starts at sample 0.
    for (job, mode, expect) in [
        (
            "stereo",
            json!({"mode":"stereo"}),
            Box::new(|k: usize| [left[k], right[k]]) as Box<dyn Fn(usize) -> [i16; 2]>,
        ),
        (
            "pick",
            json!({"mode":"channel","channel":1}),
            Box::new(|k: usize| [right[k], right[k]]),
        ),
        (
            "mean",
            json!({"mode":"downmix"}),
            Box::new(|k: usize| {
                let m = written((f64::from(left[k]) + f64::from(right[k])) / 2.0);
                [m, m]
            }),
        ),
    ] {
        let base = format!(
            "r{}",
            if job == "stereo" {
                1
            } else if job == "pick" {
                2
            } else {
                3
            }
        );
        let op = if job == "stereo" {
            json!({"op":"add_clip","clip":with(mode)})
        } else {
            json!({"op":"update_clip","clip":{"id":"s","channels":mode}})
        };
        let r = edit(&mut s, "p", &base, job, json!([op]), json!({}));
        assert_eq!(r.status, Status::Ok, "{r:#?}");
        let out =
            frames(&render(&mut s, "p", &format!("render-{job}"), json!({})).artifacts[0].path);
        for k in 0..2000 {
            assert_eq!(out[k], expect(k), "{job} sample {k}");
        }
    }
    // A mono clip at 22050 Hz overlapping the stereo one sums after resampling.
    let r = edit(
        &mut s,
        "p",
        "r4",
        "mono",
        json!([{"op":"add_clip","clip":clip("m",&mono,0,960)}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok, "{r:#?}");
    let a = render(&mut s, "p", "sum", json!({}));
    let b = render(&mut s, "p", "sum-again", json!({}));
    assert_eq!(
        std::fs::read(&a.artifacts[0].path).unwrap(),
        std::fs::read(&b.artifacts[0].path).unwrap()
    );
    let played = &a.result["clips"]["played"];
    let rate = &played
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["clip"] == "m")
        .unwrap()["rate"];
    assert_eq!(rate["resampled"], true);
    assert_eq!(rate["ratio"], "2/1");
    assert_eq!(rate["method"], "kaiser_windowed_sinc");
    assert_eq!(rate["render_samples"], 2000);
    let mono_values: Vec<f64> = signal(1000, 4).iter().map(|v| f64::from(*v)).collect();
    let up = stream_values(&[&mono_values], 22050, 44100, 2000);
    let out = frames(&a.artifacts[0].path);
    for k in 0..2000 {
        let mean = (f64::from(left[k]) + f64::from(right[k])) / 2.0;
        // Each clip is converted separately and summed in f32 like the renderer.
        let sum = (mean / 32767.0) as f32 as f64 + up[0][k] / 32767.0;
        let expected = ((sum as f32).clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        assert_eq!(out[k], [expected, expected], "summed sample {k}");
    }
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn loop_passes_replay_loop_clips_and_excerpts_crop_the_full_render() {
    let root = workspace("loops");
    let mut s = Service::new(&root);
    let values = signal(30000, 5);
    let id = source(
        &mut s,
        &root,
        "long.wav",
        44100,
        std::slice::from_ref(&values),
    );
    let short = source(&mut s, &root, "short.wav", 44100, &[signal(500, 6)]);
    create(&mut s, "p", 1920, Some(960), false);
    // Intro clip at tick 0, loop clip at tick 960 (sample 44100), loop length 44100.
    let crossing = edit(
        &mut s,
        "p",
        "r1",
        "cross",
        json!([{"op":"add_clip","clip":clip("x",&id,480,960)}]),
        json!({}),
    );
    assert_eq!(
        codes(&crossing),
        vec![(
            "clip_crosses_loop_start".into(),
            "/result/mix/clips/0".into()
        )]
    );
    let beyond = edit(
        &mut s,
        "p",
        "r1",
        "beyond",
        json!([{"op":"add_clip","clip":clip("x",&id,960,1000)}]),
        json!({}),
    );
    assert_eq!(
        codes(&beyond),
        vec![(
            "event_beyond_end".into(),
            "/result/mix/clips/0/duration".into()
        )]
    );
    let r = edit(
        &mut s,
        "p",
        "r1",
        "add",
        json!([
        {"op":"add_clip","clip":clip("intro",&short,0,480)},
        {"op":"add_clip","clip":clip("loop",&id,960,960)}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok, "{r:#?}");
    let two = render(
        &mut s,
        "p",
        "two",
        json!({"layout":{"kind":"full","loop_passes":2}}),
    );
    let out = frames(&two.artifacts[0].path);
    for pass in 0..2 {
        for k in 0..30000 {
            let v = values[k];
            assert_eq!(
                out[44100 * (pass + 1) + k],
                [v, v],
                "pass {pass} sample {k}"
            );
        }
    }
    // The intro clip plays once.
    assert_ne!(out[10], [0, 0]);
    assert_eq!(&out[44100 * 2 - 100..44100 * 2], &vec![[0i16, 0]; 100][..]);
    let placements: Vec<(u64, u64)> = two.result["clips"]["played"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            c["placements"].as_array().unwrap().iter().map(|p| {
                (
                    p["pass"].as_u64().unwrap(),
                    p["render_sample"].as_u64().unwrap(),
                )
            })
        })
        .collect();
    assert_eq!(placements, vec![(1, 0), (1, 44100), (2, 88200)]);

    // An excerpt starting inside the loop clip equals the full render there.
    let full = frames(&render(&mut s, "p", "full", json!({})).artifacts[0].path);
    let excerpt = render(
        &mut s,
        "p",
        "excerpt",
        json!({"layout":{"kind":"excerpt","start":1200,"end":1800}}),
    );
    let cut = frames(&excerpt.artifacts[0].path);
    let shift = 1200 * 44100 / 960;
    assert!(cut.len() >= 27562);
    assert_eq!(cut[..27562], full[shift..shift + 27562]);
    assert_eq!(
        excerpt.result["clips"]["played"][0]["placements"][0]["first_clip_sample"],
        11025
    );
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clips_use_their_voice_strip_and_respect_scope_protections_and_sources() {
    let root = workspace("mix");
    let mut s = Service::new(&root);
    let values = signal(2000, 7);
    let id = source(
        &mut s,
        &root,
        "mono.wav",
        44100,
        std::slice::from_ref(&values),
    );
    std::fs::write(root.join("text.wav"), b"not a wav file").unwrap();
    let text = ok(
        &mut s,
        "load_instrument_source",
        json!({"job_id":"load-text","path":"text.wav"}),
    )
    .result["source"]
        .as_str()
        .unwrap()
        .to_owned();
    create(&mut s, "p", 1920, None, false);

    let unknown = edit(
        &mut s,
        "p",
        "r1",
        "unknown",
        json!([{"op":"add_clip","clip":clip("c",&"a".repeat(64),0,480)}]),
        json!({}),
    );
    assert_eq!(unknown.status, Status::Blocked);
    assert_eq!(
        codes(&unknown),
        vec![(
            "unknown_reference".into(),
            "/result/mix/clips/0/source".into()
        )]
    );
    assert!(
        unknown.diagnostics[0]
            .message
            .contains("load_instrument_source")
    );
    let not_wav = edit(
        &mut s,
        "p",
        "r1",
        "text",
        json!([{"op":"add_clip","clip":clip("c",&text,0,480)}]),
        json!({}),
    );
    assert_eq!(
        codes(&not_wav),
        vec![("invalid_audio".into(), "/result/mix/clips/0/source".into())]
    );
    let mut stereo = clip("c", &id, 0, 480);
    stereo["channels"] = json!({"mode":"stereo"});
    let wrong = edit(
        &mut s,
        "p",
        "r1",
        "stereo",
        json!([{"op":"add_clip","clip":stereo}]),
        json!({}),
    );
    assert_eq!(
        codes(&wrong),
        vec![("invalid_audio".into(), "/result/mix/clips/0".into())]
    );
    let trim = edit(
        &mut s,
        "p",
        "r1",
        "trim",
        json!([{"op":"add_clip","clip":{"id":"c","voice":"fx","source":id,"start":0,"duration":480,"source_end":3000,"channels":{"mode":"channel","channel":0}}}]),
        json!({}),
    );
    assert_eq!(
        codes(&trim),
        vec![("invalid_audio".into(), "/result/mix/clips/0".into())]
    );

    // Scope: the clip span is [480, 960).
    let add = json!([{"op":"add_clip","clip":clip("c",&id,480,480)}]);
    let outside = edit(
        &mut s,
        "p",
        "r1",
        "range",
        add.clone(),
        json!({"scope":{"range":{"start":0,"end":900}}}),
    );
    assert_eq!(
        codes(&outside),
        vec![("out_of_scope".into(), "/scope".into())]
    );
    let other = edit(
        &mut s,
        "p",
        "r1",
        "voices",
        add.clone(),
        json!({"scope":{"voices":["lead"]}}),
    );
    assert_eq!(
        codes(&other),
        vec![("out_of_scope".into(), "/scope".into())]
    );
    let protected = edit(
        &mut s,
        "p",
        "r1",
        "protected",
        add.clone(),
        json!({"protections":[{"voice":"fx","range":{"start":900,"end":1000}}]}),
    );
    assert_eq!(
        codes(&protected),
        vec![("protected_change".into(), "/protections/0".into())]
    );
    let inside = edit(
        &mut s,
        "p",
        "r1",
        "inside",
        add,
        json!({"scope":{"range":{"start":480,"end":960},"voices":["fx"]},
        "protections":[{"voice":"fx","range":{"start":0,"end":480}}]}),
    );
    assert_eq!(inside.status, Status::Ok, "{inside:#?}");
    assert_eq!(
        inside.result["difference"]["changed_mix"],
        json!(["clip `c` on voice `fx`"])
    );

    // Mute and solo silence the clip; pan, an effect and a send process it.
    let start = 22050;
    let dry = frames(&render(&mut s, "p", "dry", json!({})).artifacts[0].path);
    assert_eq!(dry[start + 5], [values[5], values[5]]);
    let r = edit(
        &mut s,
        "p",
        "r2",
        "mute",
        json!([{"op":"set_mix","target":{"kind":"voice","id":"fx"},"mute":true}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok);
    let muted = render(&mut s, "p", "muted", json!({"voices":null}));
    assert!(
        frames(&muted.artifacts[0].path)[start..start + 2000]
            .iter()
            .all(|f| f == &[0, 0])
    );
    assert_eq!(muted.result["clips"]["not_heard"], json!(["c"]));
    assert_eq!(muted.result["clips"]["played"], json!([]));
    let r = edit(
        &mut s,
        "p",
        "r3",
        "solo",
        json!([
        {"op":"set_mix","target":{"kind":"voice","id":"fx"},"mute":false},
        {"op":"set_mix","target":{"kind":"voice","id":"lead"},"solo":true}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok);
    let soloed = render(&mut s, "p", "soloed", json!({"voices":null}));
    assert!(
        frames(&soloed.artifacts[0].path)[start..start + 2000]
            .iter()
            .all(|f| f == &[0, 0])
    );
    let r = edit(
        &mut s,
        "p",
        "r4",
        "strip",
        json!([
        {"op":"set_mix","target":{"kind":"voice","id":"lead"},"solo":false},
        {"op":"set_mix","target":{"kind":"voice","id":"fx"},"pan":100.0},
        {"op":"insert_effect","target":{"kind":"voice","id":"fx"},"effect":{"id":"lim","processor":{"kind":"limiter","ceiling_db":-12.0,"release_ms":10.0,"lookahead_ms":1.0}}}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok, "{r:#?}");
    let out = frames(&render(&mut s, "p", "strip", json!({})).artifacts[0].path);
    let ceiling = (10f64.powf(-12.0 / 20.0) * 32767.0).ceil() as i16;
    assert!(
        out[start..start + 2000]
            .iter()
            .all(|f| f[0] == 0 && f[1].abs() <= ceiling)
    );
    assert!(out[start..start + 2000].iter().any(|f| f[1] != 0));
    let r = edit(
        &mut s,
        "p",
        "r5",
        "send",
        json!([
        {"op":"set_effects","target":{"kind":"voice","id":"fx"},"effects":[]},
        {"op":"set_mix","target":{"kind":"voice","id":"fx"},"pan":0.0,"volume_db":-120.0},
        {"op":"add_bus","bus":{"id":"b"}},
        {"op":"set_send","send":{"voice":"fx","bus":"b","pre_fader":true}}]),
        json!({"scope":{"buses":true}}),
    );
    assert_eq!(r.status, Status::Ok, "{r:#?}");
    let out = frames(&render(&mut s, "p", "send", json!({})).artifacts[0].path);
    for k in 0..2000 {
        assert!(
            (i32::from(out[start + k][0]) - i32::from(values[k])).abs() <= 1,
            "send sample {k}"
        );
    }

    // Inspection and comparison show the clip.
    let summary = ok(
        &mut s,
        "inspect_project",
        json!({"project":"p","view":"summary"}),
    );
    assert_eq!(summary.result["mix"]["clips"][0]["id"], "c");
    let events = ok(
        &mut s,
        "inspect_project",
        json!({"project":"p","view":"events","range":{"start":500,"end":600}}),
    );
    assert_eq!(events.result["clips"][0]["position"], "bar 1 beat 2");
    let compared = ok(
        &mut s,
        "compare_revisions",
        json!({"project":"p","from":"r1","to":"r2"}),
    );
    assert_eq!(compared.result["changed_clips"][0]["id"], "c");
    assert_eq!(compared.result["changed_clips"][0]["before"], Value::Null);
    let elsewhere = ok(
        &mut s,
        "compare_revisions",
        json!({"project":"p","from":"r1","to":"r2","range":{"start":0,"end":480}}),
    );
    // Compared categories are always listed, empty when nothing differs.
    assert_eq!(elsewhere.result["changed_clips"], json!([]));
    let removed = edit(
        &mut s,
        "p",
        "r6",
        "remove",
        json!([{"op":"remove_clip","id":"c"}]),
        json!({}),
    );
    assert_eq!(removed.status, Status::Ok);
    let missing = edit(
        &mut s,
        "p",
        "r7",
        "again",
        json!([{"op":"remove_clip","id":"c"}]),
        json!({}),
    );
    assert_eq!(
        codes(&missing),
        vec![("unknown_reference".into(), "/operations/0/id".into())]
    );
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

/// A looping BRR sample plan of a 32-sample sine.
fn sine_plan(service: &mut Service, root: &std::path::Path) -> String {
    let pcm: Vec<u8> = (0..64)
        .map(|i| (((i as f64) * std::f64::consts::TAU / 32.0).sin() * 12000.0) as i16)
        .flat_map(|v: i16| v.to_le_bytes())
        .collect();
    std::fs::write(root.join("pcm"), &pcm).unwrap();
    let source = ok(
        service,
        "load_instrument_source",
        json!({"job_id":"pcm","path":"pcm"}),
    )
    .result["source"]
        .clone();
    ok(
        service,
        "prepare_sample",
        json!({"job_id":"plan","source":source,"encoding":"s16le","sample_rate":32000,
        "target":"brr","loop_offset":64,"loop_policy":"preserve"}),
    )
    .result["plan"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn bundles_carry_clip_sources_and_instrument_data_into_a_fresh_workspace() {
    let root = workspace("bundle");
    let mut s = Service::new(&root);
    let id = source(&mut s, &root, "mono.wav", 22050, &[signal(3000, 8)]);
    let plan = sine_plan(&mut s, &root);
    std::fs::write(root.join("fm.bin"), vec![0x11; 40]).unwrap();
    let layout = json!({"record_offsets":[8],"record_size":32,"algorithm_offset":2,"operators_offset":4,
        "packing":"operator_rows","slot_order":[0,1,2,3],"reserved_records":[]});
    let bank = ok(
        &mut s,
        "load_fm_bank",
        json!({"job_id":"fm","path":"fm.bin","chip":"ym2612","layout":layout}),
    )
    .result["bank"]
        .as_str()
        .unwrap()
        .to_owned();
    create(&mut s, "p", 1920, None, false);
    let r = edit(
        &mut s,
        "p",
        "r1",
        "add",
        json!([
        {"op":"add_clip","clip":clip("c",&id,480,960)},
        {"op":"set_instrument","voice":"fx","instrument":{"kind":"fm","bank":bank,"index":0,
            "context":{"chip":"ym2612","clock_hz":7670454,"pan":"center"},"a4_hz":440.0,
            "velocity":{"kind":"fixed","carrier_tl_offset":0}}},
        {"op":"set_instrument","voice":"lead","instrument":{"kind":"snes_brr","plan":plan,
            "pitch":{"kind":"root","root_note":60,"root_register":4096},
            "envelope":{"kind":"registers","adsr1":0x8f,"adsr2":0xe0,"gain":0},
            "volume":{"left":64,"right":64},"velocity":{"kind":"ignore"}}}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok, "{r:#?}");
    let first = render(&mut s, "p", "one", json!({"voices":null}));
    let second = render(&mut s, "p", "two", json!({"voices":["fx"]}));
    assert_eq!(
        first.result["render"]["sound_source"],
        "reference-waveforms"
    );
    let kinds: Vec<&str> = first.result["render"]["played_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["fm", "snes_brr", "audio_clip"]);
    assert_eq!(
        first.result["render"]["played_by"][1]["source"],
        plan.as_str()
    );

    // MIDI cannot carry clips: the report says so and the file is unchanged.
    let r1 = ok(
        &mut s,
        "export_project",
        json!({"job_id":"x1","project":"p","revision":"r1","destination":"out/r1","render_audio":"none"}),
    );
    let full = ok(
        &mut s,
        "export_project",
        json!({"job_id":"x2","project":"p","destination":"out/full","render_audio":"listed","render_ids":["render-2"]}),
    );
    let midi = |r: &ToolResponse| {
        std::fs::read(&r.artifacts.iter().find(|a| a.kind == "midi").unwrap().path).unwrap()
    };
    assert_eq!(midi(&r1), midi(&full));
    assert!(root.join("out/full/renders/render-2.wav").exists());
    assert!(!root.join("out/full/renders/render-1.wav").exists());
    assert!(root.join("out/full/objects").join(&id).exists());
    let report: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/full/report.json")).unwrap()).unwrap();
    assert!(
        report["not_in_midi"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().starts_with("audio clips"))
    );
    assert_eq!(report["mix"]["clips"][0]["written_to_midi"], false);
    let collections: Vec<&str> = report["referenced_data"]["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["collection"].as_str().unwrap())
        .collect();
    assert_eq!(collections, ["fm_bank", "sample_plan", "instrument_source"]);
    for (args, location) in [
        (json!({"render_ids":["render-1"]}), "/render_ids"),
        (json!({"render_audio":"listed"}), "/render_ids"),
        (
            json!({"render_audio":"none","render_ids":["render-9"]}),
            "/render_ids",
        ),
    ] {
        let mut input = json!({"job_id":format!("bad-{location}-{args}"),"project":"p","destination":"out/bad"});
        for (k, v) in args.as_object().unwrap() {
            input[k] = v.clone();
        }
        let refused = s.call("export_project", input);
        assert_eq!(refused.diagnostics[0].location, location, "{refused:#?}");
    }
    let lean = ok(
        &mut s,
        "export_project",
        json!({"job_id":"x3","project":"p","destination":"out/lean","render_audio":"none","referenced_data":"omit"}),
    );
    assert!(!root.join("out/lean/objects").exists());
    assert_eq!(lean.result["render_audio"], "none");
    drop(s);

    // A fresh workspace refuses a bundle without the objects, naming them.
    let fresh = root.join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    let mut f = Service::new(&fresh);
    let refused = f.call("import_project", json!({"job_id":"lean","path":root.join("out/lean/p.project.json"),"missing_renders":"regenerate"}));
    assert_eq!(refused.status, Status::Blocked);
    assert_eq!(
        codes(&refused),
        vec![("unknown_reference".into(), "/path".into())]
    );
    assert!(
        refused.diagnostics[0]
            .message
            .contains("referenced_data=include"),
        "{refused:#?}"
    );
    // A bundle without references names the revision's reference.
    let mut bare: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/full/p.project.json")).unwrap())
            .unwrap();
    bare.as_object_mut().unwrap().remove("references");
    std::fs::write(
        root.join("out/full/bare.project.json"),
        serde_json::to_vec(&bare).unwrap(),
    )
    .unwrap();
    let refused = f.call("import_project", json!({"job_id":"bare","path":root.join("out/full/bare.project.json"),"missing_renders":"regenerate"}));
    assert_eq!(
        codes(&refused),
        vec![(
            "unknown_reference".into(),
            "/revisions/1/composition/mix/instruments/0/instrument/bank".into()
        )]
    );
    // The same bundle imports where its referenced data is retained.
    bare["project"]["name"] = json!("p2");
    std::fs::write(
        root.join("out/full/bare.project.json"),
        serde_json::to_vec(&bare).unwrap(),
    )
    .unwrap();
    let mut origin = Service::new(&root);
    ok(
        &mut origin,
        "import_project",
        json!({"job_id":"bare-here","path":root.join("out/full/bare.project.json"),"missing_renders":"regenerate"}),
    );
    drop(origin);
    // A tampered object is refused.
    let object = root.join("out/full/objects").join(&id);
    let original = std::fs::read(&object).unwrap();
    std::fs::write(&object, b"tampered").unwrap();
    let refused = f.call("import_project", json!({"job_id":"tampered","path":root.join("out/full/p.project.json"),"missing_renders":"regenerate"}));
    assert_eq!(
        codes(&refused),
        vec![("revision_content_mismatch".into(), "/path".into())]
    );
    std::fs::write(&object, original).unwrap();

    let imported = ok(
        &mut f,
        "import_project",
        json!({"job_id":"full","path":root.join("out/full/p.project.json"),"missing_renders":"regenerate"}),
    );
    assert_eq!(imported.result["regenerated_renders"], json!(["render-1"]));
    assert_eq!(
        imported.result["referenced_data"].as_array().unwrap().len(),
        3
    );
    let again = ok(
        &mut f,
        "render_preview",
        json!({"job_id":"again","project":"p","layout":{"kind":"full","loop_passes":1}}),
    );
    assert_eq!(
        std::fs::read(&again.artifacts[0].path).unwrap(),
        std::fs::read(&first.artifacts[0].path).unwrap()
    );
    // The carried records work for further edits and other tools.
    let r = edit(
        &mut f,
        "p",
        "r2",
        "move",
        json!([{"op":"update_clip","clip":{"id":"c","start":0}}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok, "{r:#?}");
    ok(&mut f, "inspect_sample", json!({"plan":plan}));
    let reexport = ok(
        &mut f,
        "export_project",
        json!({"job_id":"re","project":"p","destination":"out/re"}),
    );
    assert!(reexport.artifacts.iter().any(|a| a.kind == "object"));
    let _ = second;
    drop(f);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_backed_projects_play_clips_without_changing_source_bytes() {
    let root = workspace("source");
    let mut s = Service::new(&root);
    let id = source(&mut s, &root, "mono.wav", 44100, &[signal(5000, 9)]);
    let bundle = common::village();
    ok(
        &mut s,
        "create_project",
        json!({"job_id":"create","project":"village","timebase":bundle.project.timebase,
        "composition":bundle.revisions[0].composition}),
    );
    ok(
        &mut s,
        "export_project",
        json!({"job_id":"export","project":"village","destination":"out/source"}),
    );
    ok(
        &mut s,
        "import_midi",
        json!({"job_id":"import","project":"imported","path":"out/source/village.mid"}),
    );
    let summary = ok(
        &mut s,
        "inspect_project",
        json!({"view":"summary","project":"imported"}),
    );
    let voice = summary.result["voices"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut c = clip("c", &id, 960, 960);
    c["voice"] = json!(voice);
    let r = edit(
        &mut s,
        "imported",
        "r1",
        "add",
        json!([{"op":"add_clip","clip":c}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok, "{r:#?}");
    let full = ok(
        &mut s,
        "render_preview",
        json!({"job_id":"full","project":"imported","layout":{"kind":"full","loop_passes":1},"voices":[voice]}),
    );
    let excerpt = ok(
        &mut s,
        "render_preview",
        json!({"job_id":"excerpt","project":"imported","layout":{"kind":"excerpt","start":1200,"end":1800},"voices":[voice]}),
    );
    let again = ok(
        &mut s,
        "render_preview",
        json!({"job_id":"again","project":"imported","layout":{"kind":"excerpt","start":1200,"end":1800},"voices":[voice]}),
    );
    assert_eq!(
        std::fs::read(&excerpt.artifacts[0].path).unwrap(),
        std::fs::read(&again.artifacts[0].path).unwrap()
    );
    assert_eq!(full.result["clips"]["played"][0]["clip"], "c");
    assert_eq!(excerpt.result["clips"]["played"][0]["clip"], "c");
    let original = std::fs::read(root.join("out/source/village.mid")).unwrap();
    let delivered = ok(
        &mut s,
        "export_project",
        json!({"job_id":"deliver","project":"imported","destination":"out/delivered"}),
    );
    let midi = delivered
        .artifacts
        .iter()
        .find(|a| a.kind == "midi")
        .unwrap();
    assert_eq!(std::fs::read(&midi.path).unwrap(), original);
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
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

#[test]
fn bundles_carry_instrument_banks_for_bank_slot_voices() {
    let root = workspace("bank");
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
        module(0x5000, &[bank, sample].concat()),
    )
    .unwrap();
    std::fs::write(root.join("template.bin"), module(0x4000, &[0; 16])).unwrap();
    std::fs::write(root.join("driver.bin"), module(0x0400, &[1, 2, 3])).unwrap();
    std::fs::write(root.join("resident.bin"), module(0x2000, &resident)).unwrap();
    let mut s = Service::new(&root);
    let id = ok(
        &mut s,
        "load_instrument_bank",
        json!({"job_id":"bank","template":"template.bin","bank":"bank.bin","driver":"driver.bin",
        "resident":"resident.bin","layout":{"directory":0x5000,"slots":2,"tuning":0x4000,
        "envelopes":0x2000,"envelope_count":2,"sample_memory_end":0x6000}}),
    )
    .result["bank"]
        .clone();
    create(&mut s, "p", 1920, None, false);
    let r = edit(
        &mut s,
        "p",
        "r1",
        "assign",
        json!([{"op":"set_instrument","voice":"lead","instrument":{"kind":"snes_brr","bank":id,"slot":1,
        "pitch":{"kind":"fixed","register":0x0c00},"envelope":{"kind":"bank_table","index":0},
        "volume":{"left":100,"right":-50},"velocity":{"kind":"ignore"}}}]),
        json!({}),
    );
    assert_eq!(r.status, Status::Ok, "{r:#?}");
    let first = ok(
        &mut s,
        "render_preview",
        json!({"job_id":"one","project":"p","layout":{"kind":"full","loop_passes":1}}),
    );
    assert_eq!(first.result["render"]["played_by"][0]["kind"], "snes_brr");
    ok(
        &mut s,
        "export_project",
        json!({"job_id":"x","project":"p","destination":"out/p","render_audio":"none"}),
    );
    let fresh = root.join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    let mut f = Service::new(&fresh);
    let imported = ok(
        &mut f,
        "import_project",
        json!({"job_id":"i","path":root.join("out/p/p.project.json"),"missing_renders":"regenerate"}),
    );
    assert_eq!(imported.result["regenerated_renders"], json!(["render-1"]));
    assert_eq!(
        imported.result["referenced_data"][0]["collection"],
        "instrument_bank"
    );
    assert!(
        !imported.result["referenced_data"][0]["objects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    ok(&mut f, "inspect_instrument_bank", json!({"bank":id}));
    drop((s, f));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_span_rounded_up_to_whole_ticks_is_not_reported_as_exhausted() {
    let root = workspace("rounding");
    let mut s = Service::new(&root);
    // 1000 frames at 44100 Hz are 21.8 ticks at 120 BPM and 480 PPQ.
    let id = source(&mut s, &root, "hit.wav", 44100, &[signal(1000, 3)]);
    let loaded = ok(
        &mut s,
        "load_instrument_source",
        json!({"job_id":"load-hit.wav","path":"hit.wav"}),
    );
    assert_eq!(loaded.result["discovery"]["formats"][0]["format"], "wav");
    create(&mut s, "p", 1920, None, false);
    let added = edit(
        &mut s,
        "p",
        "r1",
        "add",
        json!([{"op":"add_clip","clip":clip("c1",&id,0,22)},
            {"op":"add_clip","clip":clip("c2",&id,960,23)}]),
        json!({}),
    );
    assert_eq!(added.status, Status::Ok, "{added:#?}");
    let facts = &added.result["clips"];
    assert_eq!(facts[0]["duration_for_audio"], 22);
    assert_eq!(facts[0]["source_exhausted"], false);
    assert_eq!(facts[1]["source_exhausted"], true);
    let played = &render(&mut s, "p", "rounding", json!({})).result["clips"]["played"];
    let exhausted = |clip: &Value| clip["placements"][0]["source_exhausted"].clone();
    assert_eq!(exhausted(&played[0]), false);
    assert_eq!(played[0]["placements"][0]["silent_samples"], 11);
    assert_eq!(exhausted(&played[1]), true);
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn wav_amplitude_is_explicit_and_preserves_legacy_revision_audio() {
    let root = workspace("normalization");
    let mut s = Service::new(&root);
    let values = vec![-32768, -32767, -16384, -1, 0, 1, 16384, 32767];
    let id = source(
        &mut s,
        &root,
        "edges.wav",
        44100,
        std::slice::from_ref(&values),
    );
    create(&mut s, "p", 480, None, false);
    let added = edit(
        &mut s,
        "p",
        "r1",
        "add",
        json!([
            {"op":"add_clip","clip":clip("c", &id, 0, 480)}
        ]),
        json!({}),
    );
    assert_eq!(added.status, Status::Ok);
    let legacy = render(&mut s, "p", "legacy", json!({"revision":"r2"}));
    let legacy_bytes = std::fs::read(&legacy.artifacts[0].path).unwrap();
    assert_eq!(frames(&legacy.artifacts[0].path)[0], [-32767; 2]);
    let updated = edit(
        &mut s,
        "p",
        "r2",
        "normalize",
        json!([
            {"op":"update_clip","clip":{"id":"c","normalization":"wav_amplitude"}}
        ]),
        json!({}),
    );
    assert_eq!(updated.status, Status::Ok, "{updated:#?}");
    drop(s);
    let mut s = Service::new(&root);
    let normalized = ok(
        &mut s,
        "render_batch",
        json!({"job_id":"normalized", "project":"p", "format":"float32", "mixdown":false, "stems":{"by":"voice", "voices":["fx"]}}),
    );
    let bytes = std::fs::read(&normalized.artifacts[0].path).unwrap();
    let wav = tunemend::wav::parse(&bytes).unwrap();
    assert_eq!(wav.bits, 32);
    for (frame, value) in wav.data.as_chunks::<8>().0.iter().zip(&values) {
        for channel in frame.as_chunks::<4>().0 {
            assert_eq!(f32::from_le_bytes(*channel), f32::from(*value) / 32768.0);
        }
    }
    assert_eq!(
        normalized.result["spans"][0]["clips"][0]["source_normalization"]["gain_from_normalized_wav"],
        1.0
    );
    let old = render(&mut s, "p", "old-again", json!({"revision":"r2"}));
    assert_eq!(std::fs::read(&old.artifacts[0].path).unwrap(), legacy_bytes);
    let reverted = edit(
        &mut s,
        "p",
        "r3",
        "revert",
        json!([
            {"op":"update_clip","clip":{"id":"c","normalization":"pcm16_codes"}}
        ]),
        json!({}),
    );
    assert_eq!(reverted.status, Status::Ok);
    let back = render(&mut s, "p", "back", json!({}));
    assert_eq!(
        std::fs::read(&back.artifacts[0].path).unwrap(),
        legacy_bytes
    );
}
