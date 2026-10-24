//! Insert effect chains: edit operations, scope checks, rendering, tails and delivery.

mod common;

use serde_json::{Value, json};
use tunemend::composition::{Composition, Timebase};
use tunemend::edit::{EditRequest, apply_edit};
use tunemend::render::{RenderedAudio, render};
use tunemend::review::RenderLayout;
use tunemend::service::{Service, Status};
use tunemend::soundfont::reference_soundfont;

const RATE: u32 = 22_050;

fn village() -> Composition {
    common::village().revisions[0].composition.clone()
}

fn edit_in(
    timebase: Timebase,
    base: &Composition,
    request: Value,
) -> Result<Composition, Vec<String>> {
    let mut request = request;
    request["job_id"] = json!("job");
    request["base_revision"] = json!("r1");
    let request: EditRequest = serde_json::from_value(request).unwrap();
    apply_edit(timebase, base, &request).map_err(|diagnostics| {
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

fn edit(base: &Composition, request: Value) -> Result<Composition, Vec<String>> {
    edit_in(Timebase::Music { ppq: 480 }, base, request)
}

fn with_ops(base: &Composition, operations: Value) -> Composition {
    edit(
        base,
        json!({"scope": {"buses": true}, "operations": operations}),
    )
    .unwrap()
}

fn play(composition: &Composition, voices: Option<&[&str]>) -> RenderedAudio {
    let voices: Option<Vec<String>> = voices.map(|v| v.iter().map(|s| s.to_string()).collect());
    render(
        Timebase::Music { ppq: 480 },
        composition,
        &RenderLayout::Full { loop_passes: 1 },
        voices.as_deref(),
        &reference_soundfont(),
        RATE,
    )
    .unwrap()
}

fn energy(samples: &[f32]) -> f64 {
    samples.iter().map(|s| f64::from(*s).powi(2)).sum()
}

fn voice(id: &str) -> Value {
    json!({"kind": "voice", "id": id})
}

fn eq() -> Value {
    json!({"id": "tone", "processor": {"kind": "eq", "bands": [
        {"filter": "high_pass", "frequency_hz": 80.0, "q": 0.71},
        {"filter": "peaking", "frequency_hz": 2500.0, "gain_db": 3.0, "q": 1.2}]}})
}

fn limiter(ceiling: f64) -> Value {
    json!({"id": "limit", "processor": {"kind": "limiter", "ceiling_db": ceiling, "release_ms": 80.0, "lookahead_ms": 5.0}})
}

#[test]
fn chain_operations_insert_move_update_and_remove() {
    let bgm = village();
    let chained = with_ops(
        &bgm,
        json!([
            {"op": "insert_effect", "target": voice("melody"), "effect": eq()},
            {"op": "insert_effect", "target": voice("melody"), "index": 0, "effect":
                {"id": "squash", "processor": {"kind": "compressor", "threshold_db": -18.0, "ratio": 3.0, "attack_ms": 5.0, "release_ms": 120.0}}},
            {"op": "move_effect", "target": voice("melody"), "id": "tone", "index": 0},
            {"op": "set_effect", "target": voice("melody"), "id": "squash", "bypass": true},
            {"op": "set_effect", "target": voice("melody"), "id": "squash", "processor":
                {"kind": "compressor", "threshold_db": -24.0, "ratio": 4.0, "knee_db": 6.0, "attack_ms": 5.0, "release_ms": 120.0}},
        ]),
    );
    let chain = serde_json::to_value(&chained.mix.effects).unwrap();
    assert_eq!(chain[0]["target"], voice("melody"));
    assert_eq!(chain[0]["effects"][0]["id"], json!("tone"));
    assert_eq!(chain[0]["effects"][1]["id"], json!("squash"));
    assert_eq!(chain[0]["effects"][1]["bypass"], json!(true));
    assert_eq!(chain[0]["effects"][1]["processor"]["knee_db"], json!(6.0));

    // Removing the last effect removes the chain and leaves no mixer data.
    let removed = with_ops(
        &chained,
        json!([
            {"op": "remove_effect", "target": voice("melody"), "id": "tone"},
            {"op": "remove_effect", "target": voice("melody"), "id": "squash"},
        ]),
    );
    assert_eq!(
        serde_json::to_string(&removed).unwrap(),
        serde_json::to_string(&bgm).unwrap()
    );

    // set_effects replaces a chain; removing a bus removes its chain.
    let bus = with_ops(
        &bgm,
        json!([
            {"op": "add_bus", "bus": {"id": "echo"}},
            {"op": "set_effects", "target": {"kind": "bus", "id": "echo"}, "effects": [
                {"id": "snes", "processor": {"kind": "snes_echo", "fir": [127, 0, 0, 0, 0, 0, 0, 0], "delay": 5, "feedback": 70, "volume_left": 50, "volume_right": -50, "dry": false}}]},
        ]),
    );
    assert_eq!(bus.mix.effects.len(), 1);
    let gone = with_ops(&bus, json!([{"op": "remove_bus", "id": "echo"}]));
    assert!(gone.mix.is_empty());

    let refusals = [
        (
            json!([{"op": "insert_effect", "target": voice("melody"), "effect": eq()},
                {"op": "insert_effect", "target": voice("melody"), "effect": eq()}]),
            "duplicate_id",
        ),
        (
            json!([{"op": "insert_effect", "target": voice("melody"), "effect": eq()},
                {"op": "set_effect", "target": voice("melody"), "id": "tone", "processor": limiter(-1.0)["processor"]}]),
            "invalid_range",
        ),
        (
            json!([{"op": "remove_effect", "target": voice("melody"), "id": "none"}]),
            "unknown_reference",
        ),
        (
            json!([{"op": "insert_effect", "target": {"kind": "bus", "id": "none"}, "effect": eq()}]),
            "unknown_reference",
        ),
        (
            json!([{"op": "insert_effect", "target": voice("melody"), "index": 3, "effect": eq()}]),
            "invalid_range",
        ),
        (
            json!([{"op": "insert_effect", "target": {"kind": "master"}, "effect": limiter(0.5)}]),
            "value_out_of_range",
        ),
    ];
    for (operations, code) in refusals {
        assert_eq!(
            edit(
                &bgm,
                json!({"scope": {"buses": true}, "operations": operations})
            ),
            Err(vec![code.to_owned()]),
            "{operations}"
        );
    }
    assert!(
        serde_json::from_value::<EditRequest>(json!({"job_id": "j", "base_revision": "r1", "operations": [
            {"op": "insert_effect", "target": voice("melody"), "effect": {"id": "x", "processor":
                {"kind": "eq", "bands": [{"filter": "peaking", "frequency_hz": 1000.0, "gain_db": 1.0, "q": 0.707}]}}}]}))
        .is_err(),
        "q has 0.01 resolution"
    );
}

#[test]
fn chains_follow_voice_scope_and_bus_scope() {
    let bgm = village();
    let insert = json!({"op": "insert_effect", "target": voice("melody"), "effect": eq()});
    assert_eq!(
        edit(
            &bgm,
            json!({"protections": [{"voice": "melody"}], "operations": [insert]})
        ),
        Err(vec!["protected_change".to_owned()])
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"scope": {"voices": ["bass"]}, "operations": [insert]})
        ),
        Err(vec!["out_of_scope".to_owned()])
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"scope": {"range": {"start": 0, "end": 960}}, "operations": [insert]})
        ),
        Err(vec!["out_of_scope".to_owned()]),
        "a chain affects the whole time span"
    );
    assert!(
        edit(
            &bgm,
            json!({"scope": {"voices": ["melody"]}, "operations": [insert]})
        )
        .is_ok()
    );
    let master =
        json!({"op": "insert_effect", "target": {"kind": "master"}, "effect": limiter(-1.0)});
    assert_eq!(
        edit(&bgm, json!({"operations": [master]})),
        Err(vec!["out_of_scope".to_owned()]),
        "the master chain needs buses scope"
    );
    assert!(
        edit(
            &bgm,
            json!({"scope": {"buses": true}, "operations": [master]})
        )
        .is_ok()
    );
}

fn ducker(key: Value) -> Value {
    json!({"id": "duck", "processor": {"kind": "compressor", "threshold_db": -40.0, "ratio": 20.0,
        "attack_ms": 1.0, "release_ms": 60.0, "sidechain": key}})
}

#[test]
fn sidechains_are_declared_and_acyclic() {
    let bgm = village();
    let keyed = |target: &str, key: Value| json!({"op": "insert_effect", "target": voice(target), "effect": ducker(key)});
    assert!(
        edit(
            &bgm,
            json!({"operations": [keyed("bass", voice("melody"))]})
        )
        .is_ok()
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"operations": [keyed("bass", voice("melody")), keyed("melody", voice("bass"))]})
        ),
        Err(vec!["invalid_range".to_owned()]),
        "a key cycle is refused"
    );
    assert_eq!(
        edit(&bgm, json!({"operations": [keyed("bass", voice("bass"))]})),
        Err(vec!["invalid_range".to_owned()])
    );
    assert_eq!(
        edit(&bgm, json!({"operations": [keyed("bass", voice("lead"))]})),
        Err(vec!["unknown_reference".to_owned()])
    );
    assert_eq!(
        edit(
            &bgm,
            json!({"scope": {"buses": true}, "operations": [
            {"op": "add_bus", "bus": {"id": "drums"}}, keyed("bass", json!({"kind": "bus", "id": "drums"}))]})
        ),
        Err(vec!["invalid_range".to_owned()]),
        "voices are processed before buses"
    );
    // A bus or the master may be keyed by a voice or another bus.
    assert!(edit(&bgm, json!({"scope": {"buses": true}, "operations": [
        {"op": "add_bus", "bus": {"id": "a"}}, {"op": "add_bus", "bus": {"id": "b"}},
        {"op": "insert_effect", "target": {"kind": "bus", "id": "a"}, "effect": ducker(json!({"kind": "bus", "id": "b"}))},
        {"op": "insert_effect", "target": {"kind": "master"}, "effect": ducker(voice("melody"))}]})).is_ok());

    // A note-synced delay needs a tempo map.
    let chime = common::chime().revisions[0].composition.clone();
    let delay = json!({"op": "insert_effect", "target": voice("lead"), "effect": {"id": "d", "processor":
        {"kind": "delay", "time": {"unit": "note", "numerator": 1, "denominator": 8}, "mix_percent": 30.0}}});
    assert_eq!(
        edit_in(Timebase::Effect, &chime, json!({"operations": [delay]})),
        Err(vec!["musical_timing_in_effect".to_owned()])
    );
}

#[test]
fn bypassed_effects_and_silent_keys_render_like_no_effect() {
    let bgm = village();
    let base = with_ops(&bgm, json!([{"op": "add_bus", "bus": {"id": "spare"}}]));
    let plain = play(&base, None);
    let bypassed = with_ops(
        &base,
        json!([{"op": "set_effects", "target": {"kind": "master"}, "effects": [
            {"id": "verb", "bypass": true, "processor": {"kind": "reverb", "room_size_percent": 80.0, "damping_percent": 40.0, "mix_percent": 50.0}}]}]),
    );
    assert_eq!(play(&bypassed, None), plain);

    // The key voice is muted, so it is not rendered and keys nothing.
    let muted_key = with_ops(
        &base,
        json!([{"op": "set_mix", "target": voice("melody"), "mute": true},
               {"op": "insert_effect", "target": voice("bass"), "effect": ducker(voice("melody"))}]),
    );
    let muted = with_ops(
        &base,
        json!([{"op": "set_mix", "target": voice("melody"), "mute": true}]),
    );
    assert_eq!(play(&muted_key, None), play(&muted, None));
}

#[test]
fn a_sidechain_ducks_by_the_key_signal() {
    let bgm = village();
    // Melody keys the bass and is heard nowhere: its output goes to a muted bus.
    let setup = json!([
        {"op": "add_bus", "bus": {"id": "hidden", "mute": true}},
        {"op": "set_mix", "target": voice("melody"), "output": "hidden"},
        {"op": "set_mix", "target": voice("accompaniment"), "mute": true},
    ]);
    let undocked = with_ops(&bgm, setup.clone());
    let mut operations = setup.as_array().unwrap().clone();
    operations.push(
        json!({"op": "insert_effect", "target": voice("bass"), "effect": ducker(voice("melody"))}),
    );
    let ducked = with_ops(&bgm, Value::Array(operations));
    let (before, after) = (play(&undocked, None), play(&ducked, None));
    let content = before.content_samples as usize;
    let ratio = energy(&after.left[..content]) / energy(&before.left[..content]);
    assert!(ratio < 0.5, "{ratio}");
    assert!(ratio > 0.0);
}

#[test]
fn a_master_limiter_bounds_the_rendered_peak() {
    let bgm = village();
    let loud = with_ops(
        &bgm,
        json!([{"op": "set_mix", "target": {"kind": "master"}, "volume_db": 12.0}]),
    );
    let limited = with_ops(
        &loud,
        json!([{"op": "insert_effect", "target": {"kind": "master"}, "effect": limiter(-6.0)}]),
    );
    let peak = |audio: &RenderedAudio| {
        audio
            .left
            .iter()
            .chain(&audio.right)
            .fold(0.0f64, |p, s| p.max(f64::from(s.abs())))
    };
    let ceiling = 10f64.powf(-6.0 / 20.0);
    assert!(
        peak(&play(&loud, None)) > ceiling,
        "the test drives the master over the ceiling"
    );
    let audio = play(&limited, None);
    assert!(peak(&audio) <= ceiling, "{}", peak(&audio));
    assert!(peak(&audio) > ceiling * 0.98);
}

#[test]
fn delay_tails_keep_echoes_after_silent_gaps() {
    let bgm = village();
    let melody = Some(&["melody"][..]);
    // A half-second excerpt and one echo a second later: silence lies
    // between the excerpt's decay and the echo.
    let excerpt = |composition: &Composition| {
        render(
            Timebase::Music { ppq: 480 },
            composition,
            &RenderLayout::Excerpt {
                start: 3840,
                end: 4320,
            },
            Some(&["melody".to_owned()]),
            &reference_soundfont(),
            RATE,
        )
        .unwrap()
    };
    let echoed = with_ops(
        &bgm,
        json!([{"op": "insert_effect", "target": voice("melody"), "effect": {"id": "echo", "processor":
            {"kind": "delay", "time": {"unit": "ms", "ms": 1000.0}, "mix_percent": 50.0}}}]),
    );
    let (dry_excerpt, audio) = (excerpt(&bgm), excerpt(&echoed));
    assert_eq!(audio.content_samples, dry_excerpt.content_samples);
    assert!(!audio.tail_truncated);
    let content = audio.content_samples as usize;
    assert!(dry_excerpt.left.len() < content + RATE as usize / 10);
    assert!(
        audio.left.len() > content + RATE as usize,
        "the echo after the gap is kept"
    );
    let block = RATE as usize / 100;
    let blocks: Vec<f32> = audio.left[content..]
        .chunks(block)
        .zip(audio.right[content..].chunks(block))
        .map(|(l, r)| l.iter().chain(r).fold(0.0f32, |p, s| p.max(s.abs())))
        .collect();
    let first_silent = blocks
        .iter()
        .position(|p| *p < 1e-4)
        .expect("a gap before the echo");
    assert!(blocks[first_silent..].iter().any(|p| *p >= 1e-4));
    // The cut follows the first silent block after the echo.
    assert!(*blocks.last().unwrap() < 1e-4);
    assert!(blocks[blocks.len() - 2] >= 1e-4);

    let dry = play(&bgm, melody);

    // Note-synced: an eighth note at 120 BPM is 250 ms.
    let synced = with_ops(
        &bgm,
        json!([{"op": "insert_effect", "target": voice("melody"), "effect": {"id": "echo", "processor":
            {"kind": "delay", "time": {"unit": "note", "numerator": 1, "denominator": 8}, "mix_percent": 100.0}}}]),
    );
    let audio = play(&synced, melody);
    let shift = 5513; // round(0.25 s × 22050 Hz)
    for n in 0..RATE as usize {
        assert_eq!(audio.left[n + shift], dry.left[n], "sample {n}");
    }
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
fn service_edits_inspects_renders_and_reports_effects() {
    let path = std::env::temp_dir().join(format!("tunemend-test-effects-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    let workspace = Workspace(path);
    let mut service = Service::new(&workspace.0);
    let bundle = common::village();
    ok(service.call(
        "create_project",
        json!({"job_id": "create", "project": "village", "timebase": bundle.project.timebase, "composition": bundle.revisions[0].composition}),
    ));
    ok(service.call(
        "export_project",
        json!({"job_id": "export-dry", "project": "village", "destination": "out/dry"}),
    ));
    let edited = ok(service.call(
        "edit_project",
        json!({"project": "village", "request": {"job_id": "fx", "base_revision": "r1", "scope": {"buses": true},
            "rationale": "retro echo on the lead, safety limiter on the master",
            "operations": [
                {"op": "add_bus", "bus": {"id": "echo", "role": "SNES echo return"}},
                {"op": "set_send", "send": {"voice": "melody", "bus": "echo", "gain_db": -6.0}},
                {"op": "set_effects", "target": {"kind": "bus", "id": "echo"}, "effects": [
                    {"id": "snes", "processor": {"kind": "snes_echo", "fir": [127, 0, 0, 0, 0, 0, 0, 0], "delay": 4, "feedback": 60, "volume_left": 40, "volume_right": 40, "dry": false}}]},
                {"op": "insert_effect", "target": voice("melody"), "effect": eq()},
                {"op": "insert_effect", "target": {"kind": "master"}, "effect": limiter(-1.0)},
            ]}}),
    ));
    let changed: Vec<String> = edited["difference"]["changed_mix"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    for label in [
        "effects of bus `echo`",
        "effects of voice `melody`",
        "effects of the master",
    ] {
        assert!(changed.iter().any(|c| c.contains(label)), "{changed:?}");
    }
    let summary = ok(service.call(
        "inspect_project",
        json!({"view": "summary", "project": "village"}),
    ));
    let effects = summary["mix"]["effects"].as_array().unwrap();
    assert_eq!(effects.len(), 3);
    assert_eq!(
        effects[0]["effects"][0]["processor"]["kind"],
        json!("snes_echo")
    );

    let response = service.call(
        "render_preview",
        json!({"job_id": "render", "project": "village", "layout": {"kind": "excerpt", "start": 3840, "end": 7680}}),
    );
    assert_eq!(response.status, Status::Ok, "{response:#?}");
    let wav = std::fs::read(&response.artifacts[0].path).unwrap();
    let peak = wav[44..]
        .chunks(2)
        .map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs())
        .max()
        .unwrap();
    assert!(
        f64::from(peak) <= (32767.0 * 10f64.powf(-1.0 / 20.0)).round(),
        "{peak}"
    );

    ok(service.call(
        "export_project",
        json!({"job_id": "export-fx", "project": "village", "destination": "out/fx"}),
    ));
    let read =
        |dir: &str| std::fs::read(workspace.0.join(format!("out/{dir}/village.mid"))).unwrap();
    assert_eq!(read("dry"), read("fx"), "effects never change MIDI");
    let report: Value =
        serde_json::from_slice(&std::fs::read(workspace.0.join("out/fx/report.json")).unwrap())
            .unwrap();
    assert_eq!(report["mix"]["written_to_midi"], json!(false));
    assert!(
        report["mix"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i == "effects of the master")
    );
    assert!(
        report["not_in_midi"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().contains("effect chains"))
    );
    let bundle: tunemend::ProjectBundle = serde_json::from_slice(
        &std::fs::read(workspace.0.join("out/fx/village.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(tunemend::check_project_bundle(&bundle), vec![]);
    assert_eq!(
        bundle
            .revisions
            .last()
            .unwrap()
            .composition
            .mix
            .effects
            .len(),
        3
    );

    let capabilities = ok(service.call(
        "get_capabilities",
        json!({"sections": ["edit_operations", "mixer"]}),
    ));
    let operations = capabilities["edit_operations"].as_array().unwrap();
    for op in [
        "set_effects",
        "insert_effect",
        "remove_effect",
        "move_effect",
        "set_effect",
    ] {
        assert!(operations.iter().any(|o| o == op), "{op}");
    }
    assert!(capabilities["mixer"]["effects"]["processors"]["snes_echo"].is_string());
}
