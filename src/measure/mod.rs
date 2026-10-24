//! Audio measurement: peak, true peak, RMS, BS.1770 loudness, loudness range,
//! DC offset, clipping, silence, band spectrum, fundamental frequency, and
//! sample-aligned comparison.
//!
//! Levels are relative to digital full scale (1.0). `rms_dbfs` and band
//! levels are 10·log10 of the mean square, so a full-scale square wave reads
//! 0 dB and a full-scale sine −3.01 dB. Values that would be −∞ (silence) are
//! `None`.

pub mod difference;
pub mod loudness;
pub mod peak;
pub mod pitch;
pub mod spectrum;
pub mod wav;

use serde::Serialize;

pub use spectrum::Bands;

/// Largest WAV file read for measurement.
pub const MAX_WAV_BYTES: u64 = 256 << 20;
/// Largest number of decoded sample values (frames × channels) per input and range.
pub const MAX_SAMPLE_VALUES: u64 = 1 << 26;
/// Largest offset searched when aligning two inputs.
pub const MAX_OFFSET_SECONDS: f64 = 2.0;
pub const DEFAULT_SILENCE_DBFS: f64 = -60.0;

/// Decoded audio, one normalized vector per channel.
#[derive(Debug, Clone)]
pub struct Audio {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
    /// Magnitude at which a sample counts as clipped (the format's full-scale code).
    pub clip_level: f32,
}

impl Audio {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

pub(crate) fn round(value: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (value * scale).round() / scale
}

/// 10·log10 of a mean square, rounded to 0.01 dB.
pub(crate) fn db_power(power: f64) -> Option<f64> {
    (power > 0.0).then(|| round(10.0 * power.log10(), 2))
}

fn db_amplitude(amplitude: f64) -> Option<f64> {
    db_power(amplitude * amplitude)
}

#[derive(Debug, Clone, Copy)]
pub struct MeasureOptions {
    pub bands: Bands,
    pub per_channel: bool,
    pub silence_threshold_dbfs: f64,
}

impl Default for MeasureOptions {
    fn default() -> Self {
        Self {
            bands: Bands::ThirdOctave,
            per_channel: false,
            silence_threshold_dbfs: DEFAULT_SILENCE_DBFS,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Levels {
    pub sample_peak_dbfs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    pub rms_dbfs: Option<f64>,
    /// Mean sample value per channel, linear.
    pub dc_offset: Vec<f64>,
    /// Samples at the format's full-scale code (or beyond ±1.0 for float).
    pub clipped_samples: u64,
    pub longest_clip_run: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoudnessReport {
    pub integrated_lufs: Option<f64>,
    pub momentary_max_lufs: Option<f64>,
    pub short_term_max_lufs: Option<f64>,
    pub loudness_range_lu: Option<f64>,
    pub relative_gate_lufs: Option<f64>,
    /// 400 ms blocks measured and those kept by both gates.
    pub blocks: usize,
    pub gated_blocks: usize,
    pub short_term_values: usize,
    pub channel_weights: Vec<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Silence {
    pub threshold_dbfs: f64,
    pub leading_seconds: f64,
    pub trailing_seconds: f64,
    pub all_silent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Band {
    pub center_hz: f64,
    pub low_hz: f64,
    pub high_hz: f64,
    /// Mean-square level of the band's content, averaged over channels.
    pub level_db: Option<f64>,
    /// Fraction of the total power.
    pub share: f64,
    /// FFT bins summed; zero means the resolution is too coarse for the band.
    pub bins: usize,
}

/// Shares and centroid of the spectrum after each channel's mean is
/// subtracted, reported when DC carries at least `DC_REPORT_SHARE` of the power.
#[derive(Debug, Clone, Serialize)]
pub struct DcRemovedSpectrum {
    /// Fraction of the total power that is the channels' mean (DC).
    pub dc_share: f64,
    pub centroid_hz: Option<f64>,
    pub below_first_band_share: f64,
    pub above_last_band_share: f64,
    /// Share of each band in `levels` order, of the power without DC.
    pub shares: Vec<f64>,
}

/// DC power share from which `dc_removed` is reported.
pub const DC_REPORT_SHARE: f64 = 0.001;

#[derive(Debug, Clone, Serialize)]
pub struct Spectrum {
    /// The band layout (`octave` or `third_octave`); the bands are `levels`.
    pub bands: Bands,
    pub fft_size: usize,
    pub frames: usize,
    pub resolution_hz: f64,
    pub centroid_hz: Option<f64>,
    pub below_first_band_share: f64,
    pub above_last_band_share: f64,
    pub levels: Vec<Band>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dc_removed: Option<DcRemovedSpectrum>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelMeasurement {
    pub channel: usize,
    pub sample_peak_dbfs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    pub rms_dbfs: Option<f64>,
    pub dc_offset: f64,
    pub clipped_samples: u64,
    /// This channel measured alone with weight 1.
    pub integrated_lufs: Option<f64>,
    pub momentary_max_lufs: Option<f64>,
    pub short_term_max_lufs: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Measurement {
    pub frames: u64,
    pub duration_seconds: f64,
    pub levels: Levels,
    pub loudness: LoudnessReport,
    pub silence: Silence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spectrum: Option<Spectrum>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<ChannelMeasurement>>,
}

struct ChannelLevels {
    peak: f64,
    true_peak: f64,
    square_sum: f64,
    sum: f64,
    clipped: u64,
    longest_run: u64,
}

fn channel_levels(samples: &[f32], clip_level: f32) -> ChannelLevels {
    let (mut square_sum, mut sum, mut clipped, mut run, mut longest_run) = (0.0, 0.0, 0, 0, 0);
    for &x in samples {
        let v = f64::from(x);
        square_sum += v * v;
        sum += v;
        if x.abs() >= clip_level {
            clipped += 1;
            run += 1;
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    ChannelLevels {
        peak: peak::sample_peak(samples),
        true_peak: peak::true_peak(samples),
        square_sum,
        sum,
        clipped,
        longest_run,
    }
}

fn silence(audio: &Audio, threshold_dbfs: f64) -> Silence {
    let threshold = 10f64.powf(threshold_dbfs / 20.0) as f32;
    let frames = audio.frames();
    let loud = |i: usize| audio.channels.iter().any(|c| c[i].abs() >= threshold);
    let leading = (0..frames).find(|&i| loud(i)).unwrap_or(frames);
    let trailing = if leading == frames {
        frames
    } else {
        frames - 1 - (0..frames).rev().find(|&i| loud(i)).expect("found above")
    };
    let rate = f64::from(audio.sample_rate);
    Silence {
        threshold_dbfs,
        leading_seconds: round(leading as f64 / rate, 4),
        trailing_seconds: round(trailing as f64 / rate, 4),
        all_silent: leading == frames,
    }
}

/// Channel-averaged power spectrum of `channels`.
fn average_spectrum(channels: &[Vec<f32>]) -> Option<spectrum::PowerSpectrum> {
    let spectra: Vec<_> = channels
        .iter()
        .map(|c| spectrum::power_spectrum(c))
        .collect::<Option<_>>()?;
    let mut average = spectra[0].clone();
    for other in &spectra[1..] {
        for (a, b) in average.bins.iter_mut().zip(&other.bins) {
            *a += b;
        }
    }
    let count = spectra.len() as f64;
    average.bins.iter_mut().for_each(|b| *b /= count);
    Some(average)
}

/// Fraction of the total power carried by the channels' means.
fn dc_share(channels: &[Vec<f32>]) -> f64 {
    let (mut dc, mut total) = (0.0, 0.0);
    for channel in channels {
        let n = channel.len().max(1) as f64;
        let mean = channel.iter().map(|&x| f64::from(x)).sum::<f64>() / n;
        dc += mean * mean;
        total += channel.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / n;
    }
    if total > 0.0 { dc / total } else { 0.0 }
}

fn band_spectrum(audio: &Audio, bands: Bands) -> Option<Spectrum> {
    if bands == Bands::None {
        return None;
    }
    let average = average_spectrum(&audio.channels)?;
    let powers = spectrum::band_powers(&average, audio.sample_rate, bands);
    let nyquist = f64::from(audio.sample_rate) / 2.0;
    let share_of = |p: f64, total: f64| {
        if total > 0.0 {
            round(p / total, 4)
        } else {
            0.0
        }
    };
    let share = |p: f64| share_of(p, powers.total);
    let listed = |edge: &(f64, f64, f64)| edge.1 < nyquist;
    let dc = dc_share(&audio.channels);
    let dc_removed = (dc >= DC_REPORT_SHARE)
        .then(|| {
            let centred: Vec<Vec<f32>> = audio
                .channels
                .iter()
                .map(|c| {
                    let mean = c.iter().map(|&x| f64::from(x)).sum::<f64>() / c.len() as f64;
                    c.iter().map(|&x| (f64::from(x) - mean) as f32).collect()
                })
                .collect();
            let without =
                spectrum::band_powers(&average_spectrum(&centred)?, audio.sample_rate, bands);
            let share = |p: f64| share_of(p, without.total);
            Some(DcRemovedSpectrum {
                dc_share: round(dc, 4),
                centroid_hz: without.centroid_hz.map(|c| round(c, 1)),
                below_first_band_share: share(without.below),
                above_last_band_share: share(without.above),
                shares: without
                    .edges
                    .iter()
                    .zip(&without.powers)
                    .filter(|(edge, _)| listed(edge))
                    .map(|(_, &p)| share(p))
                    .collect(),
            })
        })
        .flatten();
    Some(Spectrum {
        bands,
        fft_size: average.fft_size,
        frames: average.frames,
        resolution_hz: round(f64::from(audio.sample_rate) / average.fft_size as f64, 3),
        centroid_hz: powers.centroid_hz.map(|c| round(c, 1)),
        below_first_band_share: share(powers.below),
        above_last_band_share: share(powers.above),
        levels: powers
            .edges
            .iter()
            .zip(&powers.powers)
            .zip(&powers.bins)
            .filter(|((edge, _), _)| listed(edge))
            .map(|((&(center, low, high), &power), &bins)| Band {
                center_hz: round(center, 1),
                low_hz: round(low, 1),
                high_hz: round(high.min(nyquist), 1),
                level_db: db_power(power),
                share: share(power),
                bins,
            })
            .collect(),
        dc_removed,
    })
}

fn lufs(value: Option<f64>) -> Option<f64> {
    value.map(|v| round(v, 2))
}

pub fn measure(audio: &Audio, options: &MeasureOptions) -> Measurement {
    let channels: Vec<&[f32]> = audio.channels.iter().map(Vec::as_slice).collect();
    let weights = loudness::channel_weights(channels.len());
    let loud = loudness::measure(&channels, &weights, audio.sample_rate);
    let per: Vec<ChannelLevels> = channels
        .iter()
        .map(|c| channel_levels(c, audio.clip_level))
        .collect();
    let frames = audio.frames();
    let values = (frames * channels.len()) as f64;
    let mean = |x: f64| if frames == 0 { 0.0 } else { x / frames as f64 };
    let levels = Levels {
        sample_peak_dbfs: db_amplitude(per.iter().map(|c| c.peak).fold(0.0, f64::max)),
        true_peak_dbtp: db_amplitude(per.iter().map(|c| c.true_peak).fold(0.0, f64::max)),
        rms_dbfs: (values > 0.0)
            .then(|| db_power(per.iter().map(|c| c.square_sum).sum::<f64>() / values))
            .flatten(),
        dc_offset: per.iter().map(|c| round(mean(c.sum), 6)).collect(),
        clipped_samples: per.iter().map(|c| c.clipped).sum(),
        longest_clip_run: per.iter().map(|c| c.longest_run).max().unwrap_or(0),
    };
    let channel_report = options.per_channel.then(|| {
        channels
            .iter()
            .zip(&per)
            .enumerate()
            .map(|(index, (samples, levels))| {
                let alone = loudness::measure(&[samples], &[1.0], audio.sample_rate);
                ChannelMeasurement {
                    channel: index,
                    sample_peak_dbfs: db_amplitude(levels.peak),
                    true_peak_dbtp: db_amplitude(levels.true_peak),
                    rms_dbfs: (frames > 0)
                        .then(|| db_power(levels.square_sum / frames as f64))
                        .flatten(),
                    dc_offset: round(mean(levels.sum), 6),
                    clipped_samples: levels.clipped,
                    integrated_lufs: lufs(alone.integrated),
                    momentary_max_lufs: lufs(alone.momentary_max),
                    short_term_max_lufs: lufs(alone.short_term_max),
                }
            })
            .collect()
    });
    Measurement {
        frames: frames as u64,
        duration_seconds: round(frames as f64 / f64::from(audio.sample_rate), 6),
        levels,
        loudness: LoudnessReport {
            integrated_lufs: lufs(loud.integrated),
            momentary_max_lufs: lufs(loud.momentary_max),
            short_term_max_lufs: lufs(loud.short_term_max),
            loudness_range_lu: lufs(loud.loudness_range),
            relative_gate_lufs: lufs(loud.relative_gate),
            blocks: loud.blocks,
            gated_blocks: loud.gated_blocks,
            short_term_values: loud.short_term_values,
            channel_weights: weights,
        },
        silence: silence(audio, options.silence_threshold_dbfs),
        spectrum: band_spectrum(audio, options.bands),
        channels: channel_report,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LevelDeltas {
    pub integrated_lu: Option<f64>,
    pub momentary_max_lu: Option<f64>,
    pub short_term_max_lu: Option<f64>,
    pub loudness_range_lu: Option<f64>,
    pub sample_peak_db: Option<f64>,
    pub true_peak_db: Option<f64>,
    pub rms_db: Option<f64>,
    pub duration_seconds: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BandDelta {
    pub center_hz: f64,
    pub reference_db: Option<f64>,
    pub candidate_db: Option<f64>,
    pub difference_db: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    /// Candidate minus reference.
    pub deltas: LevelDeltas,
    pub difference: difference::Difference,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub band_deltas: Vec<BandDelta>,
}

fn delta(candidate: Option<f64>, reference: Option<f64>) -> Option<f64> {
    Some(round(candidate? - reference?, 2))
}

/// Compares measurements of two signals and their sample-aligned difference.
pub fn compare(
    reference: (&Audio, &Measurement),
    candidate: (&Audio, &Measurement),
    max_offset: usize,
    start_seconds: f64,
) -> Comparison {
    let (r, c) = (reference.1, candidate.1);
    let band_deltas = match (&r.spectrum, &c.spectrum) {
        (Some(a), Some(b)) => a
            .levels
            .iter()
            .zip(&b.levels)
            .map(|(a, b)| BandDelta {
                center_hz: a.center_hz,
                reference_db: a.level_db,
                candidate_db: b.level_db,
                difference_db: delta(b.level_db, a.level_db),
            })
            .collect(),
        _ => vec![],
    };
    Comparison {
        deltas: LevelDeltas {
            integrated_lu: delta(c.loudness.integrated_lufs, r.loudness.integrated_lufs),
            momentary_max_lu: delta(c.loudness.momentary_max_lufs, r.loudness.momentary_max_lufs),
            short_term_max_lu: delta(
                c.loudness.short_term_max_lufs,
                r.loudness.short_term_max_lufs,
            ),
            loudness_range_lu: delta(c.loudness.loudness_range_lu, r.loudness.loudness_range_lu),
            sample_peak_db: delta(c.levels.sample_peak_dbfs, r.levels.sample_peak_dbfs),
            true_peak_db: delta(c.levels.true_peak_dbtp, r.levels.true_peak_dbtp),
            rms_db: delta(c.levels.rms_dbfs, r.levels.rms_dbfs),
            duration_seconds: round(c.duration_seconds - r.duration_seconds, 6),
        },
        difference: difference::difference(reference.0, candidate.0, max_offset, start_seconds),
        band_deltas,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, amplitude: f64, seconds: f64) -> Vec<f32> {
        (0..(seconds * f64::from(rate)) as usize)
            .map(|i| {
                (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin())
                    as f32
            })
            .collect()
    }

    #[test]
    fn a_minus_20_dbfs_stereo_tone_measures_as_expected() {
        let rate = 48_000;
        let amplitude = 0.1; // -20 dBFS peak
        let mut left = vec![0.0; 4_800]; // 0.1 s lead-in
        left.extend(tone(rate, 997.0, amplitude, 5.0));
        left.extend(vec![0.0; 9_600]); // 0.2 s tail
        let audio = Audio {
            sample_rate: rate,
            channels: vec![left.clone(), left],
            clip_level: 1.0,
        };
        let m = measure(
            &audio,
            &MeasureOptions {
                per_channel: true,
                ..Default::default()
            },
        );
        assert_eq!(m.levels.sample_peak_dbfs, Some(-20.0));
        assert!((m.levels.true_peak_dbtp.unwrap() + 20.0).abs() < 0.01);
        // RMS over the whole range includes the silent ends: 5 / 5.3 of the tone's power.
        let expected_rms = -23.01 + 10.0 * (5.0f64 / 5.3).log10();
        assert!((m.levels.rms_dbfs.unwrap() - expected_rms).abs() < 0.02);
        // Blocks overlapping the silent ends keep 49/50 of the tone's mean power.
        let integrated = -20.0 + 10.0 * 0.98f64.log10();
        assert!((m.loudness.integrated_lufs.unwrap() - integrated).abs() < 0.05);
        assert!((m.loudness.momentary_max_lufs.unwrap() + 20.0).abs() < 0.05);
        assert_eq!(m.levels.clipped_samples, 0);
        assert_eq!(
            (m.silence.leading_seconds, m.silence.all_silent),
            (0.1, false)
        );
        assert!((m.silence.trailing_seconds - 0.2).abs() < 0.001);
        let channels = m.channels.unwrap();
        assert!((channels[0].integrated_lufs.unwrap() - (integrated - 3.01)).abs() < 0.05);
        let spectrum = m.spectrum.unwrap();
        let loudest = spectrum
            .levels
            .iter()
            .max_by(|a, b| a.share.total_cmp(&b.share))
            .unwrap();
        assert_eq!(loudest.center_hz, 1000.0);
        assert!(loudest.share > 0.99);
    }

    #[test]
    fn clipping_dc_and_silence_are_reported() {
        let mut x = vec![0.25f32; 1_000];
        x[100] = 1.0;
        x[101] = -1.0;
        x[102] = 1.0;
        x[500] = 1.0;
        let audio = Audio {
            sample_rate: 8_000,
            channels: vec![x],
            clip_level: 32_767.0 / 32_768.0,
        };
        let m = measure(
            &audio,
            &MeasureOptions {
                bands: Bands::None,
                ..Default::default()
            },
        );
        assert_eq!(
            (m.levels.clipped_samples, m.levels.longest_clip_run),
            (4, 3)
        );
        assert!((m.levels.dc_offset[0] - 0.251).abs() < 1e-6);
        assert!(m.spectrum.is_none());
        // A unipolar square: DC dominates the raw shares; without it the
        // fundamental's band holds most of the power.
        let rate = 32_000;
        let square: Vec<f32> = (0..rate)
            .map(|i| if (i / 40) % 2 == 0 { 0.4 } else { 0.0 })
            .collect();
        let m = measure(
            &Audio {
                sample_rate: rate as u32,
                channels: vec![square],
                clip_level: 1.0,
            },
            &MeasureOptions::default(),
        );
        let spectrum = m.spectrum.unwrap();
        assert!(spectrum.below_first_band_share > 0.45);
        let dc = spectrum.dc_removed.unwrap();
        assert!((dc.dc_share - 0.5).abs() < 0.001);
        assert!(dc.below_first_band_share < 0.001);
        let fundamental = spectrum
            .levels
            .iter()
            .position(|b| b.low_hz <= 400.0 && 400.0 < b.high_hz)
            .unwrap();
        assert!(dc.shares[fundamental] > 0.8, "{}", dc.shares[fundamental]);
        assert_eq!(dc.shares.len(), spectrum.levels.len());
        let silent = Audio {
            sample_rate: 8_000,
            channels: vec![vec![0.0; 800]],
            clip_level: 1.0,
        };
        let m = measure(&silent, &MeasureOptions::default());
        assert!(m.silence.all_silent);
        assert_eq!(m.levels.sample_peak_dbfs, None);
        assert_eq!(m.loudness.integrated_lufs, None);
    }
}
