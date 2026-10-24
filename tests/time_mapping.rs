//! Exact musical and render time mappings, computed from
//! the creative fixtures.

use tunemend::composition::{Composition, TempoChange, Timebase};
use tunemend::edit::{EditOperation, EditRequest, NoteUpdate, apply_edit};
use tunemend::export::MidiTiming;
use tunemend::review::RenderLayout;
use tunemend::timeline::{Rational, RenderPosition, locate_in_render, seconds_at};
use tunemend::{DiagnosticCode as Code, ProjectBundle};

const MUSIC: Timebase = Timebase::Music { ppq: 480 };

fn village_root() -> Composition {
    let path = format!("{}/fixtures/village-loop.json", env!("CARGO_MANIFEST_DIR"));
    let bundle: ProjectBundle =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    bundle.revisions[0].composition.clone()
}

fn r(numerator: i128, denominator: i128) -> Rational {
    Rational::new(numerator, denominator)
}

#[test]
fn loop_render_positions_map_back_to_the_source() {
    let bgm = village_root();
    let two_passes = RenderLayout::Full { loop_passes: 2 };
    assert_eq!(seconds_at(MUSIC, &bgm, r(3840, 1)), r(4, 1));
    assert_eq!(seconds_at(MUSIC, &bgm, r(15360, 1)), r(72, 5));

    let second_pass_start = locate_in_render(MUSIC, &bgm, &two_passes, r(72, 5));
    assert_eq!(
        second_pass_start,
        RenderPosition::Content {
            source_position: r(3840, 1),
            source_seconds: r(4, 1),
            loop_pass: Some(2)
        }
    );

    let marked = locate_in_render(MUSIC, &bgm, &two_passes, r(723_240, 44_100));
    assert_eq!(
        marked,
        RenderPosition::Content {
            source_position: r(5760, 1),
            source_seconds: r(6, 1),
            loop_pass: Some(2)
        }
    );

    let after_content = locate_in_render(MUSIC, &bgm, &two_passes, r(25, 1));
    assert_eq!(
        after_content,
        RenderPosition::Tail {
            seconds_after_content: r(1, 5)
        }
    );

    let excerpt = RenderLayout::Excerpt {
        start: 5760,
        end: 7680,
    };
    let in_excerpt = locate_in_render(MUSIC, &bgm, &excerpt, r(1, 1));
    assert_eq!(
        in_excerpt,
        RenderPosition::Content {
            source_position: r(6720, 1),
            source_seconds: r(7, 1),
            loop_pass: None
        }
    );
}

#[test]
fn a_tempo_change_moves_the_same_tick_to_another_second() {
    let mut slower = village_root();
    for change in slower.tempo_map.iter_mut().take(2) {
        *change = TempoChange {
            microseconds_per_quarter: 600_000,
            ..*change
        };
    }
    assert_eq!(seconds_at(MUSIC, &slower, r(5760, 1)), r(36, 5));
}

#[test]
fn effect_times_convert_to_exact_file_ticks() {
    let sixty_per_second = MidiTiming {
        ppq: 60,
        microseconds_per_quarter: 1_000_000,
    };
    assert_eq!(sixty_per_second.ticks_for_microseconds(25_000), r(3, 2));
    let millisecond_grid = MidiTiming {
        ppq: 1000,
        microseconds_per_quarter: 1_000_000,
    };
    assert_eq!(millisecond_grid.ticks_for_microseconds(25_000), r(25, 1));
}

#[test]
fn an_edit_that_breaks_the_composition_returns_no_result() {
    let bgm = village_root();
    let request = EditRequest {
        job_id: "double-bass".into(),
        base_revision: "r1".into(),
        scope: Default::default(),
        protections: vec![],
        feedback: vec![],
        rationale: None,
        operations: vec![EditOperation::UpdateNotes {
            notes: vec![NoteUpdate {
                id: "bass-1-1".into(),
                voice: None,
                start: None,
                duration: Some(4800),
                pitch: None,
                velocity: None,
            }],
        }],
    };
    let diagnostics = apply_edit(MUSIC, &bgm, &request).unwrap_err();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == Code::NoteCrossesLoopStart && d.location == "/result/notes/1"),
        "{diagnostics:#?}"
    );
}
