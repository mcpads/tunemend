//! Fundamental-frequency tracking with the YIN estimator.
//!
//! The method follows de Cheveigné and Kawahara, "YIN, a fundamental
//! frequency estimator for speech and music", JASA 111(4), 2002, steps 1–5:
//! the difference function d(τ) over an integration window, its cumulative
//! mean normalized form d'(τ), the first dip of d' below an absolute
//! threshold (walked down to its local minimum), and parabolic interpolation
//! of the raw difference function around that lag, which the paper prefers
//! because interpolating d' biases the period. Step 6 (best local estimate)
//! is not applied: every window is estimated alone.
//!
//! Each declared window of `window` samples holds an integration window of
//! `window − τmax − 1` samples and the lags it is compared with, so every
//! estimate reads exactly the declared window. Channels are averaged to mono.
//! The cross term of d(τ) comes from one FFT correlation per window; the
//! three lags used for interpolation are recomputed directly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::round;
use super::spectrum::Fft;

pub const DEFAULT_MIN_HZ: f64 = 50.0;
pub const DEFAULT_MAX_HZ: f64 = 4000.0;
pub const DEFAULT_HOP_SECONDS: f64 = 0.01;
pub const DEFAULT_THRESHOLD: f64 = 0.15;
pub const DEFAULT_A4_HZ: f64 = 440.0;
/// Largest number of windows listed in one result.
pub const MAX_LISTED_WINDOWS: usize = 2000;
/// Largest number of windows analysed in one input.
pub const MAX_WINDOWS: usize = 200_000;
/// Runs of fewer windows than this are not listed as notes, by default.
pub const DEFAULT_MIN_NOTE_WINDOWS: usize = 2;
/// Notes of fewer windows than this are marked `few_windows`.
pub const FEW_NOTE_WINDOWS: usize = 3;
/// A window is DC-dominated when its mean's power is at least this many
/// times the power of its variation around the mean (10 dB).
pub const DC_DOMINANCE_RATIO: f64 = 10.0;

/// Fundamental-frequency analysis of monophonic material.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PitchSettings {
    /// Lowest fundamental searched, 20–2000 Hz; default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_hz: Option<f64>,
    /// Highest fundamental searched, above `min_hz` and at most a quarter of
    /// the sample rate; default 4000 (or a quarter of the rate if lower).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hz: Option<f64>,
    /// Samples read per estimate, at least two periods of `min_hz` and at
    /// most 0.5 s; default two periods of `min_hz` (0.04 s at 50 Hz).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<f64>,
    /// Step between window starts, 0.001–1 s; default 0.01.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop_seconds: Option<f64>,
    /// YIN absolute threshold on the normalized difference, 0.01–0.5;
    /// default 0.15. Lower marks more windows unvoiced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Frequency of A4 for note names and cents; default 440.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a4_hz: Option<f64>,
    /// List every window (default true). The summary and note segments are
    /// always reported; at most 2000 windows can be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<bool>,
    /// Shortest run of windows on one note that is listed as a note, 1–100;
    /// default 2, so a single-window blip at a note change is omitted (and
    /// counted in `short_runs_omitted`). 1 lists every run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_note_windows: Option<u32>,
}

/// Resolved settings, in samples where the algorithm works in samples.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PitchConfig {
    pub min_hz: f64,
    pub max_hz: f64,
    pub window_seconds: f64,
    pub hop_seconds: f64,
    pub threshold: f64,
    pub a4_hz: f64,
    pub silence_threshold_dbfs: f64,
    pub window_samples: usize,
    pub hop_samples: usize,
    /// Integration window W of the difference function.
    pub integration_samples: usize,
    /// Shortest run of windows listed as a note.
    pub min_note_windows: usize,
    #[serde(skip)]
    pub min_lag: usize,
    #[serde(skip)]
    pub max_lag: usize,
    #[serde(skip)]
    pub list_windows: bool,
}

/// A setting outside its range: the field name and the reason.
pub type SettingProblem = (&'static str, String);

impl PitchSettings {
    pub fn resolve(
        &self,
        sample_rate: u32,
        silence_threshold_dbfs: f64,
    ) -> Result<PitchConfig, SettingProblem> {
        let rate = f64::from(sample_rate);
        let finite = |field: &'static str, value: f64| {
            if value.is_finite() {
                Ok(value)
            } else {
                Err((field, "must be a finite number".to_owned()))
            }
        };
        let min_hz = finite("min_hz", self.min_hz.unwrap_or(DEFAULT_MIN_HZ))?;
        if !(20.0..=2000.0).contains(&min_hz) {
            return Err(("min_hz", "must be 20–2000 Hz".into()));
        }
        let quarter = rate / 4.0;
        let max_hz = finite(
            "max_hz",
            self.max_hz.unwrap_or_else(|| DEFAULT_MAX_HZ.min(quarter)),
        )?;
        if max_hz <= min_hz || max_hz > quarter {
            return Err((
                "max_hz",
                format!(
                    "must be above min_hz ({min_hz} Hz) and at most a quarter of the {sample_rate} Hz rate ({quarter} Hz)"
                ),
            ));
        }
        let max_lag = (rate / min_hz).ceil() as usize;
        let min_lag = ((rate / max_hz).floor() as usize).max(2);
        let shortest = 2 * (max_lag + 1);
        let window_seconds = finite(
            "window_seconds",
            self.window_seconds
                .unwrap_or((shortest as f64 / rate * 1e6).ceil() / 1e6),
        )?;
        let window_samples = (window_seconds * rate).round() as usize;
        if window_samples < shortest || window_seconds > 0.5 {
            return Err((
                "window_seconds",
                format!(
                    "must hold two periods of min_hz ({:.6} s, {shortest} samples) and be at most 0.5 s",
                    shortest as f64 / rate
                ),
            ));
        }
        let hop_seconds = finite(
            "hop_seconds",
            self.hop_seconds.unwrap_or(DEFAULT_HOP_SECONDS),
        )?;
        let hop_samples = (hop_seconds * rate).round() as usize;
        if !(0.001..=1.0).contains(&hop_seconds) || hop_samples == 0 {
            return Err(("hop_seconds", "must be 0.001–1 s".into()));
        }
        let threshold = finite("threshold", self.threshold.unwrap_or(DEFAULT_THRESHOLD))?;
        if !(0.01..=0.5).contains(&threshold) {
            return Err(("threshold", "must be 0.01–0.5".into()));
        }
        let a4_hz = finite("a4_hz", self.a4_hz.unwrap_or(DEFAULT_A4_HZ))?;
        if !(400.0..=480.0).contains(&a4_hz) {
            return Err(("a4_hz", "must be 400–480 Hz".into()));
        }
        let min_note_windows = self
            .min_note_windows
            .map_or(DEFAULT_MIN_NOTE_WINDOWS, |n| n as usize);
        if !(1..=100).contains(&min_note_windows) {
            return Err(("min_note_windows", "must be 1–100".into()));
        }
        Ok(PitchConfig {
            min_hz,
            max_hz,
            window_seconds: round(window_samples as f64 / rate, 6),
            hop_seconds: round(hop_samples as f64 / rate, 6),
            threshold,
            a4_hz,
            silence_threshold_dbfs,
            window_samples,
            hop_samples,
            integration_samples: window_samples - max_lag - 1,
            min_note_windows,
            min_lag,
            max_lag,
            list_windows: self.windows.unwrap_or(true),
        })
    }
}

/// Why a window has no fundamental.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unvoiced {
    /// RMS below the silence threshold.
    Silent,
    /// No dip of the normalized difference below the threshold, including a
    /// window whose samples are all equal (a held level).
    Aperiodic,
}

/// One analysis window.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PitchWindow {
    /// Centre of the integration window, in input seconds. The estimate
    /// compares that span with itself one period later, so it describes the
    /// signal from this time to about half a period after it.
    pub time_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hz: Option<f64>,
    /// Nearest equal-tempered MIDI note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<i32>,
    /// Deviation from that note, −50 to +50.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cents: Option<f64>,
    /// 1 − d'(τ) at the chosen or deepest lag: 1 is perfectly periodic.
    pub periodicity: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unvoiced: Option<Unvoiced>,
    /// The window's mean (DC) has at least 10 times the power of its
    /// variation, so its level above the silence threshold is mostly a
    /// constant offset and any estimate rests on a small ripple. Omitted when
    /// false; silent windows are never marked.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dc_dominated: bool,
    #[serde(skip)]
    pub exact_hz: Option<f64>,
}

/// Consecutive voiced windows sharing their nearest note.
#[derive(Debug, Clone, Serialize)]
pub struct NoteSegment {
    /// Times of the first and last window.
    pub from_seconds: f64,
    pub to_seconds: f64,
    pub note: i32,
    pub name: String,
    pub windows: usize,
    pub median_hz: f64,
    pub median_cents: f64,
    pub lowest_cents: f64,
    pub highest_cents: f64,
    /// Mean periodicity of the note's windows: 1 is perfectly periodic.
    pub mean_periodicity: f64,
    /// Windows of the note marked `dc_dominated`; omitted when none.
    #[serde(skip_serializing_if = "is_zero")]
    pub dc_dominated_windows: usize,
    /// Why the note may not be a played note: `few_windows` (fewer than 3
    /// windows, such as a transition between notes) or `dc_dominated` (at
    /// least half its windows). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub low_confidence: Vec<&'static str>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize)]
pub struct PitchSummary {
    pub windows: usize,
    pub voiced_windows: usize,
    pub silent_windows: usize,
    pub aperiodic_windows: usize,
    pub median_hz: Option<f64>,
    pub lowest_hz: Option<f64>,
    pub highest_hz: Option<f64>,
    /// Nearest note of the median and the median's deviation from it.
    pub median_note: Option<i32>,
    pub median_note_name: Option<String>,
    pub median_note_cents: Option<f64>,
    /// Median absolute deviation of voiced windows from their nearest notes.
    pub median_abs_cents: Option<f64>,
    pub median_periodicity: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PitchTrack {
    pub method: &'static str,
    pub settings: PitchConfig,
    pub summary: PitchSummary,
    pub notes: Vec<NoteSegment>,
    /// Runs of voiced windows on one note shorter than `min_note_windows`,
    /// not listed in `notes`; omitted when none.
    #[serde(skip_serializing_if = "is_zero")]
    pub short_runs_omitted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<PitchWindow>>,
}

pub const METHOD: &str = "YIN (de Cheveigné and Kawahara 2002) steps 1–5 on the channel average: difference function, cumulative mean normalized difference, first dip below the absolute threshold, parabolic interpolation of the raw difference; each window alone. Monophonic material: on chords or mixes it reports the dominant common periodicity (which may be a note no voice plays, below the chord) with low periodicity, so measure a voice's solo render instead";

/// MIDI note number (fractional) of a frequency.
pub fn midi_note(hz: f64, a4_hz: f64) -> f64 {
    69.0 + 12.0 * (hz / a4_hz).log2()
}

pub fn note_name(note: i32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[note.rem_euclid(12) as usize],
        note.div_euclid(12) - 1
    )
}

/// Difference between two frequencies in cents.
pub fn cents_between(from_hz: f64, to_hz: f64) -> f64 {
    1200.0 * (to_hz / from_hz).log2()
}

/// Channel average as f64.
pub fn mono(channels: &[Vec<f32>]) -> Vec<f64> {
    let count = channels.len().max(1) as f64;
    let frames = channels.first().map_or(0, Vec::len);
    (0..frames)
        .map(|i| channels.iter().map(|c| f64::from(c[i])).sum::<f64>() / count)
        .collect()
}

/// Window starts at every hop that fits in `frames`.
pub fn window_starts(frames: usize, config: &PitchConfig) -> Vec<usize> {
    if frames < config.window_samples {
        return vec![];
    }
    (0..=(frames - config.window_samples))
        .step_by(config.hop_samples)
        .collect()
}

/// Reusable FFT buffers for one window size.
pub struct Estimator<'a> {
    config: &'a PitchConfig,
    rate: f64,
    fft: Fft,
    size: usize,
    re_a: Vec<f64>,
    im_a: Vec<f64>,
    re_b: Vec<f64>,
    im_b: Vec<f64>,
    difference: Vec<f64>,
    normalized: Vec<f64>,
}

impl<'a> Estimator<'a> {
    pub fn new(config: &'a PitchConfig, sample_rate: u32) -> Self {
        let size = config.window_samples.next_power_of_two();
        Self {
            config,
            rate: f64::from(sample_rate),
            fft: Fft::new(size),
            size,
            re_a: vec![0.0; size],
            im_a: vec![0.0; size],
            re_b: vec![0.0; size],
            im_b: vec![0.0; size],
            difference: vec![0.0; config.max_lag + 2],
            normalized: vec![0.0; config.max_lag + 2],
        }
    }

    /// Exact d(τ) by direct summation.
    fn direct_difference(&self, x: &[f64], lag: usize) -> f64 {
        let w = self.config.integration_samples;
        (0..w).map(|j| (x[j] - x[j + lag]).powi(2)).sum()
    }

    /// Estimates the window `samples[start..start + window]`; `time` is the
    /// centre of its integration window in input seconds.
    pub fn estimate(&mut self, samples: &[f64], start: usize, time: f64) -> PitchWindow {
        let config = self.config;
        let x = &samples[start..start + config.window_samples];
        let w = config.integration_samples;
        let max_lag = config.max_lag;
        let mean_square = x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64;
        let mean = x.iter().sum::<f64>() / x.len() as f64;
        let dc_power = mean * mean;
        let dc_dominated =
            mean_square > 0.0 && dc_power >= DC_DOMINANCE_RATIO * (mean_square - dc_power).max(0.0);
        let level_db = if mean_square > 0.0 {
            10.0 * mean_square.log10()
        } else {
            f64::NEG_INFINITY
        };
        let window = |periodicity: f64, unvoiced: Option<Unvoiced>, hz: Option<f64>| {
            let (note, cents) = match hz {
                Some(hz) => {
                    let exact = midi_note(hz, config.a4_hz);
                    let note = exact.round();
                    (Some(note as i32), Some(round((exact - note) * 100.0, 1)))
                }
                None => (None, None),
            };
            PitchWindow {
                time_seconds: round(time, 4),
                hz: hz.map(|h| round(h, 2)),
                note,
                cents,
                periodicity: round(periodicity.clamp(0.0, 1.0), 3),
                unvoiced,
                dc_dominated: dc_dominated && unvoiced != Some(Unvoiced::Silent),
                exact_hz: hz,
            }
        };
        if level_db < config.silence_threshold_dbfs {
            return window(0.0, Some(Unvoiced::Silent), None);
        }
        // A constant window has d(τ) = 0 at every lag, so d'(τ) is 1 and no
        // dip exists; the FFT path would otherwise turn rounding residue
        // into a spurious period.
        if x.iter().all(|&v| v == x[0]) {
            return window(0.0, Some(Unvoiced::Aperiodic), None);
        }
        // r(τ) = Σ_{j<W} x[j]·x[j+τ] by circular correlation; no lag wraps
        // because the window holds W + τmax + 1 ≤ size samples.
        self.re_a.iter_mut().for_each(|v| *v = 0.0);
        self.im_a.iter_mut().for_each(|v| *v = 0.0);
        self.im_b.iter_mut().for_each(|v| *v = 0.0);
        self.re_a[..w].copy_from_slice(&x[..w]);
        self.re_b[..x.len()].copy_from_slice(x);
        self.re_b[x.len()..].iter_mut().for_each(|v| *v = 0.0);
        self.fft.forward(&mut self.re_a, &mut self.im_a);
        self.fft.forward(&mut self.re_b, &mut self.im_b);
        for k in 0..self.size {
            let (ar, ai, br, bi) = (self.re_a[k], self.im_a[k], self.re_b[k], self.im_b[k]);
            // conj(A)·B
            self.re_b[k] = ar * br + ai * bi;
            self.im_b[k] = ar * bi - ai * br;
        }
        self.fft.inverse(&mut self.re_b, &mut self.im_b);
        let mut prefix = vec![0.0; x.len() + 1];
        for (i, v) in x.iter().enumerate() {
            prefix[i + 1] = prefix[i] + v * v;
        }
        let energy = |from: usize| prefix[from + w] - prefix[from];
        let e0 = energy(0);
        for lag in 0..=max_lag + 1 {
            self.difference[lag] = (e0 + energy(lag) - 2.0 * self.re_b[lag]).max(0.0);
        }
        self.normalized[0] = 1.0;
        let mut running = 0.0;
        for lag in 1..=max_lag + 1 {
            running += self.difference[lag];
            self.normalized[lag] = if running > 0.0 {
                self.difference[lag] * lag as f64 / running
            } else {
                1.0
            };
        }
        let range = config.min_lag..=max_lag;
        let chosen = range
            .clone()
            .find(|&lag| self.normalized[lag] < config.threshold);
        let Some(mut lag) = chosen else {
            let deepest = range
                .map(|lag| self.normalized[lag])
                .fold(f64::INFINITY, f64::min);
            return window(1.0 - deepest, Some(Unvoiced::Aperiodic), None);
        };
        while lag < max_lag && self.normalized[lag + 1] < self.normalized[lag] {
            lag += 1;
        }
        let periodicity = 1.0 - self.normalized[lag];
        let (before, at, after) = (
            self.direct_difference(x, lag - 1),
            self.direct_difference(x, lag),
            self.direct_difference(x, lag + 1),
        );
        let curvature = before - 2.0 * at + after;
        let shift = if curvature > 0.0 {
            (0.5 * (before - after) / curvature).clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let period = lag as f64 + shift;
        window(periodicity, None, Some(self.rate / period))
    }
}

/// Lower median: always one of the values, so the median of a two-note
/// line is one of its notes rather than a frequency between them.
fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[(values.len() - 1) / 2])
}

/// Estimates every window starting at `starts` of `samples`.
pub fn estimate_windows(
    samples: &[f64],
    sample_rate: u32,
    config: &PitchConfig,
    starts: &[usize],
    start_seconds: f64,
) -> Vec<PitchWindow> {
    let mut estimator = Estimator::new(config, sample_rate);
    let rate = f64::from(sample_rate);
    let centre = config.integration_samples as f64 / 2.0;
    starts
        .iter()
        .map(|&s| estimator.estimate(samples, s, start_seconds + (s as f64 + centre) / rate))
        .collect()
}

pub fn summarize(windows: &[PitchWindow], a4_hz: f64) -> PitchSummary {
    let voiced: Vec<&PitchWindow> = windows.iter().filter(|w| w.exact_hz.is_some()).collect();
    let mut hz: Vec<f64> = voiced.iter().filter_map(|w| w.exact_hz).collect();
    let mut abs_cents: Vec<f64> = voiced
        .iter()
        .map(|w| {
            let exact = midi_note(w.exact_hz.expect("voiced"), a4_hz);
            ((exact - exact.round()) * 100.0).abs()
        })
        .collect();
    let mut periodicity: Vec<f64> = voiced.iter().map(|w| w.periodicity).collect();
    let median_hz = median(&mut hz);
    let median_note = median_hz.map(|h| midi_note(h, a4_hz).round() as i32);
    PitchSummary {
        windows: windows.len(),
        voiced_windows: voiced.len(),
        silent_windows: windows
            .iter()
            .filter(|w| w.unvoiced == Some(Unvoiced::Silent))
            .count(),
        aperiodic_windows: windows
            .iter()
            .filter(|w| w.unvoiced == Some(Unvoiced::Aperiodic))
            .count(),
        median_hz: median_hz.map(|h| round(h, 2)),
        lowest_hz: hz.first().map(|h| round(*h, 2)),
        highest_hz: hz.last().map(|h| round(*h, 2)),
        median_note,
        median_note_name: median_note.map(note_name),
        median_note_cents: median_hz.map(|h| {
            let exact = midi_note(h, a4_hz);
            round((exact - exact.round()) * 100.0, 1)
        }),
        median_abs_cents: median(&mut abs_cents).map(|c| round(c, 1)),
        median_periodicity: median(&mut periodicity).map(|p| round(p, 3)),
    }
}

/// Runs of voiced windows on one nearest note, at least `min_windows` long,
/// and the number of shorter runs left out.
pub fn note_segments(
    windows: &[PitchWindow],
    a4_hz: f64,
    min_windows: usize,
) -> (Vec<NoteSegment>, usize) {
    let mut segments = Vec::new();
    let mut omitted = 0;
    let mut i = 0;
    while i < windows.len() {
        let Some(note) = windows[i].note else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < windows.len() && windows[j + 1].note == Some(note) {
            j += 1;
        }
        let run = &windows[i..=j];
        if run.len() < min_windows {
            omitted += 1;
            i = j + 1;
            continue;
        }
        let mut hz: Vec<f64> = run.iter().filter_map(|w| w.exact_hz).collect();
        let mut cents: Vec<f64> = hz
            .iter()
            .map(|&h| (midi_note(h, a4_hz) - f64::from(note)) * 100.0)
            .collect();
        let median_hz = median(&mut hz).expect("non-empty");
        let median_cents = median(&mut cents).expect("non-empty");
        let mean_periodicity = run.iter().map(|w| w.periodicity).sum::<f64>() / run.len() as f64;
        let dc_dominated_windows = run.iter().filter(|w| w.dc_dominated).count();
        let mut low_confidence = Vec::new();
        if run.len() < FEW_NOTE_WINDOWS {
            low_confidence.push("few_windows");
        }
        if 2 * dc_dominated_windows >= run.len() {
            low_confidence.push("dc_dominated");
        }
        segments.push(NoteSegment {
            from_seconds: run[0].time_seconds,
            to_seconds: run[run.len() - 1].time_seconds,
            note,
            name: note_name(note),
            windows: run.len(),
            median_hz: round(median_hz, 2),
            median_cents: round(median_cents, 1),
            lowest_cents: round(cents[0], 1),
            highest_cents: round(cents[cents.len() - 1], 1),
            mean_periodicity: round(mean_periodicity, 3),
            dc_dominated_windows,
            low_confidence,
        });
        i = j + 1;
    }
    (segments, omitted)
}

/// Tracks the fundamental of `channels` (averaged) over the whole input.
pub fn track(
    channels: &[Vec<f32>],
    sample_rate: u32,
    config: &PitchConfig,
    start_seconds: f64,
) -> PitchTrack {
    let samples = mono(channels);
    let starts = window_starts(samples.len(), config);
    let windows = estimate_windows(&samples, sample_rate, config, &starts, start_seconds);
    let (notes, short_runs_omitted) =
        note_segments(&windows, config.a4_hz, config.min_note_windows);
    PitchTrack {
        method: METHOD,
        settings: *config,
        summary: summarize(&windows, config.a4_hz),
        notes,
        short_runs_omitted,
        windows: config.list_windows.then_some(windows),
    }
}

/// One reference window compared with the candidate window at the same
/// aligned time.
#[derive(Debug, Clone, Serialize)]
pub struct PitchDifferenceWindow {
    /// Reference window time, in input seconds.
    pub time_seconds: f64,
    pub reference_hz: f64,
    pub candidate_hz: f64,
    /// Candidate minus reference.
    pub difference_cents: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PitchComparison {
    pub method: &'static str,
    pub settings: PitchConfig,
    /// Candidate windows start `offset_samples` after the reference's, from
    /// the comparison's alignment.
    pub offset_samples: i64,
    pub reference: PitchSummary,
    pub candidate: PitchSummary,
    /// Window pairs voiced in both.
    pub compared_windows: usize,
    /// Pairs voiced in exactly one of the inputs.
    pub voicing_differences: usize,
    /// Reference windows whose aligned candidate window lies outside it.
    pub unmatched_windows: usize,
    pub median_difference_cents: Option<f64>,
    pub mean_abs_difference_cents: Option<f64>,
    pub max_abs_difference_cents: Option<f64>,
    /// Pairs whose nearest notes differ.
    pub different_note_windows: usize,
    /// Up to five pairs with the largest absolute difference.
    pub largest_differences: Vec<PitchDifferenceWindow>,
}

/// Compares the fundamental of two inputs window by window.
pub fn compare(
    reference: (&[Vec<f32>], f64),
    candidate: &[Vec<f32>],
    sample_rate: u32,
    config: &PitchConfig,
    offset_samples: i64,
) -> PitchComparison {
    let (r, c) = (mono(reference.0), mono(candidate));
    let start_seconds = reference.1;
    let reference_starts = window_starts(r.len(), config);
    let reference_windows =
        estimate_windows(&r, sample_rate, config, &reference_starts, start_seconds);
    let last = c.len().checked_sub(config.window_samples);
    let pairs: Vec<(usize, Option<usize>)> = reference_starts
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let at = s as i64 + offset_samples;
            let fits = at >= 0 && last.is_some_and(|l| at as usize <= l);
            (i, fits.then_some(at as usize))
        })
        .collect();
    let candidate_starts: Vec<usize> = pairs.iter().filter_map(|p| p.1).collect();
    let candidate_windows = estimate_windows(
        &c,
        sample_rate,
        config,
        &candidate_starts,
        start_seconds + offset_samples as f64 / f64::from(sample_rate),
    );
    let mut differences = Vec::new();
    let (mut voicing, mut unmatched, mut different_notes) = (0, 0, 0);
    let mut next = candidate_windows.iter();
    for (i, at) in &pairs {
        let reference_window = &reference_windows[*i];
        if at.is_none() {
            unmatched += 1;
            continue;
        }
        let candidate_window = next.next().expect("one per matched start");
        match (reference_window.exact_hz, candidate_window.exact_hz) {
            (Some(a), Some(b)) => {
                if reference_window.note != candidate_window.note {
                    different_notes += 1;
                }
                differences.push(PitchDifferenceWindow {
                    time_seconds: reference_window.time_seconds,
                    reference_hz: round(a, 2),
                    candidate_hz: round(b, 2),
                    difference_cents: cents_between(a, b),
                });
            }
            (None, None) => {}
            _ => voicing += 1,
        }
    }
    let mut cents: Vec<f64> = differences.iter().map(|d| d.difference_cents).collect();
    let mean_abs = (!cents.is_empty())
        .then(|| cents.iter().map(|c| c.abs()).sum::<f64>() / cents.len() as f64);
    let max_abs = cents.iter().map(|c| c.abs()).reduce(f64::max);
    let median_cents = median(&mut cents);
    let mut largest = differences.clone();
    largest.sort_by(|a, b| {
        b.difference_cents
            .abs()
            .total_cmp(&a.difference_cents.abs())
            .then(a.time_seconds.total_cmp(&b.time_seconds))
    });
    largest.truncate(5);
    largest
        .iter_mut()
        .for_each(|d| d.difference_cents = round(d.difference_cents, 1));
    PitchComparison {
        method: METHOD,
        settings: *config,
        offset_samples,
        reference: summarize(&reference_windows, config.a4_hz),
        candidate: summarize(&candidate_windows, config.a4_hz),
        compared_windows: differences.len(),
        voicing_differences: voicing,
        unmatched_windows: unmatched,
        median_difference_cents: median_cents.map(|c| round(c, 1)),
        mean_abs_difference_cents: mean_abs.map(|c| round(c, 1)),
        max_abs_difference_cents: max_abs.map(|c| round(c, 1)),
        different_note_windows: different_notes,
        largest_differences: largest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const RATE: u32 = 44_100;

    fn config(settings: PitchSettings) -> PitchConfig {
        settings.resolve(RATE, -60.0).unwrap()
    }

    fn signal(seconds: f64, mut wave: impl FnMut(f64) -> f64) -> Vec<Vec<f32>> {
        let n = (seconds * f64::from(RATE)) as usize;
        vec![
            (0..n)
                .map(|i| wave(i as f64 / f64::from(RATE)) as f32)
                .collect(),
        ]
    }

    /// Phase-accumulated waveform of instantaneous frequency `hz(t)`, with
    /// `shape` mapping a phase in cycles to a sample.
    fn oscillator(
        seconds: f64,
        hz: impl Fn(f64) -> f64,
        shape: impl Fn(f64) -> f64,
    ) -> Vec<Vec<f32>> {
        let n = (seconds * f64::from(RATE)) as usize;
        let mut phase = 0.0f64;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(shape(phase) as f32);
            phase = (phase + hz(i as f64 / f64::from(RATE)) / f64::from(RATE)).fract();
        }
        vec![out]
    }

    fn worst_cents(track: &PitchTrack, expected: impl Fn(f64) -> f64) -> f64 {
        let windows = track.windows.as_ref().unwrap();
        assert!(windows.iter().all(|w| w.exact_hz.is_some()), "all voiced");
        windows
            .iter()
            .map(|w| cents_between(expected(w.time_seconds), w.exact_hz.unwrap()).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn sines_across_the_range_are_within_a_cent() {
        let settings = config(PitchSettings::default());
        for hz in [55.0, 110.0, 261.63, 440.0, 1000.0, 1760.0, 3520.0] {
            let audio = signal(0.3, |t| 0.5 * (2.0 * PI * hz * t).sin());
            let result = track(&audio, RATE, &settings, 0.0);
            let worst = worst_cents(&result, |_| hz);
            assert!(worst < 1.0, "{hz} Hz: {worst} cents");
        }
    }

    #[test]
    fn chip_waveforms_and_a_missing_fundamental_report_the_period() {
        let settings = config(PitchSettings::default());
        let hz = 220.0;
        let cases: Vec<(&str, Vec<Vec<f32>>)> = vec![
            (
                "square",
                oscillator(0.3, |_| hz, |p| if p < 0.5 { 0.5 } else { -0.5 }),
            ),
            (
                "pulse 12.5%",
                oscillator(0.3, |_| hz, |p| if p < 0.125 { 0.5 } else { -0.5 }),
            ),
            (
                "4-bit triangle",
                oscillator(
                    0.3,
                    |_| hz,
                    |p| {
                        let level = (32.0 * p).floor() as i32;
                        let step = if level < 16 { level } else { 31 - level };
                        f64::from(step) / 7.5 - 1.0
                    },
                ),
            ),
            (
                "harmonics 2-5 only",
                signal(0.3, |t| {
                    (2..=5)
                        .map(|k| 0.2 * (2.0 * PI * hz * f64::from(k) * t).sin())
                        .sum()
                }),
            ),
        ];
        for (name, audio) in cases {
            let result = track(&audio, RATE, &settings, 0.0);
            let worst = worst_cents(&result, |_| hz);
            // Naive (aliased) chip waveforms jitter by up to a sample per period.
            assert!(worst < 5.0, "{name}: {worst} cents");
            assert_eq!(result.summary.median_note, Some(57), "{name}");
        }
    }

    #[test]
    fn vibrato_is_followed_and_note_segments_split_at_note_changes() {
        let settings = config(PitchSettings::default());
        let vibrato = |t: f64| 440.0 * 2f64.powf(0.4 / 12.0 * (2.0 * PI * 5.5 * t).sin());
        let audio = oscillator(1.0, vibrato, |p| (2.0 * PI * p).sin());
        let result = track(&audio, RATE, &settings, 0.0);
        // The 20 ms integration window averages the period of a 5.5 Hz
        // vibrato; the estimate is close to the instantaneous value at its time.
        let worst = worst_cents(&result, vibrato);
        assert!(worst < 6.0, "vibrato: {worst} cents");
        assert_eq!(result.notes.len(), 1);
        assert!(result.notes[0].highest_cents > 35.0 && result.notes[0].lowest_cents < -35.0);

        let melody = |t: f64| if t < 0.25 { 261.63 } else { 293.66 };
        let audio = oscillator(0.5, melody, |p| if p < 0.25 { 0.4 } else { -0.4 });
        let result = track(&audio, RATE, &settings, 0.0);
        let names: Vec<&str> = result.notes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["C4", "D4"]);
    }

    #[test]
    fn single_window_runs_are_omitted_and_short_notes_flagged() {
        let window = |note: i32| {
            let hz = 440.0 * 2f64.powf(f64::from(note - 69) / 12.0);
            PitchWindow {
                time_seconds: 0.0,
                hz: Some(hz),
                note: Some(note),
                cents: Some(0.0),
                periodicity: 0.9,
                unvoiced: None,
                dc_dominated: false,
                exact_hz: Some(hz),
            }
        };
        let mut windows: Vec<PitchWindow> = [vec![60; 5], vec![61], vec![62; 2], vec![64; 5]]
            .concat()
            .into_iter()
            .map(window)
            .collect();
        let (notes, omitted) = note_segments(&windows, 440.0, DEFAULT_MIN_NOTE_WINDOWS);
        let names: Vec<&str> = notes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!((names, omitted), (vec!["C4", "D4", "E4"], 1));
        assert_eq!(notes[1].low_confidence, ["few_windows"]);
        assert!(notes[0].low_confidence.is_empty());
        assert_eq!(notes[0].mean_periodicity, 0.9);
        let (notes, omitted) = note_segments(&windows, 440.0, 1);
        assert_eq!((notes.len(), omitted), (4, 0));
        windows[0].dc_dominated = true;
        windows[1].dc_dominated = true;
        windows[2].dc_dominated = true;
        let (notes, _) = note_segments(&windows, 440.0, 1);
        assert_eq!(notes[0].dc_dominated_windows, 3);
        assert_eq!(notes[0].low_confidence, ["dc_dominated"]);
    }

    #[test]
    fn a_ripple_on_a_constant_offset_is_marked_dc_dominated() {
        let settings = config(PitchSettings::default());
        let ripple = oscillator(0.2, |_| 220.0, |p| 0.5 + if p < 0.5 { 0.01 } else { -0.01 });
        let result = track(&ripple, RATE, &settings, 0.0);
        let windows = result.windows.as_ref().unwrap();
        assert!(windows.iter().all(|w| w.dc_dominated));
        assert_eq!(result.notes.len(), 1);
        assert!(
            result
                .notes
                .iter()
                .all(|n| n.low_confidence.contains(&"dc_dominated"))
        );
        // A held level has no period at all.
        let held = vec![vec![0.25f32; 8_820]];
        let result = track(&held, RATE, &settings, 0.0);
        assert_eq!(result.summary.aperiodic_windows, result.summary.windows);
        assert!(result.notes.is_empty());
        assert!(
            result
                .windows
                .as_ref()
                .unwrap()
                .iter()
                .all(|w| w.dc_dominated)
        );
        let square = oscillator(0.2, |_| 220.0, |p| if p < 0.125 { 0.5 } else { -0.5 });
        let result = track(&square, RATE, &settings, 0.0);
        assert!(
            result
                .windows
                .as_ref()
                .unwrap()
                .iter()
                .all(|w| !w.dc_dominated)
        );
        assert_eq!(result.notes.len(), 1);
        assert!(result.notes.iter().all(|n| n.low_confidence.is_empty()));
    }

    #[test]
    fn silence_and_noise_are_unvoiced() {
        let settings = config(PitchSettings::default());
        let mut state = 0x1234_5678u32;
        let noise = signal(0.3, |_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            f64::from(state) / f64::from(u32::MAX) - 0.5
        });
        let result = track(&noise, RATE, &settings, 0.0);
        assert_eq!(result.summary.voiced_windows, 0);
        assert!(result.summary.aperiodic_windows > 0);
        let silent = vec![vec![0.0f32; 13_230]];
        let result = track(&silent, RATE, &settings, 0.0);
        assert_eq!(result.summary.silent_windows, result.summary.windows);
        assert_eq!(result.summary.median_hz, None);
    }

    #[test]
    fn comparison_reports_a_transposition_in_cents() {
        let settings = config(PitchSettings::default());
        let a = signal(0.5, |t| 0.4 * (2.0 * PI * 220.0 * t).sin());
        let up = 220.0 * 2f64.powf(3.0 / 12.0);
        let b = signal(0.5, |t| 0.4 * (2.0 * PI * up * t).sin());
        let result = compare((&a, 0.0), &b, RATE, &settings, 0);
        assert!((result.median_difference_cents.unwrap() - 300.0).abs() < 0.5);
        assert_eq!(result.different_note_windows, result.compared_windows);
        assert_eq!(result.voicing_differences, 0);
    }

    #[test]
    fn settings_are_checked() {
        let bad = |s: PitchSettings| s.resolve(RATE, -60.0).unwrap_err().0;
        assert_eq!(
            bad(PitchSettings {
                min_hz: Some(10.0),
                ..Default::default()
            }),
            "min_hz"
        );
        assert_eq!(
            bad(PitchSettings {
                max_hz: Some(20_000.0),
                ..Default::default()
            }),
            "max_hz"
        );
        assert_eq!(
            bad(PitchSettings {
                window_seconds: Some(0.01),
                ..Default::default()
            }),
            "window_seconds"
        );
        assert_eq!(
            bad(PitchSettings {
                min_note_windows: Some(0),
                ..Default::default()
            }),
            "min_note_windows"
        );
        let resolved = config(PitchSettings::default());
        assert_eq!(resolved.min_note_windows, DEFAULT_MIN_NOTE_WINDOWS);
        assert_eq!(resolved.max_lag, 882);
        assert_eq!(resolved.window_samples, 1766);
        assert_eq!(note_name(69), "A4");
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(-1), "B-2");
    }
}
