//! DAW-style note operations: reproducible results and all-or-nothing refusals.
use serde_json::{Value, json};
use tunemend::composition::{Composition, Timebase};
use tunemend::diagnostic::DiagnosticCode;
use tunemend::edit::{EditRequest, apply_edit};

const MUSIC: Timebase = Timebase::Music { ppq: 480 };

fn base() -> Composition {
    serde_json::from_value(json!({
        "tempo_map": [{"tick": 0, "microseconds_per_quarter": 500000}],
        "voices": [{"id": "chords"}, {"id": "lead"}],
        "notes": [
            {"id": "c", "voice": "chords", "start": 0, "duration": 960, "pitch": 60, "velocity": 80},
            {"id": "e", "voice": "chords", "start": 0, "duration": 960, "pitch": 64, "velocity": 80},
            {"id": "g", "voice": "chords", "start": 0, "duration": 960, "pitch": 67, "velocity": 80},
            {"id": "l1", "voice": "lead", "start": 0, "duration": 240, "pitch": 61, "velocity": 40},
            {"id": "l2", "voice": "lead", "start": 480, "duration": 240, "pitch": 66, "velocity": 60},
            {"id": "l3", "voice": "lead", "start": 960, "duration": 240, "pitch": 70, "velocity": 100}
        ],
        "playback": {"end": 1920}
    }))
    .unwrap()
}

fn edit(operations: Value) -> Result<Composition, Vec<DiagnosticCode>> {
    edit_with(operations, json!({}), json!([]))
}

fn edit_with(
    operations: Value,
    scope: Value,
    protections: Value,
) -> Result<Composition, Vec<DiagnosticCode>> {
    let request: EditRequest = serde_json::from_value(json!({
        "job_id": "j", "base_revision": "r1", "scope": scope,
        "protections": protections, "operations": operations
    }))
    .unwrap();
    apply_edit(MUSIC, &base(), &request).map_err(|d| d.into_iter().map(|d| d.code).collect())
}

fn note<'a>(c: &'a Composition, id: &str) -> &'a tunemend::composition::Note {
    c.notes.iter().find(|n| n.id == id).unwrap()
}

#[test]
fn humanize_is_reproducible_bounded_and_independent_of_selection_order() {
    let op = |ids: Value, seed: u64| json!([{"op":"humanize_notes","ids":ids,"seed":seed,"timing":20,"velocity":10}]);
    let a = edit(op(json!(["l2", "l3"]), 7)).unwrap();
    let b = edit(op(json!(["l3", "l2"]), 7)).unwrap();
    assert_eq!(a, b);
    let c = edit(op(json!(["l2", "l3"]), 8)).unwrap();
    assert_ne!(a, c);
    for id in ["l2", "l3"] {
        let (before, after) = (note(&base(), id).clone(), note(&a, id).clone());
        assert!(before.start.abs_diff(after.start) <= 20);
        assert!(before.velocity.abs_diff(after.velocity) <= 10);
    }
    assert_eq!(note(&a, "l1"), note(&base(), "l1"));
}

#[test]
fn velocity_scaling_and_ramps_round_and_clamp() {
    let scaled =
        edit(json!([{"op":"scale_velocities","ids":["l1","l3"],"percent":150,"offset":5}]))
            .unwrap();
    assert_eq!(
        (note(&scaled, "l1").velocity, note(&scaled, "l3").velocity),
        (65, 127)
    );
    let ramp =
        edit(json!([{"op":"ramp_velocities","ids":["l3","l1","l2"],"from":10,"to":21}])).unwrap();
    let v: Vec<u8> = ["l1", "l2", "l3"]
        .iter()
        .map(|id| note(&ramp, id).velocity)
        .collect();
    assert_eq!(v, [10, 16, 21]);
    assert_eq!(
        edit(json!([{"op":"ramp_velocities","ids":["l1"],"from":0,"to":20}])).unwrap_err(),
        [DiagnosticCode::ValueOutOfRange]
    );
}

#[test]
fn legato_extends_to_the_next_start_and_refuses_a_gap_that_leaves_nothing() {
    let legato = edit(json!([{"op":"legato_notes","ids":["l1","l2","l3"],"gap":10}])).unwrap();
    let d: Vec<u64> = ["l1", "l2", "l3"]
        .iter()
        .map(|id| note(&legato, id).duration)
        .collect();
    assert_eq!(d, [470, 470, 240]);
    assert_eq!(
        edit(json!([{"op":"legato_notes","ids":["l1","l2"],"gap":480}])).unwrap_err(),
        [DiagnosticCode::NonPositiveDuration]
    );
}

#[test]
fn strum_spreads_chords_by_pitch_order_keeping_ends() {
    let up = edit(json!([{"op":"strum_notes","ids":["g","c","e"],"step":30}])).unwrap();
    let starts: Vec<u64> = ["c", "e", "g"]
        .iter()
        .map(|id| note(&up, id).start)
        .collect();
    assert_eq!(starts, [0, 30, 60]);
    assert!(
        ["c", "e", "g"]
            .iter()
            .all(|id| note(&up, id).end() == Some(960))
    );
    let down = edit(json!([{"op":"strum_notes","ids":["c","e","g"],"step":30,"direction":"down"}]))
        .unwrap();
    assert_eq!(note(&down, "c").start, 60);
    // A single note is not a chord and stays put.
    assert_eq!(note(&up, "l1"), note(&base(), "l1"));
}

#[test]
fn snap_to_scale_supports_names_intervals_and_directions() {
    let ids = json!(["l1", "l2", "l3"]);
    let major = edit(json!([{"op":"snap_to_scale","ids":ids,"root":0,"scale":"major"}])).unwrap();
    // 61 (C#) ties between C and D and goes down; 66 (F#) ties and goes down; 70 (A#) goes down to A.
    let p: Vec<u8> = ["l1", "l2", "l3"]
        .iter()
        .map(|id| note(&major, id).pitch)
        .collect();
    assert_eq!(p, [60, 65, 69]);
    let up =
        edit(json!([{"op":"snap_to_scale","ids":ids,"root":0,"scale":"major","direction":"up"}]))
            .unwrap();
    assert_eq!(note(&up, "l3").pitch, 71);
    let custom =
        edit(json!([{"op":"snap_to_scale","ids":["l2"],"root":2,"intervals":[0,7]}])).unwrap();
    assert_eq!(note(&custom, "l2").pitch, 69);
    for bad in [
        json!({"root":0,"scale":"blues"}),
        json!({"root":12,"scale":"major"}),
        json!({"root":0}),
    ] {
        let mut op = bad;
        op["op"] = json!("snap_to_scale");
        op["ids"] = json!(["l1"]);
        assert!(edit(json!([op])).is_err());
    }
}

#[test]
fn stretch_scales_around_an_anchor_with_exact_rounding() {
    let half = edit(json!([{"op":"stretch_notes","ids":["l2","l3"],"anchor":480,"numerator":1,"denominator":2}])).unwrap();
    assert_eq!(
        (note(&half, "l2").start, note(&half, "l2").duration),
        (480, 120)
    );
    assert_eq!(
        (note(&half, "l3").start, note(&half, "l3").duration),
        (720, 120)
    );
    let thirds =
        edit(json!([{"op":"stretch_notes","ids":["l1"],"anchor":0,"numerator":2,"denominator":3}]))
            .unwrap();
    assert_eq!(note(&thirds, "l1").duration, 160);
    // Stretching before the anchor would need a negative start.
    assert!(
        edit(
            json!([{"op":"stretch_notes","ids":["l1"],"anchor":480,"numerator":3,"denominator":1}])
        )
        .is_err()
    );
}

#[test]
fn new_operations_obey_scope_protections_and_reject_duplicate_ids() {
    let op = json!([{"op":"scale_velocities","ids":["l1","c"],"percent":50}]);
    assert_eq!(
        edit_with(op.clone(), json!({"voices":["lead"]}), json!([])).unwrap_err(),
        [DiagnosticCode::OutOfScope]
    );
    assert_eq!(
        edit_with(op, json!({}), json!([{"voice":"chords"}])).unwrap_err(),
        [DiagnosticCode::ProtectedChange]
    );
    assert_eq!(
        edit(json!([{"op":"legato_notes","ids":["l1","l1"]}])).unwrap_err(),
        [DiagnosticCode::DuplicateId]
    );
}

#[test]
fn control_ramps_write_rounded_points_and_keep_points_outside() {
    let ramp = edit(json!([
        {"op":"set_control_points","voice":"chords","kind":"expression","points":[{"at":0,"value":10},{"at":1000,"value":50},{"at":1900,"value":90}]},
        {"op":"ramp_control_points","voice":"chords","kind":"expression","start":0,"end":960,"from":70,"to":100,"step":240}
    ]))
    .unwrap();
    let lane = ramp
        .control_lanes
        .iter()
        .find(|l| l.voice == "chords")
        .unwrap();
    let points: Vec<(u64, i32)> = lane.points.iter().map(|p| (p.at, p.value)).collect();
    assert_eq!(
        points,
        [
            (0, 70),
            (240, 78),
            (480, 85),
            (720, 93),
            (960, 100),
            (1000, 50),
            (1900, 90)
        ]
    );
    // Values outside the controller's range fail the composition check.
    assert!(edit(json!([{"op":"ramp_control_points","voice":"lead","kind":"volume","start":0,"end":480,"from":0,"to":200,"step":120}])).is_err());
    assert!(edit(json!([{"op":"ramp_control_points","voice":"lead","kind":"volume","start":480,"end":480,"from":0,"to":20,"step":120}])).is_err());
}

#[test]
fn edit_responses_flag_voices_whose_polyphony_grows() {
    let root = std::env::temp_dir().join(format!("tunemend-advisory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut service = tunemend::service::Service::new(&root);
    let created = service.call(
        "create_project",
        json!({"job_id":"c","project":"p","timebase":{"kind":"music","ppq":480},"composition":base()}),
    );
    assert_eq!(created.status, tunemend::service::Status::Ok, "{created:?}");
    let call = |service: &mut tunemend::service::Service, job: &str, base: &str, ops: Value| {
        service.call(
            "edit_project",
            json!({"project":"p","request":{"job_id":job,"base_revision":base,"operations":ops}}),
        )
    };
    let overlapping = call(
        &mut service,
        "o",
        "r1",
        json!([{"op":"update_notes","notes":[{"id":"l1","duration":600}]}]),
    );
    let advisory = &overlapping.result["advisories"][0];
    assert_eq!(
        (advisory["voice"].as_str(), advisory["after"].as_u64()),
        (Some("lead"), Some(2))
    );
    let unchanged = call(
        &mut service,
        "v",
        "r2",
        json!([{"op":"scale_velocities","ids":["l2"],"percent":90}]),
    );
    assert!(unchanged.result.get("advisories").is_none());
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn set_head_returns_to_a_kept_revision_after_a_diagnostic_branch() {
    let root = std::env::temp_dir().join(format!("tunemend-head-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut service = tunemend::service::Service::new(&root);
    let ok = |r: tunemend::service::ToolResponse| {
        assert_eq!(r.status, tunemend::service::Status::Ok, "{r:?}");
        r
    };
    ok(service.call(
        "create_project",
        json!({"job_id":"c","project":"p","timebase":{"kind":"music","ppq":480},"composition":base()}),
    ));
    let edit = |job: &str, base: &str, branch: bool| {
        json!({"project":"p","branch":branch,"request":{"job_id":job,"base_revision":base,
            "operations":[{"op":"scale_velocities","ids":["l1"],"percent":90}]}})
    };
    ok(service.call("edit_project", edit("keep", "r1", false)));
    ok(service.call("edit_project", edit("probe", "r1", true)));
    let head = |s: &mut tunemend::service::Service| {
        s.call("inspect_project", json!({"project":"p","view":"summary"}))
            .result["head_revision"]
            .clone()
    };
    assert_eq!(head(&mut service), "r3");
    let moved = ok(service.call(
        "set_head",
        json!({"job_id":"h","project":"p","revision":"r2"}),
    ));
    assert_eq!(moved.result["previous_head"], "r3");
    assert_eq!(head(&mut service), "r2");
    // Editing continues from the selected head without branch; unknown revisions refuse.
    ok(service.call("edit_project", edit("next", "r2", false)));
    let missing = service.call(
        "set_head",
        json!({"job_id":"m","project":"p","revision":"r9"}),
    );
    assert_eq!(missing.status, tunemend::service::Status::Blocked);
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}
