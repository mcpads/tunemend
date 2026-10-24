//! Readable positions: seconds, exact ratios and bar/beat labels.

use super::*;

pub(super) fn rational_seconds(value: Rational) -> f64 {
    *value.numer() as f64 / *value.denom() as f64
}

pub(super) fn rational_text(value: Rational) -> String {
    if value.is_integer() {
        value.to_integer().to_string()
    } else {
        format!("{}/{}", value.numer(), value.denom())
    }
}

/// Bar and beat of a tick, counting from bar 1 beat 1.
pub fn bar_beat(composition: &Composition, ppq: u16, tick: Rational) -> String {
    let Some((bar, beat, offset, _)) = bar_beat_parts(composition, ppq, tick) else {
        return String::new();
    };
    let mut label = format!("bar {bar} beat {beat}");
    if offset.is_integer() && offset != Rational::from_integer(0) {
        label.push_str(&format!(" +{} ticks", offset.to_integer()));
    } else if !offset.is_integer() {
        // The exact value stays in `source_position`; the label is for reading.
        label.push_str(&format!(" +{:.1} ticks", rational_seconds(offset)));
    }
    label
}

/// Bar, beat (both from 1), ticks after that beat and the beat length.
fn bar_beat_parts(
    composition: &Composition,
    ppq: u16,
    tick: Rational,
) -> Option<(i128, i128, Rational, Rational)> {
    let signatures = if composition.time_signatures.is_empty() {
        vec![TimeSignature {
            tick: 0,
            numerator: 4,
            denominator: 4,
        }]
    } else {
        composition.time_signatures.clone()
    };
    let mut bar = 1i128;
    for (i, signature) in signatures.iter().enumerate() {
        let beat = Rational::new(i128::from(ppq) * 4, i128::from(signature.denominator));
        let bar_length = beat * i128::from(signature.numerator);
        let start = Rational::from_integer(signature.tick.into());
        let next = signatures
            .get(i + 1)
            .map(|s| Rational::from_integer(s.tick.into()));
        if next.is_some_and(|n| tick >= n) {
            bar += ((next.expect("checked") - start) / bar_length)
                .ceil()
                .to_integer();
            continue;
        }
        let into = tick - start;
        let bars = (into / bar_length).floor();
        let remainder = into - bars * bar_length;
        let beats = (remainder / beat).floor();
        let offset = remainder - beats * beat;
        return Some((
            bar + bars.to_integer(),
            beats.to_integer() + 1,
            offset,
            beat,
        ));
    }
    None
}

/// The following beat and the ticks before it, such as "bar 3 beat 1 -14
/// ticks", when that beat is nearer than the beat `bar_beat` counts from;
/// `None` on a beat or in its first half.
pub(super) fn nearest_beat(composition: &Composition, ppq: u16, tick: Rational) -> Option<String> {
    let (_, _, offset, beat) = bar_beat_parts(composition, ppq, tick)?;
    if offset * 2 <= beat {
        return None;
    }
    let before = beat - offset;
    let (bar, number, at, _) = bar_beat_parts(composition, ppq, tick + before)?;
    if at != Rational::from_integer(0) {
        return None;
    }
    Some(if before.is_integer() {
        format!("bar {bar} beat {number} -{} ticks", before.to_integer())
    } else {
        format!(
            "bar {bar} beat {number} -{:.1} ticks",
            rational_seconds(before)
        )
    })
}

pub(super) fn position_label(
    timebase: Timebase,
    composition: &Composition,
    position: Rational,
) -> String {
    match timebase {
        Timebase::Music { ppq } => bar_beat(composition, ppq, position),
        Timebase::Effect => format!("{:.1} ms", rational_seconds(position) / 1000.0),
    }
}

/// [`nearest_beat`] for a project position; `None` in the effect timebase.
pub(super) fn nearest_label(
    timebase: Timebase,
    composition: &Composition,
    position: Rational,
) -> Option<String> {
    match timebase {
        Timebase::Music { ppq } => nearest_beat(composition, ppq, position),
        Timebase::Effect => None,
    }
}
