//! Effect parameter automation: sample positions of steps and ramps in full,
//! loop and excerpt layouts, refusals, scope, and reporting through the service.

mod common;

use serde_json::{Value, json};
use tunemend::composition::{Composition, Timebase};
use tunemend::edit::{EditRequest, apply_edit};
use tunemend::render::{RenderedAudio, render};
use tunemend::review::RenderLayout;
use tunemend::service::{Service, Status, ToolResponse};
use tunemend::soundfont::reference_soundfont;

const RATE: u32 = 44_100;
const EFFECT: Timebase = Timebase::Effect;
const MUSIC: Timebase = Timebase::Music { ppq: 480 };

fn edit_in(
    timebase: Timebase,
    base: &Composition,
    request: Value,
) -> Result<Composition, Vec<(String, String)>> {
    let mut request = request;
    request["job_id"] = json!("job");
    request["base_revision"] = json!("r1");
    let request: EditRequest = serde_json::from_value(request).unwrap();
    apply_edit(timebase, base, &request).map_err(|diagnostics| {
        diagnostics
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
    })
}

fn with_ops(timebase: Timebase, base: &Composition, operations: Value) -> Composition {
    edit_in(
        timebase,
        base,
        json!({"scope": {"buses": true}, "operations": operations}),
    )
    .unwrap()
}

fn refused(timebase: Timebase, base: &Composition, operations: Value) -> Vec<(String, String)> {
    edit_in(
        timebase,
        base,
        json!({"scope": {"buses": true}, "operations": operations}),
    )
    .unwrap_err()
}

fn lead() -> Value {
    json!({"kind": "voice", "id": "lead"})
}

/// A sustained square tone in an effect-timebase piece: one tick is 1 µs.
fn tone(loop_start: Option<u64>) -> Composition {
    let mut playback = json!({"end": 400000});
    if let Some(start) = loop_start {
        playback["loop_start"] = json!(start);
    }
    let notes = match loop_start {
        Some(_) => json!([
            {"id": "intro", "voice": "lead", "start": 0, "duration": 90000, "pitch": 69, "velocity": 100},
            {"id": "body", "voice": "lead", "start": 100000, "duration": 290000, "pitch": 69, "velocity": 100}]),
        None => json!([
            {"id": "body", "voice": "lead", "start": 0, "duration": 390000, "pitch": 69, "velocity": 100}]),
    };
    serde_json::from_value(json!({
        "voices": [{"id": "lead"}],
        "notes": notes,
        "control_lanes": [{"voice": "lead", "kind": "program", "points": [{"at": 0, "value": 80}]}],
        "playback": playback,
    }))
    .unwrap()
}

/// A compressor that never compresses: its output is input × makeup gain.
fn makeup() -> Value {
    json!({"op": "insert_effect", "target": lead(), "effect": {"id": "gain", "processor":
        {"kind": "compressor", "threshold_db": 0.0, "ratio": 1.0, "attack_ms": 0.0, "release_ms": 1.0}}})
}

fn lane(points: Value) -> Value {
    json!({"op": "set_effect_automation", "target": lead(), "effect": "gain",
        "parameter": "makeup_db", "points": points})
}

fn play(composition: &Composition, layout: RenderLayout) -> RenderedAudio {
    render(
        EFFECT,
        composition,
        &layout,
        None,
        &reference_soundfont(),
        RATE,
    )
    .unwrap()
}

const FULL: RenderLayout = RenderLayout::Full { loop_passes: 1 };

/// What the compressor writes for a dry sample at a makeup of `db`.
fn gained(dry: f32, db: f64) -> f32 {
    (f64::from(dry) * 10f64.powf(db / 20.0)) as f32
}

#[test]
fn a_step_lands_on_the_first_sample_at_or_after_its_time() {
    let base = with_ops(EFFECT, &tone(None), json!([makeup()]));
    let dry = play(&base, FULL);
    // 250000 µs is sample 11025 exactly; 250001 µs is 11025.044, so 11026.
    for (at, first) in [(250000u64, 11025usize), (250001, 11026)] {
        let stepped = with_ops(
            EFFECT,
            &base,
            json!([lane(json!([{"at": at, "value": 6.0}]))]),
        );
        let audio = play(&stepped, FULL);
        assert_eq!(audio.content_samples, dry.content_samples);
        for (side, (out, reference)) in [(&audio.left, &dry.left), (&audio.right, &dry.right)]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                out[..first],
                reference[..first],
                "side {side} before {first}"
            );
            for n in first..dry.content_samples as usize {
                assert_eq!(out[n], gained(reference[n], 6.0), "side {side} sample {n}");
            }
        }
        assert_ne!(audio.left[first], dry.left[first]);
    }
}

#[test]
fn a_linear_ramp_moves_with_exact_sample_positions() {
    let base = with_ops(EFFECT, &tone(None), json!([makeup()]));
    let dry = play(&base, FULL);
    let ramped = with_ops(
        EFFECT,
        &base,
        json!([lane(
            json!([{"at": 100000, "value": 0.0, "shape": "linear"}, {"at": 200000, "value": 12.0}])
        )]),
    );
    let audio = play(&ramped, FULL);
    // 0.1 s and 0.2 s are samples 4410 and 8820.
    let (start, end) = (4410.0, 8820.0);
    for n in 0..dry.content_samples as usize {
        let db = if (n as f64) < start {
            0.0
        } else if (n as f64) < end {
            12.0 * (n as f64 - start) / (end - start)
        } else {
            12.0
        };
        assert_eq!(audio.left[n], gained(dry.left[n], db), "sample {n}");
    }
}

#[test]
fn excerpts_start_mid_ramp_and_loop_passes_replay_the_lane() {
    // Excerpt 150000–350000 µs: the ramp is at 6 dB at its start and reaches
    // 12 dB 50000 µs (2205 samples) later.
    let base = with_ops(EFFECT, &tone(None), json!([makeup()]));
    let ramped = with_ops(
        EFFECT,
        &base,
        json!([lane(
            json!([{"at": 100000, "value": 0.0, "shape": "linear"}, {"at": 200000, "value": 12.0}])
        )]),
    );
    let layout = RenderLayout::Excerpt {
        start: 150000,
        end: 350000,
    };
    let (dry, audio) = (play(&base, layout), play(&ramped, layout));
    for n in 0..dry.content_samples as usize {
        let db = if n < 2205 {
            6.0 + 6.0 * n as f64 / 2205.0
        } else {
            12.0
        };
        assert_eq!(audio.left[n], gained(dry.left[n], db), "excerpt sample {n}");
    }

    // Loop 100000–400000 µs, a step at 250000 µs: pass 1 steps at sample
    // 11025, pass 2 starts at 0.4 s (17640) back at 0 dB and steps at 0.55 s.
    let looped = with_ops(EFFECT, &tone(Some(100000)), json!([makeup()]));
    let stepped = with_ops(
        EFFECT,
        &looped,
        json!([lane(
            json!([{"at": 100000, "value": 0.0}, {"at": 250000, "value": 6.0}])
        )]),
    );
    let two = RenderLayout::Full { loop_passes: 2 };
    let (dry, audio) = (play(&looped, two), play(&stepped, two));
    assert_eq!(audio.content_samples, 2 * 17640 - 4410);
    for n in 0..audio.content_samples as usize {
        let db = if (11025..17640).contains(&n) || n >= 24255 {
            6.0
        } else {
            0.0
        };
        assert_eq!(audio.left[n], gained(dry.left[n], db), "loop sample {n}");
    }
    // The value at the end differs from the loop start only because a point
    // sits at loop_start; without it the lane is refused.
    assert_eq!(
        refused(
            EFFECT,
            &looped,
            json!([lane(json!([{"at": 250000, "value": 6.0}]))])
        ),
        vec![(
            "loop_state_mismatch".to_owned(),
            "/result/mix/effect_automation/0".to_owned()
        )]
    );
}

#[test]
fn an_eq_step_changes_nothing_before_its_sample() {
    let eq = json!({"op": "insert_effect", "target": lead(), "effect": {"id": "tone", "processor":
        {"kind": "eq", "bands": [{"filter": "peaking", "frequency_hz": 440.0, "gain_db": -6.0, "q": 1.0}]}}});
    let base = with_ops(EFFECT, &tone(None), json!([eq]));
    let stepped = with_ops(
        EFFECT,
        &base,
        json!([{"op": "set_effect_automation", "target": lead(), "effect": "tone", "parameter": "gain_db",
            "band": 0, "points": [{"at": 250000, "value": 6.0}]}]),
    );
    let (dry, audio) = (play(&base, FULL), play(&stepped, FULL));
    assert_eq!(audio.left[..11025], dry.left[..11025]);
    assert_ne!(audio.left[11025..11100], dry.left[11025..11100]);
}

#[test]
fn an_exponential_frequency_sweep_renders_apart_from_a_linear_one() {
    let eq = json!({"op": "insert_effect", "target": lead(), "effect": {"id": "tone", "processor":
        {"kind": "eq", "bands": [{"filter": "low_pass", "frequency_hz": 200.0, "q": 0.71}]}}});
    let base = with_ops(EFFECT, &tone(None), json!([eq]));
    let sweep = |shape: &str, to: f64| {
        with_ops(
            EFFECT,
            &base,
            json!([{"op": "set_effect_automation", "target": lead(), "effect": "tone",
                "parameter": "frequency_hz", "band": 0,
                "points": [{"at": 100000, "value": 200.0, "shape": shape}, {"at": 300000, "value": to}]}]),
        )
    };
    let dry = play(&base, FULL);
    let (exponential, linear) = (
        play(&sweep("exponential", 12800.0), FULL),
        play(&sweep("linear", 12800.0), FULL),
    );
    // Both hold the static 200 Hz before the ramp starts at sample 4410.
    assert_eq!(exponential.left[..4410], dry.left[..4410]);
    assert_eq!(linear.left[..4410], dry.left[..4410]);
    assert_ne!(exponential.left[4500..13230], linear.left[4500..13230]);
    // An exponential ramp between equal values is the static setting.
    assert_eq!(play(&sweep("exponential", 200.0), FULL).left, dry.left);
    // A compressor ratio may ramp exponentially too.
    let compressed = with_ops(EFFECT, &tone(None), json!([makeup()]));
    with_ops(
        EFFECT,
        &compressed,
        json!([{"op": "set_effect_automation", "target": lead(), "effect": "gain", "parameter": "ratio",
            "points": [{"at": 100000, "value": 1.0, "shape": "exponential"}, {"at": 200000, "value": 8.0}]}]),
    );
}

fn village() -> Composition {
    common::village().revisions[0].composition.clone()
}

fn melody() -> Value {
    json!({"kind": "voice", "id": "melody"})
}

fn melody_eq() -> Value {
    json!({"op": "insert_effect", "target": melody(), "effect": {"id": "tone", "processor": {"kind": "eq", "bands": [
        {"filter": "high_pass", "frequency_hz": 80.0, "q": 0.71},
        {"filter": "peaking", "frequency_hz": 2500.0, "gain_db": 3.0, "q": 1.2}]}}})
}

fn eq_lane(parameter: &str, band: Option<u8>, points: Value) -> Value {
    let mut op = json!({"op": "set_effect_automation", "target": melody(), "effect": "tone",
        "parameter": parameter, "points": points});
    if let Some(band) = band {
        op["band"] = json!(band);
    }
    op
}

fn one(code: &str, location: &str) -> Vec<(String, String)> {
    vec![(code.to_owned(), location.to_owned())]
}

#[test]
fn lanes_are_refused_outside_their_parameter() {
    let bgm = with_ops(MUSIC, &village(), json!([melody_eq()]));
    let points = json!([{"at": 3840, "value": 3.0, "shape": "linear"}, {"at": 7680, "value": 9.0}, {"at": 11520, "value": 3.0}]);
    let ok = with_ops(
        MUSIC,
        &bgm,
        json!([eq_lane("gain_db", Some(1), points.clone())]),
    );
    assert_eq!(ok.mix.effect_automation.len(), 1);
    let at = |rest: &str| format!("/result/mix/effect_automation/0{rest}");

    let cases = [
        (
            eq_lane("gain_db", Some(1), json!([{"at": 3840, "value": 30.0}])),
            one("value_out_of_range", &at("/points/0/value")),
        ),
        (
            eq_lane("frequency_hz", Some(1), json!([{"at": 3840, "value": 5.0}])),
            one("value_out_of_range", &at("/points/0/value")),
        ),
        (
            eq_lane("threshold_db", None, json!([{"at": 3840, "value": -6.0}])),
            one("invalid_range", &at("/parameter")),
        ),
        (
            eq_lane("gain_db", None, points.clone()),
            one("invalid_range", &at("/band")),
        ),
        (
            eq_lane("gain_db", Some(2), points.clone()),
            one("invalid_range", &at("/band")),
        ),
        // Band 0 is a high-pass, which has no gain.
        (
            eq_lane("gain_db", Some(0), points.clone()),
            one("invalid_range", &at("/band")),
        ),
        (
            json!({"op": "set_effect_automation", "target": melody(), "effect": "missing",
                "parameter": "gain_db", "band": 1, "points": points}),
            one("unknown_reference", &at("/effect")),
        ),
        (
            eq_lane("gain_db", Some(1), json!([{"at": 15360, "value": 3.0}])),
            one("event_beyond_end", &at("/points/0/at")),
        ),
        (
            eq_lane(
                "gain_db",
                Some(1),
                json!([{"at": 3840, "value": 3.0}, {"at": 4000, "value": 4.0}, {"at": 4000, "value": 3.0}]),
            ),
            one("control_points_unordered", &at("/points/2/at")),
        ),
        // 3.0 at the loop start (static), 9.0 at the end.
        (
            eq_lane("gain_db", Some(1), json!([{"at": 5000, "value": 9.0}])),
            one("loop_state_mismatch", &at("")),
        ),
        (
            json!({"op": "set_effect_automation", "target": melody(), "effect": "tone",
                "parameter": "frequency_hz", "band": 1, "points": []}),
            one("unknown_reference", "/operations/0"),
        ),
        // Exponential ramps only on ratio scales (frequency_hz, ratio).
        (
            eq_lane(
                "gain_db",
                Some(1),
                json!([{"at": 3840, "value": 3.0, "shape": "exponential"}, {"at": 7680, "value": 9.0}, {"at": 11520, "value": 3.0}]),
            ),
            one("invalid_range", &at("/points/0/shape")),
        ),
        (
            json!({"op": "set_mix_automation", "target": melody(), "parameter": "volume",
                "points": [{"at": 3840, "value": -6.0, "shape": "exponential"}, {"at": 7680, "value": -3.0}, {"at": 11520, "value": -6.0}]}),
            one("invalid_range", "/result/mix/automation/0/points/0/shape"),
        ),
    ];
    for (operation, expected) in cases {
        assert_eq!(
            refused(MUSIC, &bgm, json!([operation.clone()])),
            expected,
            "{operation}"
        );
    }

    // Register parameters take whole numbers; a band belongs to EQ only.
    let echo = with_ops(
        MUSIC,
        &village(),
        json!([{"op": "insert_effect", "target": melody(), "effect": {"id": "echo", "processor":
            {"kind": "snes_echo", "fir": [127, 0, 0, 0, 0, 0, 0, 0], "delay": 2, "feedback": 40, "volume_left": 40, "volume_right": 40}}}]),
    );
    let echo_lane = |parameter: &str, band: Option<u8>, value: f64| {
        let mut op = json!({"op": "set_effect_automation", "target": melody(), "effect": "echo",
            "parameter": parameter, "points": [{"at": 3840, "value": value}, {"at": 11520, "value": 40.0}]});
        if let Some(band) = band {
            op["band"] = json!(band);
        }
        op
    };
    with_ops(MUSIC, &echo, json!([echo_lane("volume_left", None, 100.0)]));
    for (operation, expected) in [
        (
            echo_lane("volume_left", None, 40.5),
            one("value_out_of_range", &at("/points/0/value")),
        ),
        (
            echo_lane("feedback", None, -129.0),
            one("value_out_of_range", &at("/points/0/value")),
        ),
        (
            echo_lane("volume_right", Some(0), 20.0),
            one("invalid_range", &at("/band")),
        ),
    ] {
        assert_eq!(refused(MUSIC, &echo, json!([operation])), expected);
    }
}

#[test]
fn removing_or_retyping_an_automated_effect_is_explicit() {
    let points = json!([{"at": 3840, "value": 3.0, "shape": "linear"}, {"at": 7680, "value": 9.0}, {"at": 11520, "value": 3.0}]);
    let automated = with_ops(
        MUSIC,
        &village(),
        json!([melody_eq(), eq_lane("gain_db", Some(1), points.clone())]),
    );
    let lane_location = "/result/mix/effect_automation/0";
    // Removing the effect leaves a dangling lane: refused unless cleared too.
    assert_eq!(
        refused(
            MUSIC,
            &automated,
            json!([{"op": "remove_effect", "target": melody(), "id": "tone"}])
        ),
        one("unknown_reference", &format!("{lane_location}/effect"))
    );
    let cleared = with_ops(
        MUSIC,
        &automated,
        json!([{"op": "remove_effect", "target": melody(), "id": "tone"},
               eq_lane("gain_db", Some(1), json!([]))]),
    );
    assert_eq!(
        serde_json::to_string(&cleared).unwrap(),
        serde_json::to_string(&village()).unwrap()
    );
    // set_effects without the id is a removal too.
    assert_eq!(
        refused(
            MUSIC,
            &automated,
            json!([{"op": "set_effects", "target": melody(), "effects": []}])
        ),
        one("unknown_reference", &format!("{lane_location}/effect"))
    );
    // Replacing the settings so the band disappears, or loses its gain.
    for bands in [
        json!([{"filter": "peaking", "frequency_hz": 2500.0, "gain_db": 3.0, "q": 1.2}]),
        json!([{"filter": "high_pass", "frequency_hz": 80.0, "q": 0.71},
               {"filter": "low_pass", "frequency_hz": 9000.0, "q": 0.71}]),
    ] {
        assert_eq!(
            refused(
                MUSIC,
                &automated,
                json!([{"op": "set_effect", "target": melody(), "id": "tone",
                    "processor": {"kind": "eq", "bands": bands}}])
            ),
            one("invalid_range", &format!("{lane_location}/band"))
        );
    }
    // Moving keeps the lane on its effect.
    let moved = with_ops(
        MUSIC,
        &automated,
        json!([{"op": "insert_effect", "target": melody(), "effect": {"id": "verb", "processor":
                   {"kind": "reverb", "room_size_percent": 50.0, "damping_percent": 50.0, "mix_percent": 20.0}}},
               {"op": "move_effect", "target": melody(), "id": "verb", "index": 0}]),
    );
    assert_eq!(moved.mix.effect_automation, automated.mix.effect_automation);
    assert_eq!(moved.mix.effects[0].effects[1].id, "tone");

    // A lane on a parameter both kinds have must not follow the id silently.
    let delayed = with_ops(
        MUSIC,
        &village(),
        json!([{"op": "insert_effect", "target": melody(), "effect": {"id": "wet", "processor":
                   {"kind": "delay", "time": {"unit": "ms", "ms": 200.0}, "mix_percent": 30.0}}},
               {"op": "set_effect_automation", "target": melody(), "effect": "wet", "parameter": "mix_percent",
                   "points": [{"at": 3840, "value": 30.0}, {"at": 7680, "value": 60.0}, {"at": 11520, "value": 30.0}]}]),
    );
    let reverb = json!([{"op": "set_effects", "target": melody(), "effects": [{"id": "wet", "processor":
        {"kind": "reverb", "room_size_percent": 50.0, "damping_percent": 50.0, "mix_percent": 30.0}}]}]);
    assert_eq!(
        refused(MUSIC, &delayed, reverb.clone()),
        one("invalid_range", "/result/mix/effect_automation/0")
    );
    let mut retyped = reverb.as_array().unwrap().clone();
    retyped.push(json!({"op": "set_effect_automation", "target": melody(), "effect": "wet",
        "parameter": "mix_percent", "points": [{"at": 3840, "value": 20.0}, {"at": 11520, "value": 20.0}]}));
    with_ops(MUSIC, &delayed, Value::Array(retyped));

    // Removing a bus removes its chain and its lanes with it.
    let bus = with_ops(
        MUSIC,
        &village(),
        json!([{"op": "add_bus", "bus": {"id": "echo"}},
               {"op": "set_effects", "target": {"kind": "bus", "id": "echo"}, "effects": [{"id": "dl", "processor":
                   {"kind": "delay", "time": {"unit": "ms", "ms": 200.0}, "mix_percent": 30.0}}]},
               {"op": "set_effect_automation", "target": {"kind": "bus", "id": "echo"}, "effect": "dl",
                   "parameter": "feedback_percent", "points": [{"at": 3840, "value": 50.0}]}]),
    );
    assert_eq!(bus.mix.effect_automation.len(), 1);
    let gone = with_ops(MUSIC, &bus, json!([{"op": "remove_bus", "id": "echo"}]));
    assert!(gone.mix.is_empty());
}

#[test]
fn lanes_follow_voice_scope_and_bus_scope() {
    let bgm = with_ops(
        MUSIC,
        &village(),
        json!([melody_eq(), {"op": "insert_effect", "target": {"kind": "master"}, "effect":
            {"id": "limit", "processor": {"kind": "limiter", "ceiling_db": -1.0, "release_ms": 80.0, "lookahead_ms": 5.0}}}]),
    );
    let ramp = eq_lane(
        "gain_db",
        Some(1),
        json!([{"at": 4800, "value": 3.0, "shape": "linear"}, {"at": 5760, "value": 9.0}, {"at": 6720, "value": 3.0}]),
    );
    let edit = |request: Value| edit_in(MUSIC, &bgm, request);
    assert!(edit(json!({"scope": {"voices": ["melody"], "range": {"start": 4800, "end": 6721}}, "operations": [ramp.clone()]})).is_ok());
    // Each refused span reports once; the codes are what matters here.
    let out_of_scope = |result: Result<Composition, Vec<(String, String)>>| {
        let mut codes: Vec<String> = result
            .unwrap_err()
            .iter()
            .map(|(code, _)| code.clone())
            .collect();
        codes.dedup();
        codes
    };
    assert_eq!(
        out_of_scope(edit(
            json!({"scope": {"voices": ["bass"]}, "operations": [ramp.clone()]})
        )),
        vec!["out_of_scope"]
    );
    assert_eq!(
        out_of_scope(edit(
            json!({"scope": {"range": {"start": 5000, "end": 7000}}, "operations": [ramp.clone()]})
        )),
        vec!["out_of_scope"]
    );
    assert_eq!(
        out_of_scope(edit(
            json!({"protections": [{"voice": "melody", "range": {"start": 6000, "end": 6100}}], "operations": [ramp]})
        )),
        vec!["protected_change"]
    );
    let master = json!({"op": "set_effect_automation", "target": {"kind": "master"}, "effect": "limit",
        "parameter": "ceiling_db", "points": [{"at": 4800, "value": -3.0}, {"at": 6720, "value": -1.0}]});
    assert_eq!(
        out_of_scope(edit(json!({"operations": [master.clone()]}))),
        vec!["out_of_scope"]
    );
    assert!(edit(json!({"scope": {"buses": true, "range": {"start": 4800, "end": 6721}}, "operations": [master.clone()]})).is_ok());
    assert_eq!(
        out_of_scope(edit(
            json!({"scope": {"buses": true, "range": {"start": 5000, "end": 7000}}, "operations": [master]})
        )),
        vec!["out_of_scope"]
    );
}

fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{tool}: {response:#?}");
    response
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn service_reports_effect_automation_everywhere_mixer_automation_appears() {
    let root = std::env::temp_dir().join(format!(
        "tunemend-test-effect-automation-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut s = Service::new(&root);
    let bundle = common::village();
    ok(
        &mut s,
        "create_project",
        json!({"job_id": "create", "project": "village", "timebase": bundle.project.timebase,
            "composition": bundle.revisions[0].composition}),
    );
    ok(
        &mut s,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "eq", "base_revision": "r1", "operations": [melody_eq()]}}),
    );
    ok(
        &mut s,
        "export_project",
        json!({"job_id": "export-static", "project": "village", "destination": "out/static", "render_audio": "none"}),
    );
    let edited = ok(
        &mut s,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "sweep", "base_revision": "r2",
            "rationale": "open the presence band through the second phrase",
            "operations": [eq_lane("gain_db", Some(1), json!([{"at": 3840, "value": 3.0, "shape": "linear"}, {"at": 7680, "value": 9.0}, {"at": 11520, "value": 3.0}]))]}}),
    );
    let label = "gain_db band 1 automation of effect `tone` on voice `melody`";
    assert_eq!(
        strings(&edited.result["difference"]["changed_mix"]),
        vec![label]
    );
    let compared = ok(
        &mut s,
        "compare_revisions",
        json!({"project": "village", "from": "r1", "to": "r3"}),
    );
    assert!(strings(&compared.result["changed_mix"]).contains(&label.to_owned()));
    let summary = ok(
        &mut s,
        "inspect_project",
        json!({"view": "summary", "project": "village"}),
    );
    assert_eq!(
        summary.result["mix"]["effect_automation"],
        json!([{"target": melody(), "effect": "tone", "parameter": "gain_db", "band": 1, "points": 3}])
    );
    let events = ok(
        &mut s,
        "inspect_project",
        json!({"view": "events", "project": "village", "range": {"start": 0, "end": 7681}}),
    );
    let lanes = events.result["effect_automation"].as_array().unwrap();
    assert_eq!(lanes[0]["points"].as_array().unwrap().len(), 2);

    let render = ok(
        &mut s,
        "render_preview",
        json!({"job_id": "render", "project": "village", "layout": {"kind": "excerpt", "start": 3840, "end": 7680}}),
    );
    assert!(std::path::Path::new(&render.artifacts[0].path).exists());
    ok(
        &mut s,
        "export_project",
        json!({"job_id": "export-sweep", "project": "village", "destination": "out/sweep", "render_audio": "none"}),
    );
    let read =
        |dir: &str, file: &str| std::fs::read(root.join(format!("out/{dir}/{file}"))).unwrap();
    assert_eq!(
        read("static", "village.mid"),
        read("sweep", "village.mid"),
        "automation never changes MIDI"
    );
    let report: Value = serde_json::from_slice(&read("sweep", "report.json")).unwrap();
    assert!(strings(&report["mix"]["items"]).contains(&label.to_owned()));
    assert!(
        strings(&report["not_in_midi"])
            .iter()
            .any(|line| line.starts_with("effect parameter automation"))
    );
    let bundle: tunemend::ProjectBundle =
        serde_json::from_slice(&read("sweep", "village.project.json")).unwrap();
    assert_eq!(tunemend::check_project_bundle(&bundle), vec![]);

    let capabilities = ok(
        &mut s,
        "get_capabilities",
        json!({"sections": ["edit_operations", "mixer"]}),
    );
    let automation = &capabilities.result["mixer"]["effects"]["automation"];
    assert!(
        automation["shapes"]
            .as_str()
            .unwrap()
            .contains("exponential")
    );
    assert!(
        strings(&capabilities.result["edit_operations"]).contains(&"set_effect_automation".into())
    );
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}
