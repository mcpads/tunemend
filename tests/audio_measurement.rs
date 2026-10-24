//! Audio measurement and comparison through the shared service: stored
//! renders, workspace WAV files, ranges and refusals.

mod common;

use serde_json::{Value, json};
use tunemend::measure::wav::{SampleEncoding, encode};
use tunemend::service::{Service, Status, ToolResponse};

struct Workspace {
    path: std::path::PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tunemend-measure-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.path.join(name), bytes).unwrap();
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn ok(response: ToolResponse) -> Value {
    assert_eq!(response.status, Status::Ok, "{response:#?}");
    response.result
}

fn refusal(response: &ToolResponse) -> (String, String) {
    assert_eq!(response.status, Status::Blocked, "{response:#?}");
    let d = &response.diagnostics[0];
    (
        serde_json::to_value(d.code)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned(),
        d.location.clone(),
    )
}

fn sine(rate: u32, hz: f64, amplitude: f64, seconds: f64) -> Vec<f32> {
    (0..(seconds * f64::from(rate)) as usize)
        .map(|i| {
            (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin())
                as f32
        })
        .collect()
}

#[test]
fn a_workspace_wav_is_measured_in_full_and_in_a_range() {
    let workspace = Workspace::new("file");
    let rate = 48_000;
    // 4 s at -23 dBFS then 4 s at -33 dBFS, both channels, 24-bit.
    let mut tone = sine(rate, 1000.0, 10f64.powf(-23.0 / 20.0), 4.0);
    tone.extend(sine(rate, 1000.0, 10f64.powf(-33.0 / 20.0), 4.0));
    workspace.write(
        "steps.wav",
        &encode(&[tone.clone(), tone], rate, SampleEncoding::PcmS24),
    );
    let mut service = Service::new(&workspace.path);
    let whole = ok(service.call(
        "measure_audio",
        json!({"audio": {"path": "steps.wav"}, "per_channel": true}),
    ));
    assert_eq!(whole["input"]["format"]["encoding"], "pcm_s24");
    assert_eq!(whole["frames"], 384_000);
    assert!((whole["levels"]["sample_peak_dbfs"].as_f64().unwrap() + 23.0).abs() < 0.01);
    // The -33 dBFS half is within the -10 LU relative gate of the -23 half.
    let integrated = whole["loudness"]["integrated_lufs"].as_f64().unwrap();
    assert!(integrated < -25.0 && integrated > -27.0, "{integrated}");
    assert!((whole["loudness"]["momentary_max_lufs"].as_f64().unwrap() + 23.0).abs() < 0.1);
    assert_eq!(whole["channels"].as_array().unwrap().len(), 2);
    assert!(!whole["spectrum"]["levels"].as_array().unwrap().is_empty());

    let later = ok(service.call(
        "measure_audio",
        json!({"audio": {"path": "steps.wav"}, "range": {"start_seconds": 4.5, "end_seconds": 7.5}, "spectrum": "octave"}),
    ));
    assert_eq!(later["input"]["range"]["start_frame"], 216_000);
    assert_eq!(later["frames"], 144_000);
    assert!((later["loudness"]["integrated_lufs"].as_f64().unwrap() + 33.0).abs() < 0.1);
    assert_eq!(later["spectrum"]["levels"].as_array().unwrap().len(), 10);
    // Reads never create storage.
    assert!(!workspace.path.join(".tunemend").exists());
}

#[test]
fn stored_renders_are_measured_and_compared_across_revisions() {
    let workspace = Workspace::new("renders");
    let mut service = Service::new(&workspace.path);
    let bundle = common::village();
    ok(service.call(
        "create_project",
        json!({"job_id": "create", "project": "village", "timebase": bundle.project.timebase,
               "composition": bundle.revisions[0].composition}),
    ));
    let mut request =
        serde_json::to_value(bundle.revisions[1].edit_request.as_ref().unwrap()).unwrap();
    request["job_id"] = json!("smooth");
    request["base_revision"] = json!("r1");
    request.as_object_mut().unwrap().remove("feedback");
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": request}),
    ));
    // Bars 3–4 hold the revised accompaniment.
    let layout = json!({"kind": "excerpt", "start": 3840, "end": 7680});
    let first = ok(service.call(
        "render_preview",
        json!({"job_id": "render-r1", "project": "village", "revision": "r1", "layout": layout}),
    ));
    let second = ok(service.call(
        "render_preview",
        json!({"job_id": "render-r2", "project": "village", "layout": layout}),
    ));
    let (first, second) = (
        first["render"]["id"].as_str().unwrap().to_owned(),
        second["render"]["id"].as_str().unwrap().to_owned(),
    );
    let measured = ok(service.call(
        "measure_audio",
        json!({"audio": {"project": "village", "render": first}}),
    ));
    assert_eq!(measured["input"]["revision"], "r1");
    assert!(measured["loudness"]["integrated_lufs"].as_f64().is_some());
    assert!(
        measured["levels"]["true_peak_dbtp"].as_f64().unwrap()
            >= measured["levels"]["sample_peak_dbfs"].as_f64().unwrap()
    );

    // The same bytes as a workspace file measure identically.
    let path = measured["input"]["path"].as_str().unwrap();
    std::fs::copy(path, workspace.path.join("copy.wav")).unwrap();
    let copied = ok(service.call("measure_audio", json!({"audio": {"path": "copy.wav"}})));
    assert_eq!(copied["loudness"], measured["loudness"]);
    assert_eq!(copied["input"]["sha256"], measured["input"]["sha256"]);

    let same = ok(service.call(
        "compare_audio",
        json!({"reference": {"project": "village", "render": first}, "candidate": {"path": "copy.wav"}}),
    ));
    assert_eq!(same["difference"]["identical"], true);
    assert_eq!(same["deltas"]["integrated_lu"], 0.0);
    assert!(same["difference"].get("overlap_identical").is_none());

    // A shorter copy matches over its whole overlap and says so.
    let tone = sine(8000, 440.0, 0.5, 1.0);
    workspace.write(
        "long.wav",
        &encode(std::slice::from_ref(&tone), 8000, SampleEncoding::PcmS16),
    );
    workspace.write(
        "short.wav",
        &encode(&[tone[..6000].to_vec()], 8000, SampleEncoding::PcmS16),
    );
    let shorter = service.call(
        "compare_audio",
        json!({"reference": {"path": "long.wav"}, "candidate": {"path": "short.wav"}}),
    );
    assert!(
        shorter.summary.starts_with("overlap sample-identical (6000 frames at offset 0 samples); 2000 unmatched reference and 0 unmatched candidate frames"),
        "{}",
        shorter.summary
    );
    let shorter = ok(shorter);
    assert_eq!(shorter["difference"]["identical"], false);
    assert_eq!(shorter["difference"]["overlap_identical"], true);

    let revised = ok(service.call(
        "compare_audio",
        json!({"reference": {"project": "village", "render": first},
               "candidate": {"project": "village", "render": second}, "max_offset_seconds": 0.05}),
    ));
    assert_eq!(revised["difference"]["identical"], false);
    assert!(
        revised["difference"]["alignment"]["searched_offset_samples"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        !revised["difference"]["largest_difference_windows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(revised["candidate"]["input"]["revision"], "r2");

    // A -6 dB master gain from the mixer is reported as a level change, not new content.
    ok(service.call(
        "edit_project",
        json!({"project": "village", "request": {"job_id": "quieter", "base_revision": "r2",
               "scope": {"buses": true},
               "operations": [{"op": "set_mix", "target": {"kind": "master"}, "volume_db": -6.0}]}}),
    ));
    let quieter = ok(service.call(
        "render_preview",
        json!({"job_id": "render-r3", "project": "village", "layout": layout}),
    ));
    let mixed = ok(service.call(
        "compare_audio",
        json!({"reference": {"project": "village", "render": second},
               "candidate": {"project": "village", "render": quieter["render"]["id"]}}),
    ));
    let overall = &mixed["difference"]["overall"];
    assert!(
        (overall["best_gain_db"].as_f64().unwrap() + 6.02).abs() < 0.02,
        "{overall}"
    );
    assert!(
        overall["gain_matched_residual_db"].as_f64().unwrap() < -40.0,
        "{overall}"
    );
    assert!((mixed["deltas"]["integrated_lu"].as_f64().unwrap() + 6.02).abs() < 0.05);
}

#[test]
fn malformed_and_out_of_bounds_inputs_are_refused_with_locations() {
    let workspace = Workspace::new("refusals");
    let mut good = encode(
        &[sine(8_000, 440.0, 0.5, 1.0)],
        8_000,
        SampleEncoding::PcmS16,
    );
    workspace.write("good.wav", &good);
    good[22] = 0; // zero channels
    workspace.write("bad.wav", &good);
    workspace.write(
        "stereo.wav",
        &encode(
            &[sine(8_000, 440.0, 0.5, 1.0), sine(8_000, 440.0, 0.5, 1.0)],
            8_000,
            SampleEncoding::PcmS16,
        ),
    );
    let mut service = Service::new(&workspace.path);

    let bad = service.call("measure_audio", json!({"audio": {"path": "bad.wav"}}));
    assert_eq!(
        refusal(&bad),
        ("invalid_audio".into(), "/audio/path".into())
    );
    assert_eq!(bad.result["byte_offset"], 22);
    assert_eq!(bad.result["field"], "fmt channels");

    let cases = [
        (
            json!({"audio": {"path": "good.wav"}, "range": {"start_seconds": 0.5, "end_seconds": 1.5}}),
            "value_out_of_range",
            "/range/end_seconds",
        ),
        (
            json!({"audio": {"path": "good.wav"}, "range": {"start_seconds": -1.0}}),
            "value_out_of_range",
            "/range/start_seconds",
        ),
        (
            json!({"audio": {"path": "good.wav", "project": "x"}}),
            "invalid_range",
            "/audio",
        ),
        (
            json!({"audio": {"path": "good.wav", "expected_sha256": "00"}}),
            "invalid_range",
            "/audio/expected_sha256",
        ),
        (
            json!({"audio": {"path": "good.wav"}, "silence_threshold_dbfs": 3.0}),
            "value_out_of_range",
            "/silence_threshold_dbfs",
        ),
    ];
    for (input, code, location) in cases {
        let response = service.call("measure_audio", input);
        assert_eq!(refusal(&response), (code.into(), location.into()));
    }
    let mismatch = service.call(
        "compare_audio",
        json!({"reference": {"path": "good.wav"}, "candidate": {"path": "stereo.wav"}}),
    );
    assert_eq!(
        refusal(&mismatch),
        ("value_out_of_range".into(), "/candidate".into())
    );
    let bound = service.call(
        "compare_audio",
        json!({"reference": {"path": "good.wav"}, "candidate": {"path": "good.wav"}, "max_offset_seconds": 3.0}),
    );
    assert_eq!(
        refusal(&bound),
        ("value_out_of_range".into(), "/max_offset_seconds".into())
    );
    let missing = service.call("measure_audio", json!({"audio": {"path": "missing.wav"}}));
    assert_eq!(missing.status, Status::Error);
    // Arguments outside the schema have their own code.
    let unknown = service.call(
        "compare_audio",
        json!({"base": {"path": "good.wav"}, "candidate": {"path": "good.wav"}}),
    );
    assert_eq!(refusal(&unknown), ("schema_mismatch".into(), "".into()));
    assert!(
        unknown.diagnostics[0]
            .message
            .contains("do not match the tool schema")
    );
}

#[test]
fn each_compared_input_can_have_its_own_range() {
    let workspace = Workspace::new("ranges");
    let rate = 8_000;
    // Two identical seconds of 440 Hz, then a second of 660 Hz: like loop
    // passes 1 and 2 followed by something else.
    let mut tone = sine(rate, 440.0, 0.5, 1.0);
    tone.extend(sine(rate, 440.0, 0.5, 1.0));
    tone.extend(sine(rate, 660.0, 0.5, 1.0));
    workspace.write("passes.wav", &encode(&[tone], rate, SampleEncoding::PcmS16));
    let mut service = Service::new(&workspace.path);
    let input = |reference: (f64, f64), candidate: (f64, f64)| {
        json!({"reference": {"path": "passes.wav"}, "candidate": {"path": "passes.wav"},
            "reference_range": {"start_seconds": reference.0, "end_seconds": reference.1},
            "candidate_range": {"start_seconds": candidate.0, "end_seconds": candidate.1}})
    };
    let same = ok(service.call("compare_audio", input((0.0, 1.0), (1.0, 2.0))));
    assert_eq!(same["difference"]["identical"], true, "{same}");
    assert_eq!(same["candidate"]["input"]["range"]["start_frame"], 8000);
    assert_eq!(same["reference"]["input"]["range"]["start_frame"], 0);
    let other = ok(service.call("compare_audio", input((1.0, 2.0), (2.0, 3.0))));
    assert_eq!(other["difference"]["identical"], false);
    // A shared range still applies to an input without its own.
    let shared = ok(service.call(
        "compare_audio",
        json!({"reference": {"path": "passes.wav"}, "candidate": {"path": "passes.wav"},
            "range": {"start_seconds": 0.0, "end_seconds": 1.0},
            "candidate_range": {"start_seconds": 1.0, "end_seconds": 2.0}}),
    ));
    assert_eq!(shared["difference"]["identical"], true);
    let late = service.call("compare_audio", input((0.0, 1.0), (2.5, 4.0)));
    assert_eq!(
        refusal(&late),
        (
            "value_out_of_range".into(),
            "/candidate_range/end_seconds".into()
        )
    );
}

#[test]
fn pitch_is_tracked_compared_and_its_settings_checked() {
    let workspace = Workspace::new("pitch");
    let rate = 44_100;
    let a = sine(rate, 220.0, 0.4, 0.5);
    let up = 220.0 * 2f64.powf(3.0 / 12.0);
    let b = sine(rate, up, 0.4, 0.5);
    workspace.write("a.wav", &encode(&[a], rate, SampleEncoding::PcmS16));
    workspace.write("b.wav", &encode(&[b], rate, SampleEncoding::PcmS16));
    let mut service = Service::new(&workspace.path);
    let plain = ok(service.call("measure_audio", json!({"audio": {"path": "a.wav"}})));
    assert!(plain.get("pitch").is_none());
    let response = service.call(
        "measure_audio",
        json!({"audio": {"path": "a.wav"}, "pitch": {}, "spectrum": "none"}),
    );
    assert!(
        response.summary.contains("median f0 220.0 Hz (A3"),
        "{}",
        response.summary
    );
    let measured = ok(response);
    let pitch = &measured["pitch"];
    assert_eq!(pitch["summary"]["median_note_name"], "A3");
    assert!((pitch["summary"]["median_hz"].as_f64().unwrap() - 220.0).abs() < 0.05);
    assert_eq!(pitch["notes"].as_array().unwrap().len(), 1);
    let windows = pitch["windows"].as_array().unwrap();
    assert_eq!(
        windows.len(),
        pitch["summary"]["windows"].as_u64().unwrap() as usize
    );
    assert_eq!(windows[0]["note"], 57);
    let without = ok(service.call(
        "measure_audio",
        json!({"audio": {"path": "a.wav"}, "pitch": {"windows": false}, "range": {"start_seconds": 0.1}}),
    ));
    assert!(without["pitch"].get("windows").is_none());
    // Window times are input seconds: the range start is added.
    assert!(
        without["pitch"]["notes"][0]["from_seconds"]
            .as_f64()
            .unwrap()
            > 0.1
    );

    let compared = ok(service.call(
        "compare_audio",
        json!({"reference": {"path": "a.wav"}, "candidate": {"path": "b.wav"}, "pitch": {}}),
    ));
    let p = &compared["pitch"];
    assert!((p["median_difference_cents"].as_f64().unwrap() - 300.0).abs() < 0.5);
    assert_eq!(p["different_note_windows"], p["compared_windows"]);
    assert_eq!(p["largest_differences"].as_array().unwrap().len(), 5);

    for (pitch, location) in [
        (json!({"min_hz": 5}), "/pitch/min_hz"),
        (json!({"min_hz": 300, "max_hz": 200}), "/pitch/max_hz"),
        (json!({"window_seconds": 0.005}), "/pitch/window_seconds"),
        (json!({"hop_seconds": 0}), "/pitch/hop_seconds"),
        (json!({"threshold": 0.9}), "/pitch/threshold"),
        (json!({"a4_hz": 300}), "/pitch/a4_hz"),
    ] {
        let response = service.call(
            "measure_audio",
            json!({"audio": {"path": "a.wav"}, "pitch": pitch}),
        );
        assert_eq!(
            refusal(&response),
            ("value_out_of_range".to_owned(), location.to_owned())
        );
    }
    // More windows than can be listed: refused unless windows are left out.
    let long = sine(rate, 220.0, 0.4, 21.0);
    workspace.write("long.wav", &encode(&[long], rate, SampleEncoding::PcmS16));
    let response = service.call(
        "measure_audio",
        json!({"audio": {"path": "long.wav"}, "pitch": {}}),
    );
    assert_eq!(
        refusal(&response),
        ("value_out_of_range".to_owned(), "/pitch/windows".to_owned())
    );
    ok(service.call(
        "measure_audio",
        json!({"audio": {"path": "long.wav"}, "pitch": {"windows": false}, "spectrum": "none"}),
    ));
}
