//! Exact conversions between timebase positions, seconds and rendered audio.
//!
//! The functions expect a composition that passed the composition checks:
//! a positive PPQ and a tempo map with positive tempos starting at tick 0.

use num_rational::Ratio;

use crate::composition::{Composition, TempoChange, Timebase};
use crate::review::RenderLayout;

/// Exact rational value used for every time conversion.
pub type Rational = Ratio<i128>;

const MICROSECONDS_PER_SECOND: i128 = 1_000_000;

fn whole(value: u64) -> Rational {
    Rational::from_integer(i128::from(value))
}

/// Seconds from the start of the composition to `position`, which may be a
/// fractional tick or microsecond.
pub fn seconds_at(timebase: Timebase, composition: &Composition, position: Rational) -> Rational {
    match timebase {
        Timebase::Effect => position / MICROSECONDS_PER_SECOND,
        Timebase::Music { ppq } => {
            let mut seconds = Rational::from_integer(0);
            for (i, change) in composition.tempo_map.iter().enumerate() {
                let segment_start = whole(change.tick);
                if position <= segment_start {
                    break;
                }
                let segment_end = match composition.tempo_map.get(i + 1) {
                    Some(next) => position.min(whole(next.tick)),
                    None => position,
                };
                seconds += (segment_end - segment_start) * seconds_per_tick(change, ppq);
            }
            seconds
        }
    }
}

/// Inverse of [`seconds_at`]: the exact timebase position heard at `seconds`.
pub fn position_at_seconds(
    timebase: Timebase,
    composition: &Composition,
    seconds: Rational,
) -> Rational {
    match timebase {
        Timebase::Effect => seconds * MICROSECONDS_PER_SECOND,
        Timebase::Music { ppq } => {
            let mut elapsed = Rational::from_integer(0);
            for (i, change) in composition.tempo_map.iter().enumerate() {
                let per_tick = seconds_per_tick(change, ppq);
                let remaining_ticks = (seconds - elapsed) / per_tick;
                let Some(next) = composition.tempo_map.get(i + 1) else {
                    return whole(change.tick) + remaining_ticks;
                };
                let segment_ticks = whole(next.tick) - whole(change.tick);
                if remaining_ticks < segment_ticks {
                    return whole(change.tick) + remaining_ticks;
                }
                elapsed += segment_ticks * per_tick;
            }
            Rational::from_integer(0)
        }
    }
}

fn seconds_per_tick(change: &TempoChange, ppq: u16) -> Rational {
    Rational::new(
        i128::from(change.microseconds_per_quarter),
        i128::from(ppq) * MICROSECONDS_PER_SECOND,
    )
}

/// Where a moment of a rendered file comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderPosition {
    Content {
        /// Exact position in the project's timebase unit.
        source_position: Rational,
        source_seconds: Rational,
        /// 1-based repetition of the loop region; `None` outside the loop.
        loop_pass: Option<u32>,
    },
    /// After the last content position, while the rendered sound decays.
    Tail { seconds_after_content: Rational },
}

/// Seconds of musical content a render layout plays, excluding the tail.
pub fn content_seconds(
    timebase: Timebase,
    composition: &Composition,
    layout: &RenderLayout,
) -> Rational {
    match *layout {
        RenderLayout::Full { loop_passes } => {
            let total = seconds_at(timebase, composition, whole(composition.playback.end));
            match composition.playback.loop_start {
                Some(loop_start) => {
                    let intro = seconds_at(timebase, composition, whole(loop_start));
                    intro + (total - intro) * i128::from(loop_passes)
                }
                None => total,
            }
        }
        RenderLayout::Excerpt { start, end } => {
            seconds_at(timebase, composition, whole(end))
                - seconds_at(timebase, composition, whole(start))
        }
    }
}

/// Maps a time in a rendered file back to the composition it was rendered from.
pub fn locate_in_render(
    timebase: Timebase,
    composition: &Composition,
    layout: &RenderLayout,
    render_seconds: Rational,
) -> RenderPosition {
    let content = content_seconds(timebase, composition, layout);
    if render_seconds >= content {
        return RenderPosition::Tail {
            seconds_after_content: render_seconds - content,
        };
    }
    let (source_seconds, loop_pass) = match (*layout, composition.playback.loop_start) {
        (RenderLayout::Excerpt { start, .. }, _) => (
            seconds_at(timebase, composition, whole(start)) + render_seconds,
            None,
        ),
        (RenderLayout::Full { .. }, None) => (render_seconds, None),
        (RenderLayout::Full { .. }, Some(loop_start)) => {
            let intro = seconds_at(timebase, composition, whole(loop_start));
            if render_seconds < intro {
                (render_seconds, None)
            } else {
                let loop_length =
                    seconds_at(timebase, composition, whole(composition.playback.end)) - intro;
                let into_loops = render_seconds - intro;
                let completed = (into_loops / loop_length).floor();
                let pass = u32::try_from(completed.to_integer() + 1)
                    .expect("pass is bounded by loop_passes");
                (intro + into_loops - completed * loop_length, Some(pass))
            }
        }
    };
    RenderPosition::Content {
        source_position: position_at_seconds(timebase, composition, source_seconds),
        source_seconds,
        loop_pass,
    }
}
