//! Chip instruments on project voices: edits, renders against the audition
//! tools, polyphony, layouts, refusals and delivery. Synthetic data only.
mod common;

use serde_json::{Value, json};
use tunemend::sample_resample::stream_values;
use tunemend::service::{Service, Status, ToolResponse};

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "tunemend-voice-instruments-{name}-{}",
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

/// Stereo PCM16 frames and the header rate of a WAV written by Tunemend.
fn wav(path: &str) -> (u32, Vec<[i16; 2]>) {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let frames = bytes[44..]
        .chunks(4)
        .map(|c| {
            [
                i16::from_le_bytes([c[0], c[1]]),
                i16::from_le_bytes([c[2], c[3]]),
            ]
        })
        .collect();
    (rate, frames)
}

/// What render_preview writes for a chip frame value resampled to `v`.
fn preview_value(v: f64) -> i16 {
    let x = (v / f64::from(i16::MAX)) as f32;
    (x.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// A looping BRR sample plan of a 32-sample sine; with `root`, C4 at 32 kHz.
fn sine_plan(service: &mut Service, root: &std::path::Path, rooted: bool) -> Value {
    let pcm: Vec<u8> = (0..64)
        .flat_map(|i| {
            { (((i as f64) * std::f64::consts::TAU / 32.0).sin() * 12000.0) as i16 }.to_le_bytes()
        })
        .collect();
    std::fs::write(root.join("pcm"), &pcm).unwrap();
    let source = ok(
        service,
        "load_instrument_source",
        json!({"job_id":"pcm","path":"pcm"}),
    )
    .result["source"]
        .clone();
    let mut request = json!({"job_id": if rooted {"rooted"} else {"plan"}, "source":source,"encoding":"s16le","sample_rate":32000,
        "target":"brr","loop_offset":64,"loop_policy":"preserve"});
    if rooted {
        request["root"] = json!({"note":60,"cents":0});
    }
    ok(service, "prepare_sample", request).result["plan"].clone()
}

/// 120 BPM at 480 PPQ: one tick is 1/960 s.
fn composition(notes: Value, end: u64, loop_start: Option<u64>) -> Value {
    let mut playback = json!({"end": end});
    if let Some(start) = loop_start {
        playback["loop_start"] = json!(start);
    }
    json!({"tempo_map":[{"tick":0,"microseconds_per_quarter":500000}],
        "voices":[{"id":"lead"},{"id":"pad"}], "notes": notes, "playback": playback})
}

fn create(service: &mut Service, project: &str, composition: Value) {
    ok(
        service,
        "create_project",
        json!({"job_id":format!("create-{project}"),"project":project,
            "timebase":{"kind":"music","ppq":480},"composition":composition}),
    );
}

fn note(id: &str, voice: &str, start: u64, duration: u64, pitch: u8) -> Value {
    json!({"id":id,"voice":voice,"start":start,"duration":duration,"pitch":pitch,"velocity":100})
}

fn snes(plan: &Value) -> Value {
    json!({"kind":"snes_brr","plan":plan,"pitch":{"kind":"root","root_note":60,"root_register":4096},
        "envelope":{"kind":"registers","adsr1":0x8f,"adsr2":0xe0,"gain":0},
        "volume":{"left":64,"right":64},"velocity":{"kind":"ignore"}})
}

fn assign(
    service: &mut Service,
    project: &str,
    base: &str,
    job: &str,
    voice: &str,
    instrument: Value,
) -> ToolResponse {
    service.call(
        "edit_project",
        json!({"project":project,"request":{"job_id":job,"base_revision":base,
            "operations":[{"op":"set_instrument","voice":voice,"instrument":instrument}]}}),
    )
}

fn preview(service: &mut Service, project: &str, job: &str, extra: Value) -> ToolResponse {
    let mut input =
        json!({"job_id":job,"project":project,"layout":{"kind":"full","loop_passes":1}});
    for (k, v) in extra.as_object().unwrap() {
        input[k] = v.clone();
    }
    ok(service, "render_preview", input)
}

#[test]
fn single_snes_note_equals_render_instrument_exactly_and_after_resampling() {
    let root = workspace("snes");
    let mut s = Service::new(&root);
    let plan = sine_plan(&mut s, &root, false);
    // A 0.5 s note (16000 DSP samples) in a 1 s piece.
    create(
        &mut s,
        "solo",
        composition(json!([note("n1", "lead", 0, 480, 72)]), 960, None),
    );
    ok(
        &mut s,
        "edit_project",
        json!({"project":"solo","request":{"job_id":"assign","base_revision":"r1",
        "operations":[{"op":"set_instrument","voice":"lead","instrument":snes(&plan)}]}}),
    );
    let audition = ok(
        &mut s,
        "render_instrument",
        json!({"job_id":"audition","plan":plan,
            "pitch":{"kind":"note","note":72,"root_note":60,"root_register":4096},
            "envelope":{"kind":"registers","adsr1":0x8f,"adsr2":0xe0,"gain":0},
            "key_on_samples":16000,"release_samples":64000}),
    );
    let (_, expected) = wav(&audition.artifacts[0].path);

    let at_dsp_rate = preview(
        &mut s,
        "solo",
        "p32",
        json!({"voices":["lead"],"sample_rate":32000}),
    );
    let (rate, frames) = wav(&at_dsp_rate.artifacts[0].path);
    assert_eq!(rate, 32000);
    assert!(frames.len() > 16000 && frames.len() < expected.len());
    assert_eq!(frames[..], expected[..frames.len()]);
    assert!(expected[frames.len()..].iter().all(|f| f == &[0, 0]));
    let report = &at_dsp_rate.result["instruments"]["voices"][0];
    assert_eq!(report["voice"], "lead");
    assert_eq!(report["conversion"]["resampled"], false);
    assert_eq!(report["chip_voices"]["first_notes"][0]["chip_voice"], 0);
    assert_eq!(
        report["chip_voices"]["first_notes"][0]["native_key_off"],
        16000
    );
    assert_eq!(report["instrument"]["kind"], "snes_brr");
    assert!(
        report["instrument"]["defaults_applied"]
            .as_array()
            .unwrap()
            .contains(&json!("max_voices=8"))
    );

    let resampled = preview(&mut s, "solo", "p44", json!({"voices":["lead"]}));
    let (rate, frames) = wav(&resampled.artifacts[0].path);
    assert_eq!(rate, 44100);
    let sides: Vec<Vec<f64>> = (0..2)
        .map(|side| expected.iter().map(|f| f64::from(f[side])).collect())
        .collect();
    let converted = stream_values(&[&sides[0], &sides[1]], 32000, 44100, frames.len());
    for (n, frame) in frames.iter().enumerate() {
        assert_eq!(
            *frame,
            [
                preview_value(converted[0][n]),
                preview_value(converted[1][n])
            ],
            "sample {n}"
        );
    }
    assert_eq!(
        resampled.result["instruments"]["voices"][0]["conversion"]["method"],
        "kaiser_windowed_sinc"
    );
    std::fs::remove_dir_all(root).unwrap();
}

fn fm_registers() -> Value {
    // Algorithm 7 with only named operator 4 audible; fast attack and release.
    json!({"algorithm_feedback":7,"operators":[[1,127,31,0,0,15,0],[1,127,31,0,0,15,0],[1,127,31,0,0,15,0],[1,0,31,0,0,15,0]]})
}

fn fm(chip: &str, clock: u32) -> Value {
    json!({"kind":"fm","registers":fm_registers(),"context":{"chip":chip,"clock_hz":clock,"pan":"center"},
        "a4_hz":440.0,"velocity":{"kind":"fixed","carrier_tl_offset":0}})
}

#[test]
fn single_fm_note_equals_render_fm_patch_after_the_same_resampling() {
    let root = workspace("fm");
    let mut s = Service::new(&root);
    create(
        &mut s,
        "solo",
        composition(json!([note("n1", "lead", 0, 480, 69)]), 960, None),
    );
    assert_eq!(
        assign(&mut s, "solo", "r1", "fm", "lead", fm("ym2612", 7_670_454)).status,
        Status::Ok
    );
    // 0.5 s at 7670454/144 Hz is 26633.5 samples: both round up to 26634.
    let audition = ok(
        &mut s,
        "render_fm_patch",
        json!({"job_id":"audition","registers":fm_registers(),"context":{"chip":"ym2612","clock_hz":7670454,"pan":"center"},
            "notes":[{"start_seconds":0.0,"key_on_seconds":0.5,"pitch":{"kind":"midi_note","note":69,"a4_hz":440.0}}],
            "tail_seconds":1.0,"destination":"out/audition"}),
    );
    assert_eq!(audition.result["notes"][0]["key_off_sample"], 26634);
    let (_, native) = wav(root.join("out/audition/audition.wav").to_str().unwrap());
    let rendered = preview(&mut s, "solo", "p44", json!({"voices":["lead"]}));
    let report = &rendered.result["instruments"]["voices"][0];
    assert_eq!(report["native_rate"]["exact"], "7670454/144");
    assert_eq!(
        report["chip_voices"]["first_notes"][0]["native_key_off"],
        26634
    );
    assert_eq!(report["chip"]["chip"], "ym2612");
    let (_, frames) = wav(&rendered.artifacts[0].path);
    let sides: Vec<Vec<f64>> = (0..2)
        .map(|side| native.iter().map(|f| f64::from(f[side])).collect())
        .collect();
    let (from, to) = (7_670_454, 144 * 44_100);
    let converted = stream_values(&[&sides[0], &sides[1]], from, to, frames.len());
    // Compare where the kernel reads only samples the audition rendered.
    let compared = (0..frames.len())
        .take_while(|&m| {
            tunemend::sample_resample::stream_source_samples(from, to, m + 1) <= native.len()
        })
        .count();
    assert!(compared > 44_100 / 2, "{compared}");
    for n in 0..compared {
        assert_eq!(
            frames[n],
            [
                preview_value(converted[0][n]),
                preview_value(converted[1][n])
            ],
            "sample {n}"
        );
    }
    assert!(frames[2000..20000].iter().any(|f| f[0].abs() > 1000));
    // check_project counts the note until the patch's release bound, which
    // covers the audible tail of the audition after key-off.
    let report = ok(
        &mut s,
        "check_project",
        json!({"project": "solo", "profile": {"name": "one", "max_simultaneous_notes": 1}}),
    )
    .result["report"]
        .clone();
    let checked = report["checked"][1].as_str().unwrap();
    assert!(checked.contains("(OPN release"), "{checked}");
    let ms: f64 = checked
        .split("`lead` ")
        .nth(1)
        .and_then(|rest| rest.split(" ms").next())
        .unwrap()
        .parse()
        .unwrap();
    let last = native.iter().rposition(|f| f != &[0, 0]).unwrap();
    let tail_ms = (last + 1 - 26634) as f64 * 144.0 / 7_670_454.0 * 1000.0;
    assert!(ms >= tail_ms, "{ms} {tail_ms}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn overlapping_notes_take_separate_chip_voices_up_to_the_declared_limit() {
    let root = workspace("poly");
    let mut s = Service::new(&root);
    let plan = sine_plan(&mut s, &root, true);
    let chord = json!([
        note("c", "lead", 0, 480, 60),
        note("e", "lead", 0, 480, 64),
        note("g", "lead", 120, 360, 67),
        note("next", "lead", 480, 240, 72)
    ]);
    create(&mut s, "chord", composition(chord, 960, None));
    let mut instrument = snes(&plan);
    instrument["pitch"] = json!({"kind":"plan_root"});
    assert_eq!(
        assign(&mut s, "chord", "r1", "a", "lead", instrument.clone()).status,
        Status::Ok
    );
    let rendered = preview(&mut s, "chord", "p", json!({}));
    let voices = &rendered.result["instruments"]["voices"][0]["chip_voices"];
    assert_eq!(voices["peak_simultaneous"], 3);
    let used: Vec<u64> = voices["first_notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["chip_voice"].as_u64().unwrap())
        .collect();
    // The note after the chord reuses the lowest free voice.
    assert_eq!(used, [0, 1, 2, 0]);
    // Two voices cannot hold a three-note chord.
    instrument["max_voices"] = json!(2);
    assert_eq!(
        assign(&mut s, "chord", "r2", "b", "lead", instrument).status,
        Status::Ok
    );
    let refused = s.call(
        "render_preview",
        json!({"job_id":"over","project":"chord","layout":{"kind":"full","loop_passes":1}}),
    );
    assert_eq!(refused.status, Status::Blocked);
    assert_eq!(
        codes(&refused),
        [("chip_voices_exceeded".into(), "/mix/instruments/0".into())]
    );
    // YM2203 has three FM channels.
    let four = json!([
        note("a", "lead", 0, 480, 60),
        note("b", "lead", 0, 480, 64),
        note("c", "lead", 0, 480, 67),
        note("d", "lead", 0, 480, 71)
    ]);
    create(&mut s, "fm-chord", composition(four, 960, None));
    assert_eq!(
        assign(
            &mut s,
            "fm-chord",
            "r1",
            "fm",
            "lead",
            fm("ym2203", 3_993_600)
        )
        .status,
        Status::Ok
    );
    let refused = s.call(
        "render_preview",
        json!({"job_id":"fm-over","project":"fm-chord","layout":{"kind":"full","loop_passes":1}}),
    );
    assert_eq!(codes(&refused)[0].0, "chip_voices_exceeded");
    assert!(
        refused.summary.contains("3 chip voices"),
        "{}",
        refused.summary
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn loop_passes_and_excerpts_render_reproducibly_beside_reference_voices() {
    let root = workspace("layouts");
    let mut s = Service::new(&root);
    let plan = sine_plan(&mut s, &root, false);
    let notes = json!([
        note("a", "lead", 0, 240, 60),
        note("b", "lead", 480, 240, 67),
        note("c", "lead", 960, 480, 64),
        note("p", "pad", 480, 960, 48)
    ]);
    create(&mut s, "loop", composition(notes, 1920, Some(480)));
    assert_eq!(
        assign(&mut s, "loop", "r1", "a", "lead", snes(&plan)).status,
        Status::Ok
    );
    let bytes = |response: &ToolResponse| std::fs::read(&response.artifacts[0].path).unwrap();
    let layouts = [
        json!({"layout":{"kind":"full","loop_passes":2}}),
        json!({"layout":{"kind":"excerpt","start":600,"end":1500}}),
        json!({"layout":{"kind":"full","loop_passes":2},"voices":["lead"]}),
    ];
    for (i, layout) in layouts.iter().enumerate() {
        let first = preview(&mut s, "loop", &format!("first-{i}"), layout.clone());
        let again = preview(&mut s, "loop", &format!("again-{i}"), layout.clone());
        assert_eq!(bytes(&first), bytes(&again), "layout {i}");
        assert_eq!(
            first.result["instruments"]["voices"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    // Pass 2 replays the loop: notes b and c sound again after 2 s.
    let two = preview(&mut s, "loop", "two", layouts[0].clone());
    let notes = two.result["instruments"]["voices"][0]["notes"].as_u64();
    assert_eq!(notes, Some(5));
    let (_, frames) = wav(&two.artifacts[0].path);
    let second_b = (44_100.0 * 2.5) as usize;
    assert!(
        frames[second_b..second_b + 4000]
            .iter()
            .any(|f| f[0].abs() > 500)
    );
    // The excerpt starts the chip at the excerpt: note c re-struck at 0.
    let excerpt = preview(&mut s, "loop", "ex", layouts[1].clone());
    let first_notes = &excerpt.result["instruments"]["voices"][0]["chip_voices"]["first_notes"];
    assert_eq!(first_notes[0]["key"], 67);
    assert_eq!(first_notes[0]["seconds"], 0.0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn assignments_check_references_scope_and_controls_and_leave_midi_unchanged() {
    let root = workspace("edits");
    let mut s = Service::new(&root);
    let plan = sine_plan(&mut s, &root, false);
    let notes = json!([note("a", "lead", 0, 480, 60), note("p", "pad", 0, 480, 48)]);
    let mut base = composition(notes, 960, None);
    base["control_lanes"] = json!([{"voice":"lead","kind":"pitch_bend","points":[{"at":0,"value":0},{"at":240,"value":4096}]},
        {"voice":"lead","kind":"program","points":[{"at":0,"value":80}]},
        {"voice":"lead","kind":"pitch_bend_range","points":[{"at":0,"value":2}]}]);
    create(&mut s, "song", base);
    let reference = preview(&mut s, "song", "reference", json!({}));
    let reference_bytes = std::fs::read(&reference.artifacts[0].path).unwrap();
    let blocked = |s: &mut Service, job: &str, instrument: Value| {
        let response = assign(s, "song", "r1", job, "lead", instrument);
        assert_eq!(response.status, Status::Blocked, "{job}");
        codes(&response)
    };
    let mut unknown = snes(&plan);
    unknown["plan"] = json!("0".repeat(64));
    assert_eq!(
        blocked(&mut s, "unknown-plan", unknown),
        [(
            "target_refused".into(),
            "/result/mix/instruments/0/instrument/plan".into()
        )]
    );
    let mut bank = fm("ym2612", 7_670_454);
    bank["registers"] = Value::Null;
    bank["bank"] = json!("1".repeat(64));
    bank["index"] = json!(0);
    assert_eq!(
        blocked(&mut s, "unknown-bank", bank)[0].0,
        "unknown_reference"
    );
    let mut no_root = snes(&plan);
    no_root["pitch"] = json!({"kind":"plan_root"});
    assert_eq!(
        blocked(&mut s, "no-root", no_root)[0].1,
        "/result/mix/instruments/0/instrument/pitch"
    );
    let mut family = fm("ym2612", 7_670_454);
    family["context"]["chip"] = Value::Null;
    assert_eq!(
        blocked(&mut s, "no-chip", family)[0].1,
        "/result/mix/instruments/0/instrument/context"
    );
    let mut extra = snes(&plan);
    extra["loop"] = json!(true);
    assert_ne!(
        assign(&mut s, "song", "r1", "extra", "lead", extra).status,
        Status::Ok,
        "unknown instrument fields refuse"
    );
    let mut too_many = fm("ym2203", 3_993_600);
    too_many["max_voices"] = json!(4);
    assert_eq!(
        blocked(&mut s, "too-many", too_many)[0].0,
        "value_out_of_range"
    );
    let unknown_voice = s.call(
        "edit_project",
        json!({"project":"song","request":{"job_id":"ghost","base_revision":"r1",
            "operations":[{"op":"set_instrument","voice":"ghost","instrument":snes(&plan)}]}}),
    );
    assert_eq!(unknown_voice.status, Status::Blocked);
    // The assignment is voice data for the whole span: a range scope or a
    // protection of the voice blocks it; another voice's scope does not allow it.
    for (job, scope, protections) in [
        ("range", json!({"range":{"start":0,"end":480}}), json!([])),
        ("protected", json!({}), json!([{"voice":"lead"}])),
        ("other", json!({"voices":["pad"]}), json!([])),
    ] {
        let response = s.call(
            "edit_project",
            json!({"project":"song","request":{"job_id":job,"base_revision":"r1","scope":scope,"protections":protections,
                "operations":[{"op":"set_instrument","voice":"lead","instrument":snes(&plan)}]}}),
        );
        assert_eq!(response.status, Status::Blocked, "{job}");
    }

    let edited = ok(
        &mut s,
        "edit_project",
        json!({"project":"song","request":{"job_id":"assign","base_revision":"r1","scope":{"voices":["lead"]},
            "operations":[{"op":"set_instrument","voice":"lead","instrument":snes(&plan)}]}}),
    );
    assert_eq!(
        edited.result["difference"]["changed_mix"],
        json!(["instrument of voice `lead`"])
    );
    let summary = ok(
        &mut s,
        "inspect_project",
        json!({"view":"summary","project":"song"}),
    );
    assert_eq!(summary.result["mix"]["instruments"][0]["voice"], "lead");
    assert_eq!(
        summary.result["mix"]["instruments"][0]["instrument"]["kind"],
        "snes_brr"
    );
    let rendered = preview(&mut s, "song", "chip", json!({}));
    let report = &rendered.result["instruments"]["voices"][0];
    assert_eq!(report["not_applied"]["pitch_bend"], 2);
    assert_eq!(report["not_applied"]["program"], 1);
    assert_ne!(
        std::fs::read(&rendered.artifacts[0].path).unwrap(),
        reference_bytes
    );

    // MIDI keeps notes and program numbers; the report lists the instrument.
    let plain = ok(
        &mut s,
        "export_project",
        json!({"job_id":"x1","project":"song","revision":"r1","destination":"out/r1"}),
    );
    let chip = ok(
        &mut s,
        "export_project",
        json!({"job_id":"x2","project":"song","revision":"r2","destination":"out/r2"}),
    );
    let midi = |r: &ToolResponse| {
        let artifact = r.artifacts.iter().find(|a| a.kind == "midi").unwrap();
        std::fs::read(&artifact.path).unwrap()
    };
    assert_eq!(midi(&plain), midi(&chip));
    let report: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/r2/report.json")).unwrap()).unwrap();
    assert!(
        report["not_in_midi"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap().contains("chip instruments"))
    );
    assert_eq!(report["mix"]["instruments"][0]["voice"], "lead");
    assert_eq!(report["mix"]["instruments"][0]["written_to_midi"], false);

    // Clearing restores the reference-only revision byte for byte.
    let cleared = ok(
        &mut s,
        "edit_project",
        json!({"project":"song","request":{"job_id":"clear","base_revision":"r2",
            "operations":[{"op":"clear_instrument","voice":"lead"}]}}),
    );
    assert_eq!(cleared.result["revision"], "r3");
    let bundle = |s: &mut Service, revision: &str| {
        ok(
            s,
            "inspect_project",
            json!({"view":"summary","project":"song","revision":revision}),
        )
        .result["mix"]
            .clone()
    };
    assert_eq!(bundle(&mut s, "r3"), bundle(&mut s, "r1"));
    let again = preview(&mut s, "song", "cleared", json!({"revision":"r3"}));
    assert_eq!(
        std::fs::read(&again.artifacts[0].path).unwrap(),
        reference_bytes
    );
    let missing = s.call(
        "edit_project",
        json!({"project":"song","request":{"job_id":"clear-again","base_revision":"r3",
            "operations":[{"op":"clear_instrument","voice":"lead"}]}}),
    );
    assert_eq!(missing.status, Status::Blocked);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn composition_without_instruments_serializes_and_renders_as_before() {
    use tunemend::composition::Timebase;
    use tunemend::edit::{EditRequest, apply_edit};
    let bundle = common::village();
    let bgm = bundle.revisions[0].composition.clone();
    let text = serde_json::to_string(&bgm).unwrap();
    assert!(!text.contains("instruments"));
    let request: EditRequest = serde_json::from_value(json!({"job_id":"j","base_revision":"r1",
        "operations":[{"op":"set_instrument","voice":"bass","instrument":{"kind":"fm","registers":fm_registers(),
            "context":{"chip":"ym2612","clock_hz":7670454,"pan":"center"},"a4_hz":440.0,"velocity":{"kind":"scale_carrier_tl","range":24}}},
            {"op":"clear_instrument","voice":"bass"}]}))
    .unwrap();
    let restored = apply_edit(Timebase::Music { ppq: 480 }, &bgm, &request).unwrap();
    assert_eq!(serde_json::to_string(&restored).unwrap(), text);
}

#[test]
fn source_backed_projects_play_instruments_without_changing_source_bytes() {
    let root = workspace("source");
    let mut s = Service::new(&root);
    let plan = sine_plan(&mut s, &root, false);
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
    assert_eq!(
        assign(&mut s, "imported", "r1", "a", &voice, snes(&plan)).status,
        Status::Ok
    );
    for (job, layout) in [
        ("full", json!({"kind":"full","loop_passes":1})),
        ("excerpt", json!({"kind":"excerpt","start":1000,"end":4000})),
    ] {
        let first = preview(&mut s, "imported", job, json!({"layout":layout}));
        let again = preview(
            &mut s,
            "imported",
            &format!("{job}-again"),
            json!({"layout":layout}),
        );
        assert_eq!(
            std::fs::read(&first.artifacts[0].path).unwrap(),
            std::fs::read(&again.artifacts[0].path).unwrap()
        );
        assert_eq!(first.result["instruments"]["voices"][0]["voice"], voice);
    }
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
    std::fs::remove_dir_all(root).unwrap();
}

/// Lanes for CC7, CC11 and CC10 on `lead`, as composition control lanes.
fn channel_lanes(composition: &mut Value) {
    composition["control_lanes"] = json!([
        {"voice":"lead","kind":"volume","points":[{"at":0,"value":127},{"at":240,"value":64}]},
        {"voice":"lead","kind":"expression","points":[{"at":0,"value":127},{"at":360,"value":100}]},
        {"voice":"lead","kind":"pan","points":[{"at":0,"value":64},{"at":420,"value":0}]}
    ]);
}

#[test]
fn channel_controls_policy_applies_cc7_cc11_and_cc10_only_when_declared() {
    let root = workspace("channel-controls");
    let mut s = Service::new(&root);
    let plan = sine_plan(&mut s, &root, false);
    let mut piece = composition(json!([note("n1", "lead", 0, 480, 72)]), 960, None);
    channel_lanes(&mut piece);
    create(&mut s, "cc", piece);
    // r2: ignore (default, not written); r3: level; r4: level_and_pan.
    assert_eq!(
        assign(&mut s, "cc", "r1", "ignore", "lead", snes(&plan)).status,
        Status::Ok
    );
    let summary = ok(
        &mut s,
        "inspect_project",
        json!({"view":"summary","project":"cc"}),
    );
    let assigned = &summary.result["mix"]["instruments"][0]["instrument"];
    assert!(
        assigned.get("channel_controls").is_none(),
        "the default is not serialized: {assigned}"
    );
    for (base, job, policy) in [("r2", "level", "level"), ("r3", "pan", "level_and_pan")] {
        let mut instrument = snes(&plan);
        instrument["channel_controls"] = json!(policy);
        let edited = assign(&mut s, "cc", base, job, "lead", instrument);
        assert_eq!(edited.status, Status::Ok, "{edited:#?}");
        assert_eq!(
            edited.result["difference"]["changed_mix"],
            json!(["instrument of voice `lead`"])
        );
    }
    let render = |s: &mut Service, revision: &str| {
        let response = preview(
            s,
            "cc",
            &format!("render-{revision}"),
            json!({"revision":revision,"sample_rate":32000}),
        );
        let report = response.result["instruments"]["voices"][0].clone();
        (wav(&response.artifacts[0].path).1, report)
    };
    let (plain, plain_report) = render(&mut s, "r2");
    let (level, level_report) = render(&mut s, "r3");
    let (panned, panned_report) = render(&mut s, "r4");
    assert_eq!(plain_report["not_applied"]["cc7 volume"], 2);
    assert!(plain_report.get("channel_controls").is_none());
    assert_eq!(
        level_report["channel_controls"]["applied"],
        json!({"cc11 expression": 2, "cc7 volume": 2})
    );
    assert_eq!(level_report["not_applied"]["cc10 pan"], 2);
    assert!(panned_report["not_applied"].get("cc10 pan").is_none());

    // 120 BPM at 480 PPQ: tick 240 is 0.25 s (sample 8000), 360 is 12000,
    // 420 is 14000. CC7 64 then CC11 100 attenuate by (v/127)² each.
    let gain = |n: usize| match n {
        0..8000 => 1.0,
        8000..12000 => (64.0f64 / 127.0).powi(2),
        _ => (64.0f64 / 127.0 * 100.0 / 127.0).powi(2),
    };
    let mut compared = 0;
    for n in 0..14000 {
        for side in 0..2 {
            let expected = f64::from(plain[n][side]) * gain(n);
            assert!(
                (f64::from(level[n][side]) - expected).abs() <= 1.0,
                "level side {side} sample {n}: {} vs {expected}",
                level[n][side]
            );
            // Below CC10 change, equal-power pan puts cos 45° on each side.
            let centred = expected * std::f64::consts::FRAC_1_SQRT_2;
            assert!(
                (f64::from(panned[n][side]) - centred).abs() <= 1.0,
                "pan side {side} sample {n}: {} vs {centred}",
                panned[n][side]
            );
        }
        compared += usize::from(plain[n][0] != 0);
    }
    assert!(compared > 10000, "{compared}");
    // From sample 14000 CC10 0 is hard left.
    assert!(panned[14000..].iter().all(|f| f[1] == 0));
    assert!(panned[14000..16000].iter().any(|f| f[0] != 0));

    // FM, resampled to 44100 Hz: full CC7/CC11 lanes leave the output exactly
    // as with the policy off; CC10 0 from the start silences the right side.
    let mut fm_piece = composition(json!([note("n1", "lead", 0, 480, 69)]), 960, None);
    fm_piece["control_lanes"] = json!([
        {"voice":"lead","kind":"volume","points":[{"at":0,"value":127}]},
        {"voice":"lead","kind":"expression","points":[{"at":0,"value":127}]},
        {"voice":"lead","kind":"pan","points":[{"at":0,"value":0}]}
    ]);
    create(&mut s, "fm-cc", fm_piece);
    let mut base = "r1".to_owned();
    let mut renders = Vec::new();
    for (job, policy) in [
        ("fm-off", None),
        ("fm-level", Some("level")),
        ("fm-pan", Some("level_and_pan")),
    ] {
        let mut instrument = fm("ym2612", 7_670_454);
        if let Some(policy) = policy {
            instrument["channel_controls"] = json!(policy);
        }
        let edited = assign(&mut s, "fm-cc", &base, job, "lead", instrument);
        assert_eq!(edited.status, Status::Ok, "{edited:#?}");
        base = edited.result["revision"].as_str().unwrap().to_owned();
        let response = preview(
            &mut s,
            "fm-cc",
            &format!("render-{job}"),
            json!({"revision": base}),
        );
        renders.push(wav(&response.artifacts[0].path).1);
    }
    assert_eq!(renders[0], renders[1]);
    assert!(renders[2].iter().all(|f| f[1] == 0));
    assert!(renders[2].iter().any(|f| f[0].abs() > 1000));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn check_project_counts_chip_instrument_releases_against_the_voice_budget() {
    let root = workspace("budget");
    let mut s = Service::new(&root);
    let plan = sine_plan(&mut s, &root, false);
    // `a` and `c` end at 480 where `b` starts on `lead` and `p` on `pad`.
    let notes = json!([
        note("a", "lead", 0, 480, 60),
        note("c", "lead", 0, 480, 64),
        note("b", "lead", 480, 480, 67),
        note("p", "pad", 480, 480, 48)
    ]);
    create(&mut s, "budget", composition(notes, 960, None));
    let check = |s: &mut Service, revision: &str, profile: Value, reserved: Option<u32>| {
        let mut input = json!({"project": "budget", "revision": revision, "profile": profile});
        if let Some(reserved) = reserved {
            input["reserved_for_effects"] = json!(reserved);
        }
        ok(s, "check_project", input).result["report"].clone()
    };
    let two = json!({"name": "two", "max_simultaneous_notes": 2});
    // Without an instrument notes end at their end: two at a time.
    let plain = check(&mut s, "r1", two.clone(), None);
    assert_eq!(plain["passed"], true, "{plain}");
    assert!(!plain["checked"].to_string().contains("release"));
    assert_eq!(
        ok(
            &mut s,
            "check_project",
            json!({"project": "budget", "profile": "snes-dsp-voices"})
        )
        .result["report"]["passed"],
        true
    );
    assert_eq!(
        assign(&mut s, "budget", "r1", "snes", "lead", snes(&plan)).status,
        Status::Ok
    );
    // With the S-DSP instrument, `b` re-keys the chip voice of `a`, cutting
    // its release, but `c`'s release on the second chip voice still sounds.
    let counted = check(&mut s, "r2", two.clone(), None);
    assert_eq!(counted["passed"], false, "{counted}");
    let violation = &counted["violations"][0];
    assert_eq!(violation["rule"], "max_simultaneous_notes");
    assert_eq!(violation["at"], 480);
    assert_eq!(violation["notes"], json!(["c", "b", "p"]));
    assert_eq!(
        violation["message"],
        "3 notes sound together where 2 are allowed, 1 of them in a chip-instrument release"
    );
    assert!(
        counted["checked"]
            .to_string()
            .contains("`lead` 8.06 ms after each note (S-DSP release"),
        "{counted}"
    );
    assert!(
        counted["unchecked"]
            .to_string()
            .contains("release tails of voices without a chip instrument")
    );
    // A declared release bound replaces the envelope's.
    let cut = check(
        &mut s,
        "r2",
        json!({"name": "cut", "max_simultaneous_notes": 2, "chip_release_ms": 0}),
        None,
    );
    assert_eq!(cut["passed"], true, "{cut}");
    assert!(
        cut["checked"]
            .to_string()
            .contains("declared chip_release_ms")
    );
    // The built-in S-DSP budget has 8 voices; reserving 6 for sound effects
    // leaves 2 for the music.
    let snes_budget = check(&mut s, "r2", json!("snes-dsp-voices"), None);
    assert_eq!(snes_budget["profile"], "snes-dsp-voices");
    assert_eq!(snes_budget["passed"], true, "{snes_budget}");
    assert!(
        snes_budget["checked"]
            .to_string()
            .contains("at most 8 simultaneous notes")
    );
    let reserved = check(&mut s, "r2", json!("snes-dsp-voices"), Some(6));
    assert_eq!(reserved["passed"], false);
    assert!(
        reserved["checked"]
            .to_string()
            .contains("at most 2 simultaneous notes")
    );
    let unknown = s.call(
        "check_project",
        json!({"project": "budget", "profile": "snes"}),
    );
    assert_eq!(unknown.status, Status::Blocked);
    assert!(
        unknown.diagnostics[0]
            .message
            .contains("built-in profiles are test-four-channel, snes-dsp-voices")
    );
    // render_batch says what played, as render_preview does.
    let batch = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "batch", "project": "budget"}),
    );
    assert!(
        batch.result["limits"]
            .as_str()
            .unwrap()
            .contains("instrument voices use Tunemend's chip models"),
        "{}",
        batch.result["limits"]
    );
    // The allocation lists the first notes and counts every note.
    let many: Vec<Value> = (0..10)
        .map(|i| note(&format!("n{i}"), "lead", i * 96, 96, 60))
        .collect();
    create(&mut s, "many", composition(json!(many), 960, None));
    assert_eq!(
        assign(&mut s, "many", "r1", "many-snes", "lead", snes(&plan)).status,
        Status::Ok
    );
    let rendered = preview(&mut s, "many", "many-preview", json!({}));
    let voices = &rendered.result["instruments"]["voices"][0]["chip_voices"];
    assert_eq!(voices["first_notes"].as_array().unwrap().len(), 8);
    assert_eq!(voices["first_notes_listed"], "the first 8 of 10 notes");
    assert_eq!(voices["notes_per_chip_voice"][0], 10);
    std::fs::remove_dir_all(root).unwrap();
}
