//! How a looping sample repeats: the loop's length in periods of its
//! fundamental, what happens to the waveform and level at the seam, and
//! the pitch of the looped sustain. Also the loop starts that keep a loop
//! aligned and a whole number of periods long, for refusals and guides.

use serde_json::{Value, json};

use crate::measure::pitch::{self, PitchSettings, midi_note, note_name};

/// A period-count remainder above this, in periods, is reported as a phase jump.
pub const PHASE_TOLERANCE: f64 = 0.05;
/// Loop starts suggested as whole-period loops are within this many periods.
pub const WHOLE_PERIOD_TOLERANCE: f64 = 0.02;
/// Sustain pitch further than this from the declared root is reported.
pub const ROOT_TOLERANCE_CENTS: f64 = 10.0;
/// Level changes across the seam larger than this are reported.
pub const LEVEL_TOLERANCE_DB: f64 = 1.0;

/// The fundamental of a steady loop traversal and its pitch track.
pub struct Sustain {
    /// Median of the voiced windows' unrounded frequencies.
    pub hz: f64,
    pub track: pitch::PitchTrack,
}

/// Pitch of `body` (one loop traversal) repeated to at least 0.3 s, as the
/// looped sustain sounds. `None` when fewer than half the windows are voiced
/// or the rate is too low for the default search range.
pub fn sustain(body: &[i32], rate: u32) -> Option<Sustain> {
    if body.is_empty() {
        return None;
    }
    let settings = PitchSettings {
        windows: Some(true),
        ..Default::default()
    };
    let config = settings
        .resolve(rate, crate::measure::DEFAULT_SILENCE_DBFS)
        .ok()?;
    let wanted = (config.window_samples * 4).max(rate as usize * 3 / 10);
    let repeats = wanted.div_ceil(body.len()).max(2);
    let signal: Vec<f32> = (0..repeats)
        .flat_map(|_| body.iter().map(|&v| v as f32 / 32768.0))
        .collect();
    let track = pitch::track(&[signal], rate, &config, 0.0);
    let mut hz: Vec<f64> = track
        .windows
        .iter()
        .flatten()
        .filter_map(|w| w.exact_hz)
        .collect();
    if hz.is_empty() || hz.len() * 2 < track.summary.windows {
        return None;
    }
    hz.sort_by(f64::total_cmp);
    let middle = hz.len() / 2;
    let median = if hz.len() % 2 == 1 {
        hz[middle]
    } else {
        (hz[middle - 1] + hz[middle]) / 2.0
    };
    Some(Sustain { hz: median, track })
}

/// Rounded, with a negative zero read as zero.
fn round(value: f64, digits: i32) -> f64 {
    crate::measure::round(value, digits) + 0.0
}

fn rms(values: &[i32]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    (values.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

/// `x` at a fractional index, linearly interpolated.
fn at(x: &[i32], position: f64) -> f64 {
    let low = position.floor() as usize;
    let fraction = position - low as f64;
    let a = x[low] as f64;
    let b = x.get(low + 1).copied().unwrap_or(x[low]) as f64;
    a + (b - a) * fraction
}

/// RMS difference between the `width` samples from `from` and the samples
/// one period earlier, relative to `scale`: 0 when the waveform repeats.
fn period_error(x: &[i32], from: usize, width: usize, period: f64, scale: f64) -> Option<f64> {
    if scale <= 0.0 || (from as f64) < period + 1.0 || from + width > x.len() {
        return None;
    }
    let squares: f64 = (0..width)
        .map(|j| (x[from + j] as f64 - at(x, (from + j) as f64 - period)).powi(2))
        .sum();
    Some(round((squares / width as f64).sqrt() / scale, 4))
}

/// Diagnostics of a decoded looping stream: `stream` holds the first pass
/// (`first_pass` samples, looping back to `loop_start`) followed by one loop
/// traversal, as the preview WAVs of a preserved loop do. `root` is the
/// declared root as a fractional MIDI note.
pub fn diagnostics(
    stream: &[i32],
    first_pass: usize,
    loop_start: usize,
    rate: u32,
    root: Option<f64>,
) -> Value {
    let loop_samples = first_pass.saturating_sub(loop_start);
    if loop_samples == 0 || stream.len() < first_pass + loop_samples || first_pass == 0 {
        return json!({"unavailable": "the stream holds no complete loop traversal"});
    }
    let body = &stream[first_pass..first_pass + loop_samples];
    let scale = rms(&stream[loop_start..first_pass]);
    let mut flags: Vec<String> = Vec::new();
    let mut out = json!({
        "sample_rate": rate,
        "loop_start_sample": loop_start,
        "loop_samples": loop_samples,
        "loop_seconds": round(loop_samples as f64 / f64::from(rate), 6),
    });
    let found = sustain(body, rate);
    let period = found.as_ref().map(|s| f64::from(rate) / s.hz);
    match &found {
        Some(s) => {
            let periods = loop_samples as f64 * s.hz / f64::from(rate);
            let remainder = periods - periods.round();
            out["fundamental_hz"] = json!(round(s.hz, 3));
            out["period_samples"] = json!(round(f64::from(rate) / s.hz, 3));
            out["periods_in_loop"] = json!(round(periods, 3));
            out["phase_jump_periods"] = json!(round(remainder, 3));
            if remainder.abs() > PHASE_TOLERANCE {
                flags.push(format!(
                    "the loop spans {periods:.3} periods of {:.2} Hz, not a whole number: the waveform's phase jumps by {remainder:+.3} period at the seam on every repeat",
                    s.hz
                ));
            }
        }
        None => {
            out["fundamental_hz"] = Value::Null;
            flags.push("no stable fundamental in the looped sustain (fewer than half its pitch windows are voiced); period measures are omitted".into());
        }
    }
    let largest_inside = stream[loop_start..first_pass]
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .max()
        .unwrap_or(0);
    let seam_step = (stream[first_pass] - stream[first_pass - 1]).abs();
    let mut seam = json!({
        "step": seam_step,
        "largest_step_inside_loop": largest_inside,
    });
    if seam_step > 256 && f64::from(seam_step) > 1.5 * f64::from(largest_inside) {
        flags.push(format!(
            "the sample step at the seam ({seam_step}) is larger than every step inside the loop (at most {largest_inside}): a click is likely"
        ));
    }
    if let Some(period) = period {
        let width = (period.round() as usize).max(8).min(loop_samples);
        let at_seam = period_error(stream, first_pass, width, period, scale);
        let inside = period_error(stream, loop_start + loop_samples / 2, width, period, scale)
            .filter(|_| loop_start + loop_samples / 2 + width <= first_pass);
        seam["waveform_error"] = json!(at_seam);
        seam["waveform_error_inside_loop"] = json!(inside);
        if let (Some(at_seam), Some(inside)) = (at_seam, inside)
            && at_seam > (2.0 * inside).max(inside + 0.1)
        {
            flags.push(format!(
                "the waveform breaks at the seam: one period after it differs from one period before by {at_seam:.3} of the loop RMS, against {inside:.3} inside the loop"
            ));
        }
    }
    let level_width = match period {
        Some(p) => ((f64::from(rate) / 100.0 / p).ceil() * p).round() as usize,
        None => rate as usize / 100,
    }
    .clamp(1, (loop_samples / 2).max(1));
    let end_level = rms(&stream[first_pass - level_width..first_pass]);
    let start_level = rms(&stream[loop_start..loop_start + level_width]);
    let change = (end_level > 0.0 && start_level > 0.0)
        .then(|| round(20.0 * (start_level / end_level).log10(), 2));
    seam["level_change_db"] = json!(change);
    seam["level_window_samples"] = json!(level_width);
    if let Some(change) = change
        && change.abs() > LEVEL_TOLERANCE_DB
    {
        flags.push(format!(
            "the level changes by {change:+.2} dB across the seam (loop start against loop end)"
        ));
    }
    out["seam"] = seam;
    if let Some(s) = &found {
        let summary = &s.track.summary;
        let exact = midi_note(s.hz, pitch::DEFAULT_A4_HZ);
        let note = exact.round() as i32;
        let cents = |hz: Option<f64>| hz.map(|h| round(1200.0 * (h / s.hz).log2(), 1));
        let mut sustain = json!({
            "median_hz": round(s.hz, 3),
            "note": note_name(note),
            "cents_from_note": round((exact - f64::from(note)) * 100.0, 1),
            "lowest_cents_from_median": cents(summary.lowest_hz),
            "highest_cents_from_median": cents(summary.highest_hz),
            "windows": summary.windows,
            "voiced_windows": summary.voiced_windows,
            "aperiodic_windows": summary.aperiodic_windows,
            "median_periodicity": summary.median_periodicity,
        });
        if summary.aperiodic_windows * 5 > summary.windows {
            flags.push(format!(
                "{} of {} pitch windows of the looped sustain are aperiodic",
                summary.aperiodic_windows, summary.windows
            ));
        }
        if let Some(root) = root {
            let from_root = round((exact - root) * 100.0, 1);
            sustain["cents_from_root"] = json!(from_root);
            if from_root.abs() > ROOT_TOLERANCE_CENTS {
                flags.push(format!(
                    "the looped sustain sounds {from_root:+.1} cents from the declared root"
                ));
            }
        }
        out["sustain_pitch"] = sustain;
    }
    out["flags"] = json!(flags);
    out["method"] = json!(format!(
        "measured on the decoded target (first pass, then one traversal with decoder history carried); fundamental by YIN over the traversal repeated to at least 0.3 s; waveform_error is the RMS difference between one period after a point and the period before it, relative to the loop RMS; levels are RMS over level_window_samples at the loop start and end. Flag thresholds: phase jump above {PHASE_TOLERANCE} period, seam waveform error above twice the inside error (and 0.1 more), a seam step 1.5 times the largest inside step, a level change above {LEVEL_TOLERANCE_DB} dB, more than a fifth of the windows aperiodic, more than {ROOT_TOLERANCE_CENTS} cents from the root"
    ));
    out
}

/// Source samples between loop starts (and lengths) that land on whole
/// `block`-sample blocks after resampling from `rate` to `out_rate`.
pub fn source_step(block: u64, rate: u32, out_rate: u32) -> u64 {
    let scaled = block * u64::from(rate);
    scaled / gcd(scaled, u64::from(out_rate))
}

pub fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Up to `limit` loop starts on multiples of `step`, nearest `start` first,
/// whose loop to `length` spans a whole number of `period`-sample periods
/// (within [`WHOLE_PERIOD_TOLERANCE`]); `length` should itself be a multiple
/// of `step`. Each is the start and the periods spanned.
pub fn whole_period_starts(
    length: u64,
    start: u64,
    step: u64,
    period: f64,
    limit: usize,
) -> Vec<(u64, f64)> {
    if step == 0 || period <= 0.0 || length < step {
        return vec![];
    }
    let mut found: Vec<(u64, f64)> = (0..length / step)
        .map(|k| k * step)
        .filter_map(|s| {
            let periods = (length - s) as f64 / period;
            (periods >= 1.0 && (periods - periods.round()).abs() <= WHOLE_PERIOD_TOLERANCE)
                .then_some((s, periods))
        })
        .collect();
    found.sort_by_key(|&(s, _)| (s.abs_diff(start), s));
    found.truncate(limit);
    found
}

/// Up to `limit` target rates other than `out_rate`, within a quarter of it
/// (and 8000 to 64000 Hz), at which a loop to `length` (source samples at `rate`) fits
/// `block`-sample blocks with a source start that also makes it a whole
/// number of `period`-sample periods. Each is the rate, its start nearest
/// `start`, and the periods spanned, ordered by how far the start moves,
/// then by how far the rate moves. `max_samples` bounds the target's first
/// pass plus one traversal.
pub fn whole_period_rates(
    (length, start): (u64, u64),
    block: u64,
    (rate, out_rate): (u32, u32),
    period: f64,
    (limit, max_samples): (usize, u64),
) -> Vec<(u32, u64, f64)> {
    // The length lands on whole blocks at multiples of this rate step.
    let scaled = block * u64::from(rate);
    let rate_step = scaled / gcd(length.max(1), scaled);
    let anchor = u64::from(out_rate);
    let low = (anchor * 3 / 4).max(8000);
    let high = (anchor * 5 / 4).min(64000);
    let mut found: Vec<(u32, u64, f64)> = (low.div_ceil(rate_step)..=high / rate_step)
        .map(|k| k * rate_step)
        .filter(|&t| t != anchor)
        .filter_map(|t| {
            let step = source_step(block, rate, t as u32);
            if !length.is_multiple_of(step) {
                return None;
            }
            let &(s, periods) = whole_period_starts(length, start, step, period, 1).first()?;
            let samples = (length + (length - s)) * t / u64::from(rate);
            (samples <= max_samples).then_some((t as u32, s, periods))
        })
        .collect();
    found.sort_by_key(|&(t, s, _)| (s.abs_diff(start), t.abs_diff(out_rate), t));
    found.truncate(limit);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, samples: usize) -> Vec<i32> {
        (0..samples)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                (12000.0 * (std::f64::consts::TAU * hz * t).sin()
                    + 4000.0 * (2.0 * std::f64::consts::TAU * hz * t).sin())
                .round() as i32
            })
            .collect()
    }

    /// The first pass and one traversal of a loop `[start, len)`.
    fn looped(pcm: &[i32], start: usize) -> Vec<i32> {
        [pcm, &pcm[start..]].concat()
    }

    #[test]
    fn a_whole_period_loop_has_no_seam_flags() {
        // 32000 / 250 = 128 samples per period; 40 periods from 1280.
        let pcm = tone(32000, 250.0, 1280 + 128 * 40);
        let d = diagnostics(&looped(&pcm, 1280), pcm.len(), 1280, 32000, Some(59.2));
        assert!(
            (d["periods_in_loop"].as_f64().unwrap() - 40.0).abs() < 0.01,
            "{d}"
        );
        assert!(d["phase_jump_periods"].as_f64().unwrap().abs() < 0.01);
        assert!(d["seam"]["waveform_error"].as_f64().unwrap() < 0.02, "{d}");
        assert!(d["seam"]["level_change_db"].as_f64().unwrap().abs() < 0.1);
        assert_eq!(d["sustain_pitch"]["note"], "B3");
        let flags = d["flags"].as_array().unwrap();
        assert!(flags.is_empty(), "{flags:?}");
    }

    #[test]
    fn a_fractional_loop_reports_the_phase_jump_and_waveform_break() {
        // 40.33 periods: the seam cuts a third of a period.
        let pcm = tone(32000, 250.0, 1280 + 128 * 40 + 43);
        let d = diagnostics(&looped(&pcm, 1280), pcm.len(), 1280, 32000, None);
        assert!(
            (d["phase_jump_periods"].as_f64().unwrap() - 0.336).abs() < 0.01,
            "{d}"
        );
        let seam = d["seam"]["waveform_error"].as_f64().unwrap();
        let inside = d["seam"]["waveform_error_inside_loop"].as_f64().unwrap();
        assert!(seam > 0.5 && inside < 0.05, "{seam} {inside}");
        let flags: Vec<&str> = d["flags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        assert!(flags.iter().any(|f| f.contains("phase jumps")), "{flags:?}");
        assert!(
            flags.iter().any(|f| f.contains("waveform breaks")),
            "{flags:?}"
        );
    }

    #[test]
    fn a_level_step_and_root_offset_are_reported() {
        let mut pcm = tone(32000, 250.0, 1280 + 128 * 40);
        let quiet = pcm.len() - 640;
        for v in &mut pcm[quiet..] {
            *v /= 2;
        }
        // Root 60 is C4 (261.6 Hz); 250 Hz is about 79 cents lower.
        let d = diagnostics(&looped(&pcm, 1280), pcm.len(), 1280, 32000, Some(60.0));
        assert!(d["seam"]["level_change_db"].as_f64().unwrap() > 5.0, "{d}");
        let from_root = d["sustain_pitch"]["cents_from_root"].as_f64().unwrap();
        assert!((from_root + 78.7).abs() < 1.0, "{from_root}");
        let flags = d["flags"].to_string();
        assert!(flags.contains("level changes by +6.0"), "{flags}");
        assert!(
            flags.contains("-78.7 cents from the declared root"),
            "{flags}"
        );
        assert!(!flags.contains("phase jumps"), "{flags}");
    }

    #[test]
    fn whole_period_starts_are_aligned_and_nearest_first() {
        // 44100 Hz to 28800 Hz: BRR blocks need source multiples of 49.
        let step = source_step(16, 44100, 28800);
        assert_eq!(step, 49);
        let length = 22050 / step * step;
        let starts = whole_period_starts(length, 6713, step, 44100.0 / 261.6255653, 2);
        assert_eq!(starts.len(), 2);
        for (s, periods) in &starts {
            assert_eq!(s % step, 0);
            assert!((periods - periods.round()).abs() <= WHOLE_PERIOD_TOLERANCE);
        }
        assert!(starts[0].0.abs_diff(6713) <= starts[1].0.abs_diff(6713));
    }

    #[test]
    fn whole_period_rates_fit_blocks_and_exclude_the_requested_rate() {
        let period = 44100.0 / 261.6255653;
        let found = whole_period_rates((22050, 6664), 16, (44100, 28800), period, (3, 131072));
        assert_eq!(found.len(), 3);
        for &(t, s, periods) in &found {
            assert_ne!(t, 28800);
            assert!((21600..=36000).contains(&t), "{t}");
            let step = source_step(16, 44100, t);
            assert_eq!(22050 % step, 0, "{t}");
            assert_eq!(s % step, 0, "{t} {s}");
            assert!((periods - periods.round()).abs() <= WHOLE_PERIOD_TOLERANCE);
        }
        assert!(found[0].1.abs_diff(6664) <= found[2].1.abs_diff(6664));
    }
}
