//! The Converter timeline built from a revision; no Converter is needed.

mod common;

use serde_json::json;
use tunemend::composition::{ControlKind, ControlLane, ControlPoint};
use tunemend::target::{ChipVoice, TargetMapping, build_timeline};

fn mapping(voices: &[&str]) -> TargetMapping {
    TargetMapping {
        frames_per_second: 60,
        voices: voices
            .iter()
            .map(|v| ChipVoice { voice: v.to_string(), chip: "ym2612".into(), patch: "p".into(), pan: 192, role: None })
            .collect(),
        patches: [("p".to_owned(), json!({"chip": "ym2612", "algorithm": 7, "operators": vec![[1, 0, 31, 0, 0, 15, 0]; 4]}))].into(),
        omit: vec![],
        quantize: false,
    }
}

#[test]
fn music_on_the_frame_grid_becomes_a_frame_timeline() {
    let bundle = common::village();
    let bgm = &bundle.revisions[1].composition;
    let built = build_timeline(
        "village@r2",
        ("village@r2", "{}"),
        bundle.project.timebase,
        bgm,
        &mapping(&["melody", "accompaniment", "bass"]),
    )
    .unwrap();
    // 4 s intro and 10.4 s loop at 60 frames per second.
    assert_eq!(built.timeline["end_tick"], 864);
    assert_eq!(built.timeline["loop"], json!([240, 864]));
    let melody = &built.timeline["tracks"][0]["notes"][0];
    assert_eq!(
        melody,
        &json!({"start": 240, "duration": 30, "pitch_cents": 7600, "velocity": 96, "instrument": "p"})
    );
    let kinds: Vec<&str> = built
        .losses
        .iter()
        .map(|l| l["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["program_replaced_by_patch_mapping"; 3]);
    assert_eq!(
        built.voice_map["melody"],
        json!({"role": "melody", "chip": "ym2612", "patch": "p", "pan": 192})
    );
}

#[test]
fn unmapped_voices_off_grid_notes_and_chords_are_refused_or_reported() {
    let bundle = common::chime();
    let chime = &bundle.revisions[0].composition;
    let timebase = bundle.project.timebase;

    let mut unmapped = mapping(&[]);
    let refused = build_timeline("t", ("t", ""), timebase, chime, &unmapped).unwrap_err();
    assert!(
        refused[0].message.contains("`lead` has no chip voice"),
        "{refused:?}"
    );
    unmapped.omit = vec!["lead".into()];
    assert_eq!(
        build_timeline("t", ("t", ""), timebase, chime, &unmapped)
            .unwrap()
            .losses[0]["kind"],
        "voice_omitted"
    );

    // 60 ms is 3.6 frames: refused unless quantizing was asked for.
    let refused = build_timeline("t", ("t", ""), timebase, chime, &mapping(&["lead"])).unwrap_err();
    assert!(
        refused.iter().any(|d| d.message.contains("18/5 frames")),
        "{refused:?}"
    );
    let mut quantized = mapping(&["lead"]);
    quantized.quantize = true;
    let built = build_timeline("t", ("t", ""), timebase, chime, &quantized).unwrap();
    let kinds: Vec<&str> = built
        .losses
        .iter()
        .map(|l| l["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"time_quantized")
            && kinds.contains(&"pitch_bend_not_transferred")
            && kinds.contains(&"level_control_not_transferred"),
        "{kinds:?}"
    );

    let mut chord = bundle.revisions[0].composition.clone();
    chord.notes[1].start = 0;
    chord.control_lanes.push(ControlLane {
        voice: "lead".into(),
        kind: ControlKind::Pan,
        points: vec![ControlPoint { at: 0, value: 64 }],
    });
    let refused = build_timeline("t", ("t", ""), timebase, &chord, &quantized).unwrap_err();
    assert!(
        refused
            .iter()
            .any(|d| d.message.contains("one note at a time")),
        "{refused:?}"
    );
}
