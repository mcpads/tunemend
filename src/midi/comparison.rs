//! Event-level comparison of two Standard MIDI Files.
//!
//! Each track is aligned as a sequence (Myers' O(ND) difference algorithm,
//! *An O(ND) Difference Algorithm and Its Variations*, 1986) on absolute
//! tick, decoded event and complete metadata payload. An inserted or removed
//! event is therefore one difference, not a shift of every later index.

use super::*;
use serde_json::{Value, json};

/// Differences returned per page unless a limit is given.
pub const DEFAULT_DIFFERENCE_PAGE: usize = 50;
pub const MAX_DIFFERENCE_PAGE: usize = 500;
/// Beyond this edit distance a track's differing middle is compared by
/// position instead; the report names the tracks where that happened.
const MAX_ALIGNMENT_DISTANCE: usize = 2048;

pub fn compare_midi(reference: &[u8], candidate: &[u8]) -> Value {
    compare_midi_page(reference, candidate, 0, DEFAULT_DIFFERENCE_PAGE)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Same(usize, usize),
    Removed(usize),
    Inserted(usize),
}

/// The shortest edit script turning `a` into `b` under `same`, or `None`
/// when it needs more than `limit` insertions and removals.
fn align(
    n: usize,
    m: usize,
    same: impl Fn(usize, usize) -> bool,
    limit: usize,
) -> Option<Vec<Step>> {
    let mut steps = Vec::new();
    let (mut start, mut end_a, mut end_b) = (0, n, m);
    while start < n.min(m) && same(start, start) {
        steps.push(Step::Same(start, start));
        start += 1;
    }
    let mut suffix = Vec::new();
    while end_a > start && end_b > start && same(end_a - 1, end_b - 1) {
        end_a -= 1;
        end_b -= 1;
        suffix.push(Step::Same(end_a, end_b));
    }
    let (n_mid, m_mid) = (end_a - start, end_b - start);
    let eq = |x: usize, y: usize| same(start + x, start + y);
    // trace[d][(k + d) / 2] is the furthest x on diagonal k after d edits.
    let mut trace: Vec<Vec<usize>> = Vec::new();
    let furthest = |trace: &Vec<Vec<usize>>, d: usize, k: isize| -> usize {
        trace[d][((k + d as isize) / 2) as usize]
    };
    let mut found = None;
    'search: for d in 0..=limit.min(n_mid + m_mid) {
        let mut current = vec![0usize; d + 1];
        for (slot, x_slot) in current.iter_mut().enumerate() {
            let k = 2 * slot as isize - d as isize;
            let mut x = if d == 0 {
                0
            } else if k == -(d as isize)
                || (k != d as isize
                    && furthest(&trace, d - 1, k - 1) < furthest(&trace, d - 1, k + 1))
            {
                furthest(&trace, d - 1, k + 1)
            } else {
                furthest(&trace, d - 1, k - 1) + 1
            };
            let mut y = (x as isize - k) as usize;
            while x < n_mid && y < m_mid && eq(x, y) {
                x += 1;
                y += 1;
            }
            *x_slot = x;
            if x >= n_mid && y >= m_mid {
                trace.push(current);
                found = Some(d);
                break 'search;
            }
        }
        trace.push(current);
    }
    let distance = found?;
    let mut middle = Vec::new();
    let (mut x, mut y) = (n_mid, m_mid);
    for d in (1..=distance).rev() {
        let k = x as isize - y as isize;
        let down = k == -(d as isize)
            || (k != d as isize && furthest(&trace, d - 1, k - 1) < furthest(&trace, d - 1, k + 1));
        let previous_k = if down { k + 1 } else { k - 1 };
        let previous_x = furthest(&trace, d - 1, previous_k);
        let previous_y = (previous_x as isize - previous_k) as usize;
        let (snake_x, snake_y) = if down {
            (previous_x, previous_y + 1)
        } else {
            (previous_x + 1, previous_y)
        };
        while x > snake_x && y > snake_y {
            x -= 1;
            y -= 1;
            middle.push(Step::Same(start + x, start + y));
        }
        middle.push(if down {
            Step::Inserted(start + previous_y)
        } else {
            Step::Removed(start + previous_x)
        });
        (x, y) = (previous_x, previous_y);
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        middle.push(Step::Same(start + x, start + y));
    }
    steps.extend(middle.into_iter().rev());
    steps.extend(suffix.into_iter().rev());
    Some(steps)
}

/// Channel messages name their MIDI channel 1–16, as voice IDs do.
fn describe_event(kind: TrackEventKind) -> String {
    match kind {
        TrackEventKind::Midi { channel, message } => {
            format!("MIDI channel {}: {message:?}", channel.as_int() + 1)
        }
        other => format!("{other:?}"),
    }
}

struct Event<'a> {
    tick: u64,
    kind: TrackEventKind<'a>,
    payload: Option<(u8, &'a [u8])>,
}

fn events<'a>(track: &[(&'a [u8], TrackEvent<'a>)]) -> Vec<Event<'a>> {
    let mut tick = 0;
    track
        .iter()
        .map(|(raw, event)| {
            tick += u64::from(event.delta.as_int());
            Event {
                tick,
                kind: event.kind,
                payload: super::source::meta_payload(raw),
            }
        })
        .collect()
}

/// Differences of one track: paired removals and insertions at one tick are
/// `changed`, of one identical event at different ticks `moved`.
fn track_differences(
    track: usize,
    a: &[Event],
    b: &[Event],
    positional: &mut Vec<usize>,
) -> Vec<Value> {
    let same = |i: usize, j: usize| {
        a[i].tick == b[j].tick && a[i].kind == b[j].kind && a[i].payload == b[j].payload
    };
    let steps = align(a.len(), b.len(), same, MAX_ALIGNMENT_DISTANCE).unwrap_or_else(|| {
        positional.push(track);
        (0..a.len().max(b.len()))
            .flat_map(|i| match (i < a.len(), i < b.len()) {
                (true, true) if same(i, i) => vec![Step::Same(i, i)],
                (true, true) => vec![Step::Removed(i), Step::Inserted(i)],
                (true, false) => vec![Step::Removed(i)],
                _ => vec![Step::Inserted(i)],
            })
            .collect()
    });
    let location = |i: usize| format!("/tracks/{track}/events/{i}");
    let describe = |e: &Event| describe_event(e.kind);
    let mut out = Vec::new();
    let mut run: (Vec<usize>, Vec<usize>) = (vec![], vec![]);
    // Reference position before which the current run's insertions go.
    let mut at = 0;
    let flush = |run: &mut (Vec<usize>, Vec<usize>), at: usize, out: &mut Vec<Value>| {
        let (removed, inserted) = std::mem::take(run);
        let mut used = vec![false; inserted.len()];
        let mut entries = Vec::new();
        for &i in &removed {
            let pair = (0..inserted.len())
                .find(|&s| !used[s] && b[inserted[s]].tick == a[i].tick)
                .map(|s| (s, "changed"))
                .or_else(|| {
                    (0..inserted.len())
                        .find(|&s| {
                            !used[s]
                                && b[inserted[s]].kind == a[i].kind
                                && b[inserted[s]].payload == a[i].payload
                        })
                        .map(|s| (s, "moved"))
                });
            match pair {
                Some((s, change)) => {
                    used[s] = true;
                    let j = inserted[s];
                    entries.push((a[i].tick, json!({"location": location(i), "candidate_location": location(j), "change": change,
                        "reference_tick": a[i].tick, "candidate_tick": b[j].tick, "reference": describe(&a[i]), "candidate": describe(&b[j])})));
                }
                None => entries.push((
                    a[i].tick,
                    json!({"location": location(i), "change": "removed",
                    "reference_tick": a[i].tick, "reference": describe(&a[i])}),
                )),
            }
        }
        for (_, &j) in inserted.iter().enumerate().filter(|(s, _)| !used[*s]) {
            entries.push((b[j].tick, json!({"location": location(at), "candidate_location": location(j), "change": "inserted",
                "candidate_tick": b[j].tick, "candidate": describe(&b[j])})));
        }
        entries.sort_by_key(|(tick, _)| *tick);
        out.extend(entries.into_iter().map(|(_, entry)| entry));
    };
    for step in steps {
        match step {
            Step::Same(i, _) => {
                flush(&mut run, at, &mut out);
                at = i + 1;
            }
            Step::Removed(i) => {
                run.0.push(i);
                at = i + 1;
            }
            Step::Inserted(j) => run.1.push(j),
        }
    }
    flush(&mut run, at, &mut out);
    out
}

/// One page of differences from `offset`; `next_cursor` continues it.
pub fn compare_midi_page(reference: &[u8], candidate: &[u8], offset: usize, limit: usize) -> Value {
    let parsed = (
        midly::SmfBytemap::parse(reference),
        midly::SmfBytemap::parse(candidate),
    );
    let (Ok(reference_smf), Ok(candidate_smf)) = parsed else {
        return json!({"comparable": false, "byte_identical": reference == candidate,
            "events_identical": false, "reason": "both inputs must be well-formed Standard MIDI Files"});
    };
    let mut differences = Vec::new();
    if reference_smf.header != candidate_smf.header {
        differences.push(json!({"location": "/header", "reference": format!("{:?}", reference_smf.header), "candidate": format!("{:?}", candidate_smf.header)}));
    }
    if reference_smf.tracks.len() != candidate_smf.tracks.len() {
        differences.push(json!({"location": "/tracks", "reference": reference_smf.tracks.len(), "candidate": candidate_smf.tracks.len()}));
    }
    let mut event_differences = 0;
    let mut positional = Vec::new();
    for track in 0..reference_smf.tracks.len().max(candidate_smf.tracks.len()) {
        let a = reference_smf
            .tracks
            .get(track)
            .map(|t| events(t))
            .unwrap_or_default();
        let b = candidate_smf
            .tracks
            .get(track)
            .map(|t| events(t))
            .unwrap_or_default();
        if a.len() != b.len() && track < reference_smf.tracks.len().min(candidate_smf.tracks.len())
        {
            differences.push(json!({"location": format!("/tracks/{track}"), "reference_events": a.len(), "candidate_events": b.len()}));
        }
        let found = track_differences(track, &a, &b, &mut positional);
        event_differences += found.len();
        differences.extend(found);
    }
    let total = differences.len();
    let limit = limit.clamp(1, MAX_DIFFERENCE_PAGE);
    let page: Vec<Value> = differences.into_iter().skip(offset).take(limit).collect();
    let mut result = json!({"comparable": true, "byte_identical": reference == candidate,
        "events_identical": reference_smf.header == candidate_smf.header && reference_smf.tracks.len() == candidate_smf.tracks.len() && event_differences == 0,
        "container_identical": match (super::source::container_data(reference), super::source::container_data(candidate)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }, "aligned_event_differences": event_differences,
        "differences": page, "reference_sha256": crate::store::sha256_hex(reference),
        "candidate_sha256": crate::store::sha256_hex(candidate),
        "scope": "MIDI headers, tracks, every event class, exact ticks and event order. This does not compare audio or prove target playback."});
    if total > 0 {
        result["total_differences"] = json!(total);
        result["offset"] = json!(offset);
        if offset + limit < total {
            result["next_cursor"] = json!(offset + limit);
            result["paging"] = json!(
                "more differences follow: call compare_midi with the same reference and candidate files and next_cursor as offset"
            );
        }
        result["alignment"] = json!(if positional.is_empty() {
            "each track aligned as an event sequence on tick, event and payload: an inserted or removed event is one difference; removal and insertion at one tick pair as changed, the same event at another tick as moved; locations are reference event indices (insertions: the reference index they precede) and candidate_location the candidate index".to_owned()
        } else {
            format!(
                "as for aligned tracks, except tracks {positional:?}, whose differences exceeded {MAX_ALIGNMENT_DISTANCE} edits and are compared by position"
            )
        });
    }
    result
}
