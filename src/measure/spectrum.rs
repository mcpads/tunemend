//! Fast Fourier transform and averaged band energies.
//!
//! The spectrum averages periodic-Hann frames of up to 32768 samples at a
//! quarter-frame hop. The signal is padded with silence so that every sample
//! is covered by the same total squared window weight (Hann² sums to a
//! constant 1.5 at that hop), so bin powers sum exactly to the signal's mean
//! square and short or transient sounds are weighted evenly. Bins are then
//! summed into IEC 61260 base-10 octave or third-octave bands.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_FFT: usize = 1 << 15;

/// Radix-2 transform plan of one power-of-two size.
pub struct Fft {
    size: usize,
    cos: Vec<f64>,
    sin: Vec<f64>,
}

impl Fft {
    pub fn new(size: usize) -> Self {
        assert!(size.is_power_of_two());
        let angle = -2.0 * std::f64::consts::PI / size as f64;
        Self {
            size,
            cos: (0..size / 2).map(|k| (angle * k as f64).cos()).collect(),
            sin: (0..size / 2).map(|k| (angle * k as f64).sin()).collect(),
        }
    }

    /// In-place forward transform of `size` values.
    pub fn forward(&self, re: &mut [f64], im: &mut [f64]) {
        let n = self.size;
        debug_assert!(re.len() == n && im.len() == n);
        let mut j = 0;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut length = 2;
        while length <= n {
            let half = length / 2;
            let stride = n / length;
            for start in (0..n).step_by(length) {
                for k in 0..half {
                    let (wr, wi) = (self.cos[k * stride], self.sin[k * stride]);
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            length <<= 1;
        }
    }

    /// In-place inverse transform, scaled by 1/size.
    pub fn inverse(&self, re: &mut [f64], im: &mut [f64]) {
        im.iter_mut().for_each(|x| *x = -*x);
        self.forward(re, im);
        let n = self.size as f64;
        re.iter_mut().for_each(|x| *x /= n);
        im.iter_mut().for_each(|x| *x = -*x / n);
    }
}

/// One-sided power per bin of `fft_size` bins; sums to the mean square.
#[derive(Debug, Clone)]
pub struct PowerSpectrum {
    pub fft_size: usize,
    pub frames: usize,
    pub bins: Vec<f64>,
}

pub fn power_spectrum(samples: &[f32]) -> Option<PowerSpectrum> {
    if samples.len() < 2 {
        return None;
    }
    let size = samples.len().next_power_of_two().clamp(4, MAX_FFT);
    let hop = size / 4;
    let window: Vec<f64> = (0..size)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / size as f64).cos())
        .collect();
    let plan = Fft::new(size);
    let mut bins = vec![0.0; size / 2 + 1];
    let (mut re, mut im) = (vec![0.0; size], vec![0.0; size]);
    let n = samples.len() as isize;
    let mut start = -((size - hop) as isize);
    let mut frames = 0;
    while start < n {
        for (i, (slot, w)) in re.iter_mut().zip(&window).enumerate() {
            let at = start + i as isize;
            *slot = if (0..n).contains(&at) {
                f64::from(samples[at as usize]) * w
            } else {
                0.0
            };
        }
        im.iter_mut().for_each(|x| *x = 0.0);
        plan.forward(&mut re, &mut im);
        for (k, bin) in bins.iter_mut().enumerate() {
            let one_sided = if k == 0 || k == size / 2 { 1.0 } else { 2.0 };
            *bin += one_sided * (re[k] * re[k] + im[k] * im[k]);
        }
        frames += 1;
        start += hop as isize;
    }
    // Σ frames Σ k |X|² / size = 1.5 · Σ x².
    let scale = 1.0 / (size as f64 * 1.5 * samples.len() as f64);
    bins.iter_mut().for_each(|b| *b *= scale);
    Some(PowerSpectrum {
        fft_size: size,
        frames,
        bins,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Bands {
    /// No spectrum.
    None,
    /// Ten octave bands, 31.5 Hz–16 kHz.
    Octave,
    /// Thirty-one third-octave bands, 20 Hz–20 kHz.
    #[default]
    ThirdOctave,
}

/// Exact base-10 mid-band frequency and band edges.
pub fn band_edges(bands: Bands) -> Vec<(f64, f64, f64)> {
    let (range, step) = match bands {
        Bands::None => return vec![],
        Bands::Octave => (-5..=4, 3.0),
        Bands::ThirdOctave => (-17..=13, 1.0),
    };
    range
        .map(|n| {
            let center = 1000.0 * 10f64.powf(f64::from(n) * step / 10.0);
            let edge = 10f64.powf(step / 20.0);
            (center, center / edge, center * edge)
        })
        .collect()
}

/// Mean-square power per band and outside the bands, from a power spectrum.
#[derive(Debug, Clone)]
pub struct BandPowers {
    pub edges: Vec<(f64, f64, f64)>,
    pub powers: Vec<f64>,
    pub bins: Vec<usize>,
    pub below: f64,
    pub above: f64,
    pub total: f64,
    pub centroid_hz: Option<f64>,
}

pub fn band_powers(spectrum: &PowerSpectrum, rate: u32, bands: Bands) -> BandPowers {
    let edges = band_edges(bands);
    let resolution = f64::from(rate) / spectrum.fft_size as f64;
    let mut powers = vec![0.0; edges.len()];
    let mut bins = vec![0; edges.len()];
    let (mut below, mut above, mut total, mut moment) = (0.0, 0.0, 0.0, 0.0);
    let low = edges.first().map_or(f64::INFINITY, |e| e.1);
    let high = edges.last().map_or(0.0, |e| e.2);
    for (k, &power) in spectrum.bins.iter().enumerate() {
        let hz = k as f64 * resolution;
        total += power;
        moment += hz * power;
        if hz < low {
            below += power;
        } else if hz >= high {
            above += power;
        } else if let Some(band) = edges.iter().position(|e| hz >= e.1 && hz < e.2) {
            powers[band] += power;
            bins[band] += 1;
        }
    }
    BandPowers {
        edges,
        powers,
        bins,
        below,
        above,
        total,
        centroid_hz: (total > 0.0).then(|| moment / total),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_matches_a_direct_transform_and_inverts() {
        let x: Vec<f64> = (0..64).map(|i| ((i * i) % 7) as f64 - 3.0).collect();
        let (mut re, mut im) = (x.clone(), vec![0.0; 64]);
        let plan = Fft::new(64);
        plan.forward(&mut re, &mut im);
        for k in 0..64 {
            let (mut dr, mut di) = (0.0, 0.0);
            for (n, v) in x.iter().enumerate() {
                let a = -2.0 * std::f64::consts::PI * (k * n) as f64 / 64.0;
                dr += v * a.cos();
                di += v * a.sin();
            }
            assert!((re[k] - dr).abs() < 1e-9 && (im[k] - di).abs() < 1e-9);
        }
        plan.inverse(&mut re, &mut im);
        for (a, b) in re.iter().zip(&x) {
            assert!((a - b).abs() < 1e-12);
        }
    }

    fn sine(rate: u32, hz: f64, amplitude: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin())
                    as f32
            })
            .collect()
    }

    #[test]
    fn a_tone_lands_in_its_band_with_its_power() {
        let rate = 48_000;
        for (hz, octave_center, third_center) in [
            (1000.0, 1000.0, 1000.0),
            (440.0, 501.2, 398.1),
            (63.0, 63.1, 63.1),
            (9000.0, 7943.3, 10_000.0),
        ] {
            let tone = sine(rate, hz, 0.5, 96_000);
            let spectrum = power_spectrum(&tone).unwrap();
            for (bands, center) in [
                (Bands::Octave, octave_center),
                (Bands::ThirdOctave, third_center),
            ] {
                let powers = band_powers(&spectrum, rate, bands);
                let (index, power) = powers
                    .powers
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .unwrap();
                assert!(
                    (powers.edges[index].0 - center).abs() < 0.1,
                    "{hz} {bands:?}"
                );
                // A sine of amplitude 0.5 has mean square 0.125; leakage stays in the band.
                assert!((power / 0.125 - 1.0).abs() < 0.01, "{hz} {bands:?} {power}");
            }
            let total: f64 = spectrum.bins.iter().sum();
            assert!((total / 0.125 - 1.0).abs() < 0.01);
        }
    }

    #[test]
    fn short_transient_and_noise_signals_keep_their_power() {
        let tone = sine(44_100, 2000.0, 1.0, 3000);
        let spectrum = power_spectrum(&tone).unwrap();
        assert_eq!((spectrum.fft_size, spectrum.frames), (4096, 6));
        let powers = band_powers(&spectrum, 44_100, Bands::Octave);
        assert!((powers.centroid_hz.unwrap() - 2000.0).abs() < 30.0);
        // A burst in the first tenth of a signal: the bins still sum to its mean square.
        let mut burst = vec![0.0f32; 20_000];
        burst[..2_000].copy_from_slice(&sine(44_100, 3000.0, 0.8, 2_000));
        let mean_square = burst.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / 20_000.0;
        let total: f64 = power_spectrum(&burst).unwrap().bins.iter().sum();
        assert!(
            (total / mean_square - 1.0).abs() < 1e-9,
            "{total} {mean_square}"
        );
        // A deterministic white sequence: energy is spread over bands in
        // proportion to their width, so the 8 kHz octave exceeds the 125 Hz one.
        let mut state = 1u32;
        let noise: Vec<f32> = (0..65_536)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        let powers = band_powers(&power_spectrum(&noise).unwrap(), 44_100, Bands::Octave);
        let ratio = powers.powers[8] / powers.powers[2];
        assert!((ratio.log2() - 6.0).abs() < 0.5, "{ratio}");
        assert!(power_spectrum(&[0.5]).is_none());
    }
}
