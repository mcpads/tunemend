//! Sample and true peak.
//!
//! True peak follows the BS.1770-4 Annex 2 method: oversample 4× and take the
//! largest absolute value. The interpolator is a Kaiser-windowed sinc
//! (β = 8, spanning ±16 input samples, 32 taps per fractional phase, each
//! phase normalized to unity DC gain) instead of the Annex's 48-tap example
//! filter. Phase 0 is the input itself, so true peak never reads below sample
//! peak. The passband is flat within 0.01 dB up to 0.417 of the sample rate
//! (20 kHz at 48 kHz); like any 4× method it can under-read a sine at
//! frequency f by up to 1 − cos(π f / 4 fs).

/// Input samples on each side of an interpolated point.
const HALF: usize = 16;
const TAPS: usize = 2 * HALF;
const CHUNK: usize = 1 << 16;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-15 * sum {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Taps of the phases 1/4, 2/4 and 3/4: `y(m + p/4) = Σ_j taps[p-1][j] · x[m + HALF − j]`.
pub fn interpolator() -> [[f64; TAPS]; 3] {
    let beta = 8.0;
    let mut phases = [[0.0; TAPS]; 3];
    for (index, taps) in phases.iter_mut().enumerate() {
        for (j, tap) in taps.iter_mut().enumerate() {
            // Distance from the output point to x[m + HALF − j], in quarter samples.
            let n = 4.0 * (j as f64 - HALF as f64) + (index + 1) as f64;
            let t = std::f64::consts::PI * n / 4.0;
            let r = n / (4 * HALF) as f64;
            let window = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta);
            *tap = t.sin() / t * window;
        }
        let sum: f64 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
    }
    phases
}

/// Largest absolute sample value.
pub fn sample_peak(samples: &[f32]) -> f64 {
    samples
        .iter()
        .fold(0.0f64, |m, &x| m.max(f64::from(x.abs())))
}

/// Largest absolute value of the 4× oversampled signal, treating the signal
/// as silent outside the given samples (so edge ringing counts).
pub fn true_peak(samples: &[f32]) -> f64 {
    let phases = interpolator();
    let n = samples.len() as isize;
    let at = |i: isize| {
        if (0..n).contains(&i) {
            f64::from(samples[i as usize])
        } else {
            0.0
        }
    };
    let mut peak = sample_peak(samples);
    // Interpolated points m + p/4 for m in −HALF .. n + HALF − 1 can be non-zero.
    let first = -(HALF as isize);
    let last = n + HALF as isize;
    let mut buffer = Vec::with_capacity(CHUNK + TAPS);
    let mut m0 = first;
    while m0 < last {
        let count = (last - m0).min(CHUNK as isize);
        // buffer[i] = x[m0 − HALF + 1 + i]
        buffer.clear();
        buffer.extend((0..count + TAPS as isize - 1).map(|i| at(m0 - HALF as isize + 1 + i)));
        for offset in 0..count as usize {
            // x[m + HALF − j] = buffer[offset + TAPS − 1 − j]
            let input = &buffer[offset..offset + TAPS];
            for taps in &phases {
                let y: f64 = taps.iter().rev().zip(input).map(|(t, x)| t * x).sum();
                peak = peak.max(y.abs());
            }
        }
        m0 += count;
    }
    peak
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine with 10 ms raised-cosine fades, so edge ringing stays below the tone's peak.
    fn tone(rate: f64, hz: f64, amplitude: f64, phase_degrees: f64, frames: usize) -> Vec<f32> {
        let phase = phase_degrees.to_radians();
        let fade = (rate / 100.0) as usize;
        (0..frames)
            .map(|i| {
                let edge = i.min(frames - 1 - i);
                let gain = if edge < fade {
                    0.5 - 0.5 * (std::f64::consts::PI * edge as f64 / fade as f64).cos()
                } else {
                    1.0
                };
                (gain
                    * amplitude
                    * (2.0 * std::f64::consts::PI * hz * i as f64 / rate + phase).sin())
                    as f32
            })
            .collect()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    #[test]
    fn interpolator_reproduces_a_band_limited_sine_between_samples() {
        let phases = interpolator();
        for hz in [1000.0, 15_000.0] {
            // Outside the fades, the tone is a plain sine.
            let x = tone(48_000.0, hz, 0.5, 10.0, 4_000);
            for m in 1_000..1_400 {
                for (index, taps) in phases.iter().enumerate() {
                    let y: f64 = (0..TAPS)
                        .map(|j| taps[j] * f64::from(x[m + HALF - j]))
                        .sum();
                    let t = m as f64 + (index + 1) as f64 / 4.0;
                    let expected = 0.5
                        * (2.0 * std::f64::consts::PI * hz * t / 48_000.0 + 10f64.to_radians())
                            .sin();
                    assert!(
                        (y - expected).abs() < 5e-4,
                        "{hz} {m} {index}: {y} {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn inter_sample_peaks_are_found() {
        // -6 dBFS sines at fs/4, fs/6 and fs/8 with phases that put the
        // continuous peak between samples (as in EBU Tech 3341's true-peak
        // cases) read -6 dBTP within +0.2/-0.4 dB.
        let rate = 48_000.0;
        for (hz, phase) in [
            (12_000.0, 0.0),
            (12_000.0, 45.0),
            (8_000.0, 60.0),
            (6_000.0, 67.5),
        ] {
            let x = tone(rate, hz, 0.5, phase, 4_800);
            let tp = db(true_peak(&x));
            assert!((-6.42..=-5.82).contains(&tp), "{hz} Hz {phase}°: {tp}");
        }
        // Samples of an fs/4 sine at 45° phase are ±0.707 of its amplitude:
        // scaled to a 0 dBFS sample peak, the true peak is +3.01 dBTP.
        let x = tone(rate, 12_000.0, std::f64::consts::SQRT_2, 45.0, 4_800);
        assert!(db(sample_peak(&x)).abs() < 0.001);
        let tp = db(true_peak(&x));
        assert!((tp - 3.01).abs() < 0.05, "{tp}");
    }

    #[test]
    fn true_peak_matches_sample_peak_for_low_tones_and_impulses() {
        let x = tone(44_100.0, 100.0, 0.25, 0.0, 44_100);
        assert!((db(true_peak(&x)) - db(0.25)).abs() < 0.01);
        let mut impulse = vec![0.0f32; 64];
        impulse[30] = -0.5;
        assert!((true_peak(&impulse) - 0.5).abs() < 1e-12);
        assert_eq!(true_peak(&[]), 0.0);
    }
}
