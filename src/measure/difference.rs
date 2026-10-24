//! Sample-aligned comparison of two signals with the same rate and channels.
//!
//! The offset search maximizes the normalized cross-correlation of the
//! channel-averaged signals over lags within ±`max_offset` samples, computed
//! block by block with the FFT. Difference metrics are then taken over the
//! frames both signals share at that offset.

use serde::Serialize;

use super::spectrum::Fft;
use super::{Audio, db_power, round};

const BLOCK: usize = 1 << 15;
const WINDOW_SECONDS: f64 = 0.4;
const WINDOWS_REPORTED: usize = 5;

fn mono(audio: &Audio, length: usize) -> Vec<f64> {
    let scale = 1.0 / audio.channels.len() as f64;
    (0..length)
        .map(|i| audio.channels.iter().map(|c| f64::from(c[i])).sum::<f64>() * scale)
        .collect()
}

/// Cross-correlation `c[k] = Σ x[n] · y[n + k − max_offset]` for `k` in `0..=2·max_offset`.
fn cross_correlation(x: &[f64], y: &[f64], max_offset: usize) -> Vec<f64> {
    let lags = 2 * max_offset + 1;
    let mut out = vec![0.0; lags];
    if max_offset == 0 {
        out[0] = x.iter().zip(y).map(|(a, b)| a * b).sum();
        return out;
    }
    let block = BLOCK.max((2 * max_offset).next_power_of_two());
    let size = (block + 2 * max_offset).next_power_of_two();
    let plan = Fft::new(size);
    let (mut xr, mut xi, mut yr, mut yi) = (
        vec![0.0; size],
        vec![0.0; size],
        vec![0.0; size],
        vec![0.0; size],
    );
    for start in (0..x.len()).step_by(block) {
        let end = (start + block).min(x.len());
        xr.iter_mut().for_each(|v| *v = 0.0);
        xi.iter_mut().for_each(|v| *v = 0.0);
        yi.iter_mut().for_each(|v| *v = 0.0);
        xr[..end - start].copy_from_slice(&x[start..end]);
        for (i, slot) in yr.iter_mut().enumerate() {
            let at = (start + i) as isize - max_offset as isize;
            *slot = if i < end - start + 2 * max_offset && at >= 0 && (at as usize) < y.len() {
                y[at as usize]
            } else {
                0.0
            };
        }
        plan.forward(&mut xr, &mut xi);
        plan.forward(&mut yr, &mut yi);
        for k in 0..size {
            // conj(X) · Y
            let (a, b, c, d) = (xr[k], -xi[k], yr[k], yi[k]);
            xr[k] = a * c - b * d;
            xi[k] = a * d + b * c;
        }
        plan.inverse(&mut xr, &mut xi);
        for (slot, value) in out.iter_mut().zip(&xr) {
            *slot += value;
        }
    }
    out
}

/// Sum of squares over `lo..hi`, from the total and the sums of the first
/// `head` and last `tail` values.
struct Energy {
    total: f64,
    head: Vec<f64>,
    tail: Vec<f64>,
    len: usize,
}

impl Energy {
    fn new(x: &[f64], head_max: usize, tail_max: usize) -> Self {
        let mut head = vec![0.0];
        for i in 0..head_max.min(x.len()) {
            head.push(head[i] + x[i] * x[i]);
        }
        let mut tail = vec![0.0];
        for i in 0..tail_max.min(x.len()) {
            let v = x[x.len() - 1 - i];
            tail.push(tail[i] + v * v);
        }
        Self {
            total: x.iter().map(|v| v * v).sum(),
            head,
            tail,
            len: x.len(),
        }
    }
    fn between(&self, lo: usize, hi: usize) -> f64 {
        (self.total - self.head[lo] - self.tail[self.len - hi]).max(0.0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Alignment {
    pub searched_offset_samples: usize,
    /// Candidate frame `n + offset` lines up with reference frame `n`; positive means the candidate is later.
    pub offset_samples: i64,
    pub offset_seconds: f64,
    /// Normalized cross-correlation of the channel averages at the offset.
    pub correlation: Option<f64>,
    pub correlation_at_zero: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelDifference {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<usize>,
    /// Uncentered normalized correlation; 1 when the candidate is a positive multiple of the reference.
    pub correlation: Option<f64>,
    pub difference_rms_dbfs: Option<f64>,
    pub difference_peak_dbfs: Option<f64>,
    /// Difference energy relative to the reference energy; null when identical.
    pub residual_db: Option<f64>,
    /// Least-squares gain from reference to candidate.
    pub best_gain_db: Option<f64>,
    /// Residual after applying that gain; a deep value means the change is mostly level.
    pub gain_matched_residual_db: Option<f64>,
    pub differing_samples: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DifferenceWindow {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub difference_rms_dbfs: Option<f64>,
    pub residual_db: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Difference {
    pub alignment: Alignment,
    pub overlap_frames: u64,
    pub unmatched_reference_frames: u64,
    pub unmatched_candidate_frames: u64,
    /// Every shared sample is equal, at offset zero, and the lengths match.
    pub identical: bool,
    /// Not identical, but every sample of a non-empty overlap is equal: the
    /// inputs differ only in length or offset. Omitted when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub overlap_identical: bool,
    pub overall: ChannelDifference,
    pub channels: Vec<ChannelDifference>,
    /// Up to five 400 ms reference windows with the most difference energy, largest first.
    pub largest_difference_windows: Vec<DifferenceWindow>,
}

#[derive(Default, Clone, Copy)]
struct Sums {
    xx: f64,
    yy: f64,
    xy: f64,
    dd: f64,
    peak: f64,
    differing: u64,
    count: u64,
}

impl Sums {
    fn add(&mut self, other: &Sums) {
        self.xx += other.xx;
        self.yy += other.yy;
        self.xy += other.xy;
        self.dd += other.dd;
        self.peak = self.peak.max(other.peak);
        self.differing += other.differing;
        self.count += other.count;
    }
    fn report(&self, channel: Option<usize>) -> ChannelDifference {
        let gain = (self.xx > 0.0).then(|| self.xy / self.xx);
        let matched = gain.map(|g| (self.yy - g * self.xy).max(0.0));
        let relative =
            |e: f64| (self.xx > 0.0 && e > 0.0).then(|| round(10.0 * (e / self.xx).log10(), 2));
        ChannelDifference {
            channel,
            correlation: (self.xx > 0.0 && self.yy > 0.0)
                .then(|| round(self.xy / (self.xx * self.yy).sqrt(), 6)),
            difference_rms_dbfs: (self.count > 0)
                .then(|| db_power(self.dd / self.count as f64))
                .flatten(),
            difference_peak_dbfs: db_power(self.peak * self.peak),
            residual_db: relative(self.dd),
            best_gain_db: gain
                .filter(|g| *g > 0.0)
                .map(|g| round(20.0 * g.log10(), 2)),
            gain_matched_residual_db: matched.and_then(relative),
            differing_samples: self.differing,
        }
    }
}

pub fn difference(
    reference: &Audio,
    candidate: &Audio,
    max_offset: usize,
    start_seconds: f64,
) -> Difference {
    let rate = f64::from(reference.sample_rate);
    let (nx, ny) = (reference.frames(), candidate.frames());
    // Search over the full length of both inputs, so a note late in the
    // longer one can still be found.
    let max_offset = max_offset.min(nx.max(ny).saturating_sub(1));
    let x = mono(reference, nx);
    let y = mono(candidate, ny);
    let correlation = cross_correlation(&x, &y, max_offset);
    let trim = nx.abs_diff(ny) + max_offset;
    let (ex, ey) = (
        Energy::new(&x, max_offset, trim),
        Energy::new(&y, max_offset, trim),
    );
    let normalized = |k: usize| {
        let lag = k as isize - max_offset as isize;
        let lo = (-lag).max(0) as usize;
        let hi = (nx as isize).min(ny as isize - lag);
        if hi <= lo as isize {
            return None;
        }
        let hi = hi as usize;
        let e = ex.between(lo, hi)
            * ey.between((lo as isize + lag) as usize, (hi as isize + lag) as usize);
        (e > 0.0).then(|| correlation[k] / e.sqrt())
    };
    let mut best = (max_offset, normalized(max_offset));
    for k in 0..correlation.len() {
        let value = normalized(k);
        let closer = (k as isize - max_offset as isize).abs()
            < (best.0 as isize - max_offset as isize).abs();
        match (value, best.1) {
            (Some(v), Some(b)) if v > b + 1e-12 || ((v - b).abs() <= 1e-12 && closer) => {
                best = (k, value)
            }
            (Some(_), None) => best = (k, value),
            _ => {}
        }
    }
    let offset = best.0 as i64 - max_offset as i64;
    // Shared frames at the offset: reference n, candidate n + offset.
    let lo = (-offset).max(0) as usize;
    let hi = (nx as i64).min(ny as i64 - offset).max(lo as i64) as usize;
    let window = ((WINDOW_SECONDS * rate).round() as usize).max(1);
    let mut channel_sums = vec![Sums::default(); reference.channels.len()];
    let mut windows: Vec<(usize, f64, f64)> = Vec::new();
    for (c, sums) in channel_sums.iter_mut().enumerate() {
        let (xs, ys) = (&reference.channels[c], &candidate.channels[c]);
        for n in lo..hi {
            let a = f64::from(xs[n]);
            let b = f64::from(ys[(n as i64 + offset) as usize]);
            let d = b - a;
            sums.xx += a * a;
            sums.yy += b * b;
            sums.xy += a * b;
            sums.dd += d * d;
            sums.peak = sums.peak.max(d.abs());
            sums.differing += u64::from(xs[n] != ys[(n as i64 + offset) as usize]);
            sums.count += 1;
            let w = (n - lo) / window;
            if windows.len() <= w {
                windows.resize(w + 1, (0, 0.0, 0.0));
            }
            windows[w].0 = lo + w * window;
            windows[w].1 += d * d;
            windows[w].2 += a * a;
        }
    }
    let mut overall = Sums::default();
    channel_sums.iter().for_each(|s| overall.add(s));
    windows.retain(|w| w.1 > 0.0);
    windows.sort_by(|a, b| b.1.total_cmp(&a.1));
    let largest_difference_windows = windows
        .iter()
        .take(WINDOWS_REPORTED)
        .map(|&(start, dd, xx)| {
            let frames = window.min(hi - start);
            DifferenceWindow {
                start_seconds: round(start_seconds + start as f64 / rate, 4),
                end_seconds: round(start_seconds + (start + frames) as f64 / rate, 4),
                difference_rms_dbfs: db_power(dd / (frames * reference.channels.len()) as f64),
                residual_db: (xx > 0.0).then(|| round(10.0 * (dd / xx).log10(), 2)),
            }
        })
        .collect();
    let zero = normalized(max_offset);
    Difference {
        alignment: Alignment {
            searched_offset_samples: max_offset,
            offset_samples: offset,
            offset_seconds: round(offset as f64 / rate, 6),
            correlation: best.1.map(|v| round(v, 6)),
            correlation_at_zero: zero.map(|v| round(v, 6)),
        },
        overlap_frames: (hi - lo) as u64,
        unmatched_reference_frames: (nx - (hi - lo)) as u64,
        unmatched_candidate_frames: (ny - (hi - lo)) as u64,
        identical: offset == 0 && nx == ny && overall.differing == 0,
        overlap_identical: overall.differing == 0 && hi > lo && !(offset == 0 && nx == ny),
        overall: overall.report(None),
        channels: channel_sums
            .iter()
            .enumerate()
            .map(|(c, s)| s.report(Some(c)))
            .collect(),
        largest_difference_windows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(frames: usize, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..frames)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.5
            })
            .collect()
    }

    fn audio(channels: Vec<Vec<f32>>) -> Audio {
        Audio {
            sample_rate: 8_000,
            channels,
            clip_level: 1.0,
        }
    }

    #[test]
    fn identical_signals_report_identity() {
        let a = audio(vec![noise(20_000, 1), noise(20_000, 2)]);
        let d = difference(&a, &a, 100, 0.0);
        assert!(d.identical && !d.overlap_identical);
        assert_eq!(d.alignment.offset_samples, 0);
        assert_eq!(d.overall.residual_db, None);
        assert_eq!(d.overall.correlation, Some(1.0));
        assert!(d.largest_difference_windows.is_empty());
    }

    #[test]
    fn a_delay_within_the_bound_is_found_and_removed() {
        let source = noise(30_000, 7);
        let reference = audio(vec![source[..25_000].to_vec()]);
        // The candidate starts 37 samples later and is 6 dB quieter.
        let delayed: Vec<f32> = std::iter::repeat_n(0.0, 37)
            .chain(source.iter().map(|x| x * 0.5))
            .take(25_100)
            .collect();
        let candidate = audio(vec![delayed]);
        let d = difference(&reference, &candidate, 80, 0.0);
        assert_eq!(d.alignment.offset_samples, 37);
        assert!(d.alignment.correlation.unwrap() > 0.9999);
        assert!(d.alignment.correlation_at_zero.unwrap() < 0.1);
        assert!((d.overall.best_gain_db.unwrap() + 6.02).abs() < 0.01);
        assert!(
            d.overall
                .gain_matched_residual_db
                .is_none_or(|r| r < -100.0)
        );
        assert!((d.overall.residual_db.unwrap() + 6.02).abs() < 0.05);
        assert_eq!(d.overlap_frames, 25_000);
        assert_eq!(d.unmatched_candidate_frames, 100);
        // Outside the bound, no alignment is claimed.
        let d = difference(&reference, &candidate, 20, 0.0);
        assert!(d.alignment.offset_samples.abs() <= 20);
        assert!(d.alignment.correlation.unwrap() < 0.2);
    }

    #[test]
    fn a_short_reference_is_found_late_in_a_longer_candidate() {
        // A 1000-sample note that a capture holds starting at frame 3000.
        let note = noise(1_000, 11);
        let capture: Vec<f32> = std::iter::repeat_n(0.0, 3_000)
            .chain(note.iter().copied())
            .chain(std::iter::repeat_n(0.0, 500))
            .collect();
        let (note, capture) = (audio(vec![note]), audio(vec![capture]));
        let d = difference(&note, &capture, 4_000, 0.0);
        assert_eq!(d.alignment.offset_samples, 3_000);
        assert!(d.alignment.correlation.unwrap() > 0.9999);
        assert_eq!(d.overall.differing_samples, 0);
        assert!(!d.identical && d.overlap_identical);
        // The reverse direction finds the same alignment with the sign flipped.
        let d = difference(&capture, &note, 4_000, 0.0);
        assert_eq!(d.alignment.offset_samples, -3_000);
    }

    #[test]
    fn an_identical_overlap_of_different_lengths_is_reported() {
        let source = noise(10_000, 5);
        let short = audio(vec![source[..9_000].to_vec()]);
        let d = difference(&audio(vec![source]), &short, 0, 0.0);
        assert!(!d.identical && d.overlap_identical);
        assert_eq!(d.overlap_frames, 9_000);
        assert_eq!(d.unmatched_reference_frames, 1_000);
        assert_eq!(d.unmatched_candidate_frames, 0);
    }

    #[test]
    fn a_local_change_is_located() {
        let source = noise(16_000, 3);
        let mut changed = source.clone();
        // 0.1 s burst starting at 1.25 s.
        for x in &mut changed[10_000..10_800] {
            *x = -*x;
        }
        let d = difference(&audio(vec![source]), &audio(vec![changed]), 0, 1.0);
        assert!(!d.identical && !d.overlap_identical);
        assert_eq!(d.overall.differing_samples, 800);
        let top = &d.largest_difference_windows[0];
        // Window of 3200 frames starting at frame 9600, offset by the 1 s range start.
        assert_eq!((top.start_seconds, top.end_seconds), (2.2, 2.6));
        assert_eq!(d.largest_difference_windows.len(), 1);
    }
}
