//! Mixer state: edit operations, scope checks, rendering and delivery.

mod common;

use serde_json::{Value, json};
use tunemend::composition::{Composition, Timebase};
use tunemend::edit::{EditRequest, apply_edit};
use tunemend::render::{RenderedAudio, render, sample_at};
use tunemend::review::RenderLayout;
use tunemend::service::{Service, Status};
use tunemend::soundfont::reference_soundfont;
use tunemend::timeline::Rational;

const RATE: u32 = 22_050;

fn village() -> (Timebase, Composition) {
    let bundle = common::village();
    (
        bundle.project.timebase,
        bundle.revisions[0].composition.clone(),
    )
}

fn edit(base: &Composition, request: Value) -> Result<Composition, Vec<String>> {
    let mut request = request;
    request["job_id"] = json!("job");
    request["base_revision"] = json!("r1");
    let request: EditRequest = serde_json::from_value(request).unwrap();
    apply_edit(Timebase::Music { ppq: 480 }, base, &request).map_err(|diagnostics| {
        diagnostics
            .iter()
            .map(|d| {
                serde_json::to_value(d.code)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    })
}

fn with_ops(base: &Composition, operations: Value) -> Composition {
    edit(
        base,
        json!({"scope": {"buses": true}, "operations": operations}),
    )
    .unwrap()
}

fn play(composition: &Composition, passes: u32, voices: Option<&[&str]>) -> RenderedAudio {
    let voices: Option<Vec<String>> = voices.map(|v| v.iter().map(|s| s.to_string()).collect());
    render(
        Timebase::Music { ppq: 480 },
        composition,
        &RenderLayout::Full {
            loop_passes: passes,
        },
        voices.as_deref(),
        &reference_soundfont(),
        RATE,
    )
    .unwrap()
}

fn at_seconds(numerator: i128, denominator: i128) -> usize {
    sample_at(Rational::new(numerator, denominator), RATE) as usize
}

/// Every content sample of `actual` is `reference` times `gain`.
fn assert_gain(actual: &[f32], reference: &[f32], range: std::ops::Range<usize>, gain: f64) {
    for n in range {
        let expected = f64::from(reference[n]) * gain;
        assert!(
            (f64::from(actual[n]) - expected).abs() <= 1e-6 + expected.abs() * 1e-5,
            "sample {n}: {} instead of {expected}",
            actual[n]
        );
    }
}

#[test]
fn neutral_mixer_data_is_absent_and_renders_like_no_mixer() {
    let (_, bgm) = village();
    let plain = play(&bgm, 2, None);
    let louder = with_ops(
        &bgm,
        json!([{"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "volume_db": -6.0}]),
    );
    assert!(!louder.mix.is_empty());
    let restored = with_ops(
        &louder,
        json!([{"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "volume_db": 0}]),
    );
    assert!(restored.mix.is_empty(), "neutral strips are removed");
    assert_eq!(
        serde_json::to_string(&restored).unwrap(),
        serde_json::to_string(&bgm).unwrap()
    );
    assert!(!serde_json::to_string(&bgm).unwrap().contains("\"mix\""));
    assert_eq!(play(&restored, 2, None), plain);
}

#[test]
fn rendered_levels_follow_declared_gain_pan_routing_and_sends() {
    let (_, bgm) = village();
    let melody = Some(&["melody"][..]);
    let plain = play(&bgm, 1, melody);
    let content = plain.content_samples as usize;

    // An unused bus switches to per-voice mixing without changing the sound.
    let neutral = with_ops(&bgm, json!([{"op": "add_bus", "bus": {"id": "spare"}}]));
    let mixed = play(&neutral, 1, melody);
    assert_eq!(mixed.left[..content], plain.left[..content]);
    assert_eq!(mixed.right[..content], plain.right[..content]);

    let quieter = with_ops(
        &bgm,
        json!([{"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "volume_db": -6.0}]),
    );
    let audio = play(&quieter, 1, melody);
    let gain = 10f64.powf(-6.0 / 20.0);
    assert_gain(&audio.left, &plain.left, 0..content, gain);
    assert_gain(&audio.right, &plain.right, 0..content, gain);

    let right = with_ops(
        &bgm,
        json!([{"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "pan": 100}]),
    );
    let audio = play(&right, 1, melody);
    assert!(audio.left[..content].iter().all(|s| *s == 0.0));
    assert_eq!(audio.right[..content], plain.right[..content]);

    // Routed through a -6 dB bus, then the master at -3 dB.
    let routed = with_ops(
        &bgm,
        json!([
            {"op": "add_bus", "bus": {"id": "lead", "volume_db": -6.0}},
            {"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "output": "lead"},
            {"op": "set_mix", "target": {"kind": "master"}, "volume_db": -3.0},
        ]),
    );
    let audio = play(&routed, 1, melody);
    assert_gain(
        &audio.left,
        &plain.left,
        0..content,
        10f64.powf(-9.0 / 20.0),
    );

    // A pre-fader send keeps the full signal while the fader is down.
    let sent = with_ops(
        &bgm,
        json!([
            {"op": "add_bus", "bus": {"id": "echo"}},
            {"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "volume_db": -120.0},
            {"op": "set_send", "send": {"voice": "melody", "bus": "echo", "gain_db": 0, "pre_fader": true}},
        ]),
    );
    let audio = play(&sent, 1, melody);
    assert_gain(&audio.left, &plain.left, 0..content, 1.0 + 10f64.powf(-6.0));
}

#[test]
fn mute_and_solo_choose_the_voices_heard() {
    let (_, bgm) = village();
    let bass_only = play(&bgm, 1, Some(&["bass"]));
    let soloed = with_ops(
        &bgm,
        json!([{"op": "set_mix", "target": {"kind": "voice", "id": "bass"}, "solo": true}]),
    );
    let audio = play(&soloed, 1, None);
    let content = bass_only.content_samples as usize;
    assert_eq!(audio.left[..content], bass_only.left[..content]);

    let muted = with_ops(
        &bgm,
        json!([{"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "mute": true}]),
    );
    let audio = play(&muted, 1, Some(&["melody"]));
    assert!(audio.left.iter().chain(&audio.right).all(|s| *s == 0.0));
}

#[test]
fn automation_points_take_effect_at_their_mapped_render_samples() {
    let (_, bgm) = village();
    let melody = Some(&["melody"][..]);
    let plain = play(&bgm, 2, melody);
    let cut = with_ops(
        &bgm,
        json!([{"op": "set_mix_automation", "target": {"kind": "voice", "id": "melody"}, "parameter": "volume",
                "points": [{"at": 3840, "value": 0}, {"at": 5760, "value": -120.0}]}]),
    );
    let audio = play(&cut, 2, melody);
    let silent = 10f64.powf(-6.0);
    // Tick 5760 is 6 s; in the second pass it returns at 14.4 s and cuts at 16.4 s.
    let first_cut = at_seconds(6, 1);
    let loop_return = at_seconds(72, 5);
    let second_cut = at_seconds(82, 5);
    assert_gain(&audio.left, &plain.left, 0..first_cut, 1.0);
    assert_gain(&audio.left, &plain.left, first_cut..loop_return, silent);
    assert_gain(&audio.left, &plain.left, loop_return..second_cut, 1.0);
    assert_gain(
        &audio.left,
        &plain.left,
        second_cut..second_cut + 2000,
        silent,
    );

    // Linear ramps interpolate by timebase position: tick 9600 is 3/4 of the
    // way from 3840 to 11520 although the tempo changes at 7680.
    let ramp = with_ops(
        &bgm,
        json!([{"op": "set_mix_automation", "target": {"kind": "voice", "id": "melody"}, "parameter": "volume",
                "points": [{"at": 3840, "value": 0, "shape": "linear"}, {"at": 11520, "value": -16.0}]}]),
    );
    let audio = play(&ramp, 1, melody);
    let around = at_seconds(48, 5);
    let expected = 10f64.powf(-12.0 / 20.0);
    let ratios: Vec<f64> = (around - 20..around + 20)
        .filter(|&n| plain.left[n].abs() > 0.01)
        .map(|n| f64::from(audio.left[n]) / f64::from(plain.left[n]))
        .collect();
    assert!(!ratios.is_empty());
    assert!(
        ratios.iter().all(|r| (r / expected - 1.0).abs() < 0.002),
        "{ratios:?}"
    );
}

#[test]
fn excerpts_start_with_the_mixer_value_in_effect() {
    let (_, bgm) = village();
    let melody = Some(vec!["melody".to_owned()]);
    let ramp = with_ops(
        &bgm,
        json!([{"op": "set_mix_automation", "target": {"kind": "voice", "id": "melody"}, "parameter": "volume",
                "points": [{"at": 3840, "value": 0, "shape": "linear"}, {"at": 11520, "value": -16.0}]}]),
    );
    let layout = RenderLayout::Excerpt {
        start: 9600,
        end: 11520,
    };
    let excerpt = |c: &Composition| {
        render(
            Timebase::Music { ppq: 480 },
            c,
            &layout,
            melody.as_deref(),
            &reference_soundfont(),
            RATE,
        )
        .unwrap()
    };
    let (plain, mixed) = (excerpt(&bgm), excerpt(&ramp));
    let expected = 10f64.powf(-12.0 / 20.0);
    let ratios: Vec<f64> = (0..40)
        .filter(|&n| plain.left[n].abs() > 0.01)
        .map(|n| f64::from(mixed.left[n]) / f64::from(plain.left[n]))
        .collect();
    assert!(!ratios.is_empty());
    assert!(ratios.iter().all(|r| (r / expected - 1.0).abs() < 0.002));
}

#[test]
fn mixer_edits_respect_scope_protections_and_references() {
    let (_, bgm) = village();
    let voice_volume =
        json!({"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "volume_db": -3.0});
    assert_eq!(
        edit(
            &bgm,
            json!({"protections": [{"voice": "melody"}], "operations": [voice_volume]})
        ),
        Err(vec!["protected_change".to_owned()])
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"scope": {"voices": ["bass"]}, "operations": [voice_volume]})
        ),
        Err(vec!["out_of_scope".to_owned()])
    );
    assert!(
        edit(
            &bgm,
            json!({"scope": {"voices": ["melody"]}, "operations": [voice_volume]})
        )
        .is_ok()
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"operations": [{"op": "set_mix", "target": {"kind": "master"}, "volume_db": -1.0}]})
        ),
        Err(vec!["out_of_scope".to_owned()]),
        "the master needs buses scope"
    );

    // Automation is checked where it changes the sound.
    let fade = |at: u64| {
        json!({"op": "set_mix_automation", "target": {"kind": "voice", "id": "bass"}, "parameter": "volume",
               "points": [{"at": 3840, "value": 0}, {"at": at, "value": -12.0}, {"at": at + 960, "value": 0}]})
    };
    assert!(
        edit(
            &bgm,
            json!({"scope": {"range": {"start": 3840, "end": 9000}}, "operations": [fade(7680)]})
        )
        .is_ok()
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"scope": {"range": {"start": 3840, "end": 9000}}, "operations": [fade(9600)]})
        ),
        Err(vec!["out_of_scope".to_owned()])
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"protections": [{"voice": "bass", "range": {"start": 9000, "end": 9100}}], "operations": [fade(7680)]})
        ),
        Ok(with_ops(&bgm, json!([fade(7680)]))),
        "a protection outside the changed span does not block"
    );

    // References and loop state are checked on the result.
    assert_eq!(
        edit(
            &bgm,
            json!({"scope": {"buses": true}, "operations": [
            {"op": "add_bus", "bus": {"id": "drums"}},
            {"op": "set_mix", "target": {"kind": "voice", "id": "bass"}, "output": "drums"},
            {"op": "remove_bus", "id": "drums"}]})
        ),
        Err(vec!["unknown_reference".to_owned()])
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"operations": [{"op": "set_mix_automation", "target": {"kind": "voice", "id": "bass"},
            "parameter": "pan", "points": [{"at": 7680, "value": -50}]}]})
        ),
        Err(vec!["loop_state_mismatch".to_owned()])
    );
    assert!(
        serde_json::from_value::<EditRequest>(
            json!({"job_id": "j", "base_revision": "r1", "operations": [
            {"op": "set_mix", "target": {"kind": "voice", "id": "bass"}, "volume_db": -6.05}]})
        )
        .is_err(),
        "gains have 0.1 dB resolution"
    );
}

struct Workspace(std::path::PathBuf);

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ok(response: tunemend::service::ToolResponse) -> Value {
    assert_eq!(response.status, Status::Ok, "{response:#?}");
    response.result
}

#[test]
fn source_backed_mixer_changes_renders_but_never_source_bytes() {
    let path = std::env::temp_dir().join(format!("tunemend-test-mixing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    let workspace = Workspace(path);
    let mut service = Service::new(&workspace.0);
    let (timebase, bgm) = village();
    ok(service.call(
        "create_project",
        json!({"job_id": "create", "project": "village", "timebase": timebase, "composition": bgm}),
    ));
    ok(service.call(
        "export_project",
        json!({"job_id": "export", "project": "village", "destination": "out/source"}),
    ));
    ok(service.call(
        "import_midi",
        json!({"job_id": "import", "project": "imported", "path": "out/source/village.mid"}),
    ));
    let summary = ok(service.call(
        "inspect_project",
        json!({"view": "summary", "project": "imported"}),
    ));
    let voice = summary["voices"][0]["id"].as_str().unwrap().to_owned();
    let render = |service: &mut Service, job: &str, voice: &str| -> Vec<u8> {
        let response = service.call(
            "render_preview",
            json!({"job_id": job, "project": "imported", "layout": {"kind": "full", "loop_passes": 1}, "voices": [voice]}),
        );
        assert_eq!(response.status, Status::Ok, "{response:#?}");
        std::fs::read(&response.artifacts[0].path).unwrap()
    };
    let before = render(&mut service, "before", &voice);

    let edited = ok(service.call(
        "edit_project",
        json!({"project": "imported", "request": {"job_id": "mix", "base_revision": "r1", "scope": {"buses": true}, "operations": [
            {"op": "add_bus", "bus": {"id": "lead", "role": "lead group"}},
            {"op": "set_mix", "target": {"kind": "voice", "id": voice}, "output": "lead", "pan": -25.5},
            {"op": "set_mix", "target": {"kind": "bus", "id": "lead"}, "volume_db": -6.0},
        ]}}),
    ));
    assert_eq!(
        edited["difference"]["changed_mix"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let summary = ok(service.call(
        "inspect_project",
        json!({"view": "summary", "project": "imported"}),
    ));
    assert_eq!(summary["mix"]["buses"][0]["volume_db"], json!(-6.0));
    assert_eq!(summary["mix"]["voices"][0]["pan"], json!(-25.5));

    let after = render(&mut service, "after", &voice);
    // 16-bit PCM: the left channel is -6 dB and the right -6 dB × (1 - 0.255) quieter.
    let peak = |wav: &[u8], side: usize| {
        wav[44..]
            .chunks(4)
            .map(|frame| i16::from_le_bytes([frame[side * 2], frame[side * 2 + 1]]).unsigned_abs())
            .max()
            .unwrap() as f64
    };
    let left = peak(&after, 0) / peak(&before, 0);
    let right = peak(&after, 1) / peak(&before, 1);
    assert!((left / 10f64.powf(-0.3) - 1.0).abs() < 0.01, "{left}");
    assert!(
        (right / (10f64.powf(-0.3) * 0.745) - 1.0).abs() < 0.01,
        "{right}"
    );

    ok(service.call(
        "export_project",
        json!({"job_id": "export-imported", "project": "imported", "destination": "out/imported"}),
    ));
    let source = std::fs::read(workspace.0.join("out/source/village.mid")).unwrap();
    let delivered = std::fs::read(workspace.0.join("out/imported/imported.mid")).unwrap();
    assert_eq!(source, delivered, "mixer state never rewrites source bytes");
    let report: Value = serde_json::from_slice(
        &std::fs::read(workspace.0.join("out/imported/report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["mix"]["written_to_midi"], json!(false));
    assert!(
        report["not_in_midi"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap().contains("mixer"))
    );
    let bundle: tunemend::ProjectBundle = serde_json::from_slice(
        &std::fs::read(workspace.0.join("out/imported/imported.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(tunemend::check_project_bundle(&bundle), vec![]);
    assert!(!bundle.revisions.last().unwrap().composition.mix.is_empty());
}
