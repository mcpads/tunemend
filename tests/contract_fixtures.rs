use tunemend::composition::{ControlKind, ControlLane, ControlPoint, Note};
use tunemend::edit::{EditOperation, NoteUpdate};
use tunemend::export::MidiTiming;
use tunemend::review::{FeedbackState, Mark, RenderLayout};
use tunemend::{
    DiagnosticCode as Code, ProjectBundle, check_project_bundle, project_bundle_schema,
};

const FIXTURES: [&str; 2] = ["village-loop.json", "pickup-chime.json"];

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn village() -> ProjectBundle {
    serde_json::from_str(&read_fixture("village-loop.json")).unwrap()
}

fn chime() -> ProjectBundle {
    serde_json::from_str(&read_fixture("pickup-chime.json")).unwrap()
}

fn note_mut<'a>(bundle: &'a mut ProjectBundle, revision: usize, id: &str) -> &'a mut Note {
    bundle.revisions[revision]
        .composition
        .notes
        .iter_mut()
        .find(|n| n.id == id)
        .unwrap()
}

/// Adds `update` to revision 1's request and applies it to revision 1's stored
/// content, so only the rule under test can fail.
fn also_update_in_r2(bundle: &mut ProjectBundle, update: NoteUpdate) {
    let request = bundle.revisions[1].edit_request.as_mut().unwrap();
    let EditOperation::UpdateNotes { notes } = &mut request.operations[0] else {
        panic!("fixture changed")
    };
    notes.push(update.clone());
    note_mut(bundle, 1, &update.id).pitch = update.pitch.unwrap();
}

#[test]
fn creative_fixtures_satisfy_the_contract() {
    for name in FIXTURES {
        let bundle: ProjectBundle = serde_json::from_str(&read_fixture(name)).unwrap();
        assert_eq!(check_project_bundle(&bundle), vec![], "{name}");
    }
}

#[test]
fn generated_schema_matches_the_parser() {
    let schema = serde_json::to_value(project_bundle_schema()).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for name in FIXTURES {
        let document: serde_json::Value = serde_json::from_str(&read_fixture(name)).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&document)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{name}: {errors:?}");
    }

    let mut unknown_field: serde_json::Value =
        serde_json::from_str(&read_fixture("pickup-chime.json")).unwrap();
    unknown_field["revisions"][0]["composition"]["notes"][0]["sustain"] = 127.into();
    assert!(!validator.is_valid(&unknown_field));
    assert!(serde_json::from_value::<ProjectBundle>(unknown_field).is_err());
}

type Mutation = fn(&mut ProjectBundle);
/// Name, fixture, mutation, expected code and expected location.
type Violation = (
    &'static str,
    fn() -> ProjectBundle,
    Mutation,
    Code,
    &'static str,
);

#[test]
fn contract_violations_are_rejected_at_their_location() {
    let cases: Vec<Violation> = vec![
        (
            "zero-length note",
            village,
            |b| note_mut(b, 0, "mel-3-1").duration = 0,
            Code::NonPositiveDuration,
            "/revisions/0/composition/notes/14/duration",
        ),
        (
            "same pitch overlaps in a voice",
            village,
            |b| {
                let mut copy = note_mut(b, 0, "mel-3-1").clone();
                copy.id = "mel-3-1-copy".into();
                copy.start += 240;
                b.revisions[0].composition.notes.push(copy);
            },
            Code::OverlappingNotes,
            "/revisions/0/composition/notes/67",
        ),
        (
            "note held across the loop start",
            village,
            |b| note_mut(b, 0, "bass-2-2").duration = 1440,
            Code::NoteCrossesLoopStart,
            "/revisions/0/composition/notes/10",
        ),
        (
            "tempo differs when the loop returns",
            village,
            |b| {
                b.revisions[0].composition.tempo_map.remove(1);
            },
            Code::LoopStateMismatch,
            "/revisions/0/composition/tempo_map",
        ),
        (
            "controller differs when the loop returns",
            village,
            |b| {
                b.revisions[0].composition.control_lanes.push(ControlLane {
                    voice: "melody".into(),
                    kind: ControlKind::Expression,
                    points: vec![
                        ControlPoint { at: 0, value: 100 },
                        ControlPoint {
                            at: 9600,
                            value: 80,
                        },
                    ],
                })
            },
            Code::LoopStateMismatch,
            "/revisions/0/composition/control_lanes/3",
        ),
        (
            "note in an unknown voice",
            village,
            |b| note_mut(b, 0, "bass-1-1").voice = "drums".into(),
            Code::UnknownReference,
            "/revisions/0/composition/notes/1/voice",
        ),
        (
            "pitch bend without a bend range",
            chime,
            |b| {
                b.revisions[0]
                    .composition
                    .control_lanes
                    .retain(|l| l.kind != ControlKind::PitchBendRange);
            },
            Code::PitchBendRangeUnspecified,
            "/revisions/0/composition/control_lanes/1",
        ),
        (
            "tempo map in an effect",
            chime,
            |b| {
                b.revisions[0].composition.tempo_map =
                    village().revisions[0].composition.tempo_map.clone()
            },
            Code::MusicalTimingInEffect,
            "/revisions/0/composition/tempo_map",
        ),
        (
            "stored revision differs from its request",
            village,
            |b| note_mut(b, 1, "mel-4-1").pitch = 67,
            Code::RevisionContentMismatch,
            "/revisions/1/composition",
        ),
        (
            "protected melody changes",
            village,
            |b| {
                b.revisions[1].edit_request.as_mut().unwrap().scope.voices = None;
                b.revisions[1].edit_request.as_mut().unwrap().scope.range = None;
                also_update_in_r2(
                    b,
                    NoteUpdate {
                        id: "mel-4-1".into(),
                        pitch: Some(67),
                        ..update()
                    },
                );
            },
            Code::ProtectedChange,
            "/revisions/1/edit_request/protections/0",
        ),
        (
            "change outside the requested range",
            village,
            |b| {
                also_update_in_r2(
                    b,
                    NoteUpdate {
                        id: "acc-5-1".into(),
                        pitch: Some(57),
                        ..update()
                    },
                );
            },
            Code::OutOfScope,
            "/revisions/1/edit_request/scope",
        ),
        (
            "request based on another revision",
            village,
            |b| b.revisions[1].edit_request.as_mut().unwrap().base_revision = "r0".into(),
            Code::RevisionLineage,
            "/revisions/1/edit_request/base_revision",
        ),
        (
            "request names a missing note",
            village,
            |b| {
                let request = b.revisions[1].edit_request.as_mut().unwrap();
                let EditOperation::UpdateNotes { notes } = &mut request.operations[0] else {
                    panic!()
                };
                notes[3].id = "acc-9-4".into();
            },
            Code::UnknownReference,
            "/revisions/1/edit_request/operations/0/notes/3/id",
        ),
        (
            "job id reused for another revision",
            village,
            |b| {
                let mut repeat = b.revisions[1].clone();
                repeat.id = "r3".into();
                repeat.parent = Some("r2".into());
                repeat.edit_request.as_mut().unwrap().base_revision = "r2".into();
                b.revisions.push(repeat);
            },
            Code::DuplicateId,
            "/revisions/2/edit_request/job_id",
        ),
        (
            "render shorter than its content",
            village,
            |b| b.renders[0].length_samples = 1_093_679,
            Code::RenderTooShort,
            "/renders/0/length_samples",
        ),
        (
            "repeating an effect without a loop",
            chime,
            |b| b.renders[0].layout = RenderLayout::Full { loop_passes: 2 },
            Code::InvalidRenderLayout,
            "/renders/0/layout/loop_passes",
        ),
        (
            "mark after the end of the render",
            village,
            |b| b.feedback[0].marks[0].start_sample = 1_137_780,
            Code::MarkOutsideRender,
            "/feedback/0/marks/0/start_sample",
        ),
        (
            "mark on a render that was not presented",
            village,
            |b| {
                b.feedback[0].marks.push(Mark {
                    render: "r2-loop2".into(),
                    start_sample: 0,
                    end_sample: None,
                    voice: None,
                    label: None,
                })
            },
            Code::UnknownReference,
            "/feedback/0/marks/1/render",
        ),
        (
            "delivered feedback without content",
            village,
            |b| {
                b.feedback[1].state = FeedbackState::Delivered { sequence: 2 };
            },
            Code::EmptyDeliveredFeedback,
            "/feedback/1",
        ),
        (
            "controlled voices share a channel",
            village,
            |b| b.midi_exports[0].assignments[1].channel = 0,
            Code::SharedChannelControl,
            "/midi_exports/0/assignments",
        ),
        (
            "voice without a channel",
            village,
            |b| {
                b.midi_exports[0].assignments.pop();
            },
            Code::UnassignedVoice,
            "/midi_exports/0/assignments",
        ),
        (
            "music export with separate timing",
            village,
            |b| {
                b.midi_exports[0].timing = Some(MidiTiming {
                    ppq: 480,
                    microseconds_per_quarter: 500_000,
                })
            },
            Code::InvalidExportTiming,
            "/midi_exports/0/timing",
        ),
        (
            "effect time off the file grid",
            chime,
            |b| {
                b.midi_exports[0].timing = Some(MidiTiming {
                    ppq: 60,
                    microseconds_per_quarter: 1_000_000,
                })
            },
            Code::EffectTimeNotRepresentable,
            "/midi_exports/0/timing",
        ),
    ];

    for (name, fixture, mutate, code, location) in cases {
        let mut bundle = fixture();
        mutate(&mut bundle);
        let diagnostics = check_project_bundle(&bundle);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == code && d.location == location),
            "{name}: expected {code:?} at {location}, got {diagnostics:#?}",
        );
    }
}

fn update() -> NoteUpdate {
    NoteUpdate {
        id: String::new(),
        voice: None,
        start: None,
        duration: None,
        pitch: None,
        velocity: None,
    }
}
