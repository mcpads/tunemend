//! ITU-R BS.1770-4 loudness and EBU Tech 3342 loudness range.
//!
//! The two K-weighting stages are specified as 48 kHz biquads. Their analog
//! prototypes (shelf at 1681.97 Hz, +4.00 dB; high-pass at 38.14 Hz) are
//! re-derived for the actual rate with the bilinear transform, which
//! reproduces the published 48 kHz coefficients.

/// Second-order section with `a[0] = 1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b: [f64; 3],
    pub a: [f64; 3],
}

/// The pre-filter (head shelf) and RLB high-pass for `rate` Hz.
pub fn k_weighting(rate: f64) -> [Biquad; 2] {
    let shelf = {
        let (f0, gain_db, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
        let k = (std::f64::consts::PI * f0 / rate).tan();
        let vh = 10f64.powf(gain_db / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1.0 + k / q + k * k;
        Biquad {
            b: [
                (vh + vb * k / q + k * k) / a0,
                2.0 * (k * k - vh) / a0,
                (vh - vb * k / q + k * k) / a0,
            ],
            a: [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        }
    };
    let high_pass = {
        let (f0, q) = (38.13547087602444, 0.5003270373238773);
        let k = (std::f64::consts::PI * f0 / rate).tan();
        let a0 = 1.0 + k / q + k * k;
        Biquad {
            b: [1.0, -2.0, 1.0],
            a: [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        }
    };
    [shelf, high_pass]
}

/// BS.1770 channel weights: 1.0, except 1.41 for the surround pair and 0 for
/// LFE in six-channel (5.1, WAVE order L R C LFE Ls Rs) input.
pub fn channel_weights(channels: usize) -> Vec<f64> {
    if channels == 6 {
        vec![1.0, 1.0, 1.0, 0.0, 1.41, 1.41]
    } else {
        vec![1.0; channels]
    }
}

/// Start of each 100 ms step, `round(j · rate / 10)`, up to the signal length.
fn step_boundaries(frames: usize, rate: u32) -> Vec<usize> {
    let mut out = vec![0];
    loop {
        let next = ((out.len() as f64) * f64::from(rate) / 10.0).round() as usize;
        if next > frames {
            return out;
        }
        out.push(next);
    }
}

/// K-weighted energy per 100 ms step, summed over weighted channels.
fn step_energies(channels: &[&[f32]], weights: &[f64], rate: u32) -> (Vec<usize>, Vec<f64>) {
    let frames = channels.first().map_or(0, |c| c.len());
    let bounds = step_boundaries(frames, rate);
    let mut energy = vec![0.0; bounds.len().saturating_sub(1)];
    let filters = k_weighting(f64::from(rate));
    for (samples, &weight) in channels.iter().zip(weights) {
        if weight == 0.0 {
            continue;
        }
        let mut state = [[0.0f64; 2]; 2];
        let mut step = 0;
        for (i, &x) in samples.iter().enumerate() {
            // Transposed direct form II, both stages.
            let mut y = f64::from(x);
            for (filter, s) in filters.iter().zip(state.iter_mut()) {
                let out = filter.b[0] * y + s[0];
                s[0] = filter.b[1] * y - filter.a[1] * out + s[1];
                s[1] = filter.b[2] * y - filter.a[2] * out;
                y = out;
            }
            while step < energy.len() && i >= bounds[step + 1] {
                step += 1;
            }
            if step < energy.len() {
                energy[step] += weight * y * y;
            }
        }
    }
    (bounds, energy)
}

/// Loudness in LUFS of a mean-square weighted power, or `None` for silence.
pub fn lufs(power: f64) -> Option<f64> {
    (power > 0.0).then(|| -0.691 + 10.0 * power.log10())
}

/// Mean power of every complete window of `steps` consecutive 100 ms steps.
fn windows(bounds: &[usize], energy: &[f64], steps: usize) -> Vec<f64> {
    if energy.len() < steps {
        return vec![];
    }
    let mut out = Vec::with_capacity(energy.len() - steps + 1);
    let mut sum: f64 = energy[..steps].iter().sum();
    for j in 0..=energy.len() - steps {
        if j > 0 {
            sum += energy[j + steps - 1] - energy[j - 1];
        }
        let length = (bounds[j + steps] - bounds[j]) as f64;
        out.push(sum.max(0.0) / length);
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Loudness {
    pub integrated: Option<f64>,
    /// Relative gate applied to the integrated measurement.
    pub relative_gate: Option<f64>,
    pub blocks: usize,
    pub gated_blocks: usize,
    pub momentary_max: Option<f64>,
    pub short_term_max: Option<f64>,
    pub loudness_range: Option<f64>,
    pub short_term_values: usize,
}

const ABSOLUTE_GATE: f64 = -70.0;

fn gated_mean(powers: &[f64], relative_lu: f64) -> (Option<f64>, Option<f64>, Vec<f64>) {
    let absolute: Vec<f64> = powers
        .iter()
        .copied()
        .filter(|&p| lufs(p).is_some_and(|l| l > ABSOLUTE_GATE))
        .collect();
    if absolute.is_empty() {
        return (None, None, vec![]);
    }
    let mean = absolute.iter().sum::<f64>() / absolute.len() as f64;
    let gate = lufs(mean).expect("positive") + relative_lu;
    let kept: Vec<f64> = absolute
        .into_iter()
        .filter(|&p| lufs(p).is_some_and(|l| l > gate))
        .collect();
    let integrated = (!kept.is_empty())
        .then(|| lufs(kept.iter().sum::<f64>() / kept.len() as f64))
        .flatten();
    (integrated, Some(gate), kept)
}

/// EBU Tech 3342 loudness range of short-term powers, as its reference
/// implementation computes it: keep levels at or above -70 LUFS, then at or
/// above 20 LU below the power mean of those, and subtract the 10th from the
/// 95th percentile of the sorted levels, each taken at 0-based rank
/// `round((n - 1) × p)`. BS.1770's integrated gates are strict instead.
fn loudness_range(short: &[f64]) -> Option<f64> {
    let absolute: Vec<f64> = short
        .iter()
        .copied()
        .filter(|&p| lufs(p).is_some_and(|l| l >= ABSOLUTE_GATE))
        .collect();
    if absolute.is_empty() {
        return None;
    }
    let mean = absolute.iter().sum::<f64>() / absolute.len() as f64;
    let gate = lufs(mean).expect("positive") - 20.0;
    let mut levels: Vec<f64> = absolute
        .into_iter()
        .filter_map(lufs)
        .filter(|&l| l >= gate)
        .collect();
    if levels.is_empty() {
        return None;
    }
    levels.sort_by(f64::total_cmp);
    let rank = |p: f64| levels[((levels.len() - 1) as f64 * p).round() as usize];
    Some(rank(0.95) - rank(0.10))
}

/// Integrated loudness (400 ms blocks, 75% overlap, -70 LUFS absolute and
/// -10 LU relative gates), momentary (400 ms) and short-term (3 s) maxima at a
/// 100 ms step, and loudness range from the 3 s values (see
/// [`loudness_range`]).
pub fn measure(channels: &[&[f32]], weights: &[f64], rate: u32) -> Loudness {
    let (bounds, energy) = step_energies(channels, weights, rate);
    let blocks = windows(&bounds, &energy, 4);
    let (integrated, relative_gate, kept) = gated_mean(&blocks, -10.0);
    let short = windows(&bounds, &energy, 30);
    let max = |v: &[f64]| {
        v.iter()
            .copied()
            .fold(None, |m: Option<f64>, p| Some(m.map_or(p, |m| m.max(p))))
    };
    let loudness_range = loudness_range(&short);
    Loudness {
        integrated,
        relative_gate,
        blocks: blocks.len(),
        gated_blocks: kept.len(),
        momentary_max: max(&blocks).and_then(lufs),
        short_term_max: max(&short).and_then(lufs),
        loudness_range,
        short_term_values: short.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, dbfs: f64, seconds: f64) -> Vec<f32> {
        let amplitude = 10f64.powf(dbfs / 20.0);
        (0..(seconds * f64::from(rate)).round() as usize)
            .map(|i| {
                (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin())
                    as f32
            })
            .collect()
    }

    fn stereo(parts: &[(f64, f64)], rate: u32) -> Vec<f32> {
        parts
            .iter()
            .flat_map(|&(db, s)| sine(rate, 1000.0, db, s))
            .collect()
    }

    fn integrated_stereo(signal: &[f32], rate: u32) -> Loudness {
        measure(&[signal, signal], &[1.0, 1.0], rate)
    }

    #[test]
    fn coefficients_match_the_published_48_khz_filters() {
        let [shelf, high_pass] = k_weighting(48_000.0);
        let published = [
            (
                shelf.b,
                [1.53512485958697, -2.69169618940638, 1.19839281085285],
            ),
            (shelf.a, [1.0, -1.69065929318241, 0.73248077421585]),
            (high_pass.b, [1.0, -2.0, 1.0]),
            (high_pass.a, [1.0, -1.99004745483398, 0.99007225036621]),
        ];
        for (actual, expected) in published {
            for (a, e) in actual.iter().zip(expected) {
                assert!((a - e).abs() < 1e-8, "{actual:?} vs {expected:?}");
            }
        }
    }

    #[test]
    fn one_channel_full_scale_1_khz_reads_minus_3_01_at_common_rates() {
        // The bilinear re-derivation warps the shelf slightly at low rates.
        for (rate, tolerance) in [
            (32_000, 0.03),
            (44_100, 0.02),
            (48_000, 0.01),
            (96_000, 0.02),
        ] {
            let tone = sine(rate, 1000.0, 0.0, 3.0);
            let level = measure(&[&tone], &[1.0], rate).integrated.unwrap();
            assert!((level + 3.01).abs() < tolerance, "{rate}: {level}");
        }
    }

    #[test]
    fn tech_3341_stationary_and_gated_cases_read_minus_23() {
        let rate = 48_000;
        // Cases 1 and 2: stereo 1 kHz at -23 and -33 dBFS.
        for (db, expected) in [(-23.0, -23.0), (-33.0, -33.0)] {
            let l = integrated_stereo(&stereo(&[(db, 20.0)], rate), rate);
            for value in [l.integrated, l.momentary_max, l.short_term_max] {
                assert!((value.unwrap() - expected).abs() < 0.1, "{l:?}");
            }
        }
        // Case 3: quieter parts fall below the relative gate.
        let l = integrated_stereo(
            &stereo(&[(-36.0, 10.0), (-23.0, 60.0), (-36.0, 10.0)], rate),
            rate,
        );
        assert!((l.integrated.unwrap() + 23.0).abs() < 0.1, "{l:?}");
        // Case 5: all three parts pass the relative gate and average to -23.
        let l = integrated_stereo(
            &stereo(&[(-26.0, 20.0), (-20.0, 20.1), (-26.0, 20.0)], rate),
            rate,
        );
        assert!((l.integrated.unwrap() + 23.0).abs() < 0.1, "{l:?}");
    }

    #[test]
    fn absolute_gate_excludes_blocks_below_minus_70() {
        let rate = 8_000;
        // Case 4 shortened: -72 dBFS segments only pass if the absolute gate is missing.
        let signal = stereo(
            &[
                (-72.0, 10.0),
                (-36.0, 10.0),
                (-23.0, 20.0),
                (-36.0, 10.0),
                (-72.0, 10.0),
            ],
            rate,
        );
        let l = integrated_stereo(&signal, rate);
        assert!((l.integrated.unwrap() + 23.0).abs() < 0.15, "{l:?}");
        let quiet = stereo(&[(-75.0, 5.0)], rate);
        let l = integrated_stereo(&quiet, rate);
        assert_eq!((l.integrated, l.gated_blocks), (None, 0));
        assert!(l.momentary_max.unwrap() < -70.0);
        let silent = vec![0.0f32; 8_000];
        let l = integrated_stereo(&silent, rate);
        assert_eq!((l.integrated, l.momentary_max), (None, None));
    }

    #[test]
    fn short_term_follows_alternating_levels_and_range_spans_steps() {
        // Tech 3341 case 9 pattern: 1.34 s at -20 and 1.66 s at -30 dBFS keep S at -23.
        let pattern: Vec<(f64, f64)> = (0..5)
            .flat_map(|_| [(-20.0, 1.34), (-30.0, 1.66)])
            .collect();
        let l = integrated_stereo(&stereo(&pattern, 48_000), 48_000);
        assert!((l.short_term_max.unwrap() + 23.0).abs() < 0.1, "{l:?}");
        let rate = 8_000;
        // Tech 3342 case 1 pattern: 20 s at -20 and 20 s at -30 dBFS gives LRA 10 LU.
        let l = integrated_stereo(&stereo(&[(-20.0, 20.0), (-30.0, 20.0)], rate), rate);
        assert!((l.loudness_range.unwrap() - 10.0).abs() < 0.1, "{l:?}");
        // A constant tone has no range; signals under 3 s have no short-term value.
        let l = integrated_stereo(&stereo(&[(-20.0, 10.0)], rate), rate);
        assert!(l.loudness_range.unwrap().abs() < 0.01);
        let l = integrated_stereo(&stereo(&[(-20.0, 2.0)], rate), rate);
        assert_eq!(
            (l.short_term_max, l.loudness_range, l.short_term_values),
            (None, None, 0)
        );
    }

    /// Power whose loudness is `level` LUFS.
    fn power(level: f64) -> f64 {
        10f64.powf((level + 0.691) / 10.0)
    }

    #[test]
    fn loudness_range_takes_tech_3342_ranks() {
        // 21 levels one LU apart, all above the relative gate. Tech 3342's
        // reference takes ranks round(20 x 0.10) = 2 and round(20 x 0.95) = 19.
        let short: Vec<f64> = (0..21).map(|i| power(-40.0 + f64::from(i))).collect();
        let range = loudness_range(&short).unwrap();
        assert!((range - 17.0).abs() < 1e-9, "{range}");
    }

    #[test]
    fn loudness_range_gates_include_their_thresholds() {
        // Tech 3342's reference keeps a level equal to the -70 LUFS gate.
        let near = power(ABSOLUTE_GATE).to_bits();
        let at_gate = (near - 1000..near + 1000)
            .map(f64::from_bits)
            .find(|&p| lufs(p) == Some(ABSOLUTE_GATE))
            .expect("a power reading exactly -70 LUFS");
        assert_eq!(loudness_range(&[at_gate; 40]), Some(0.0));
        assert_eq!(loudness_range(&[at_gate * 0.999; 40]), None);
    }

    #[test]
    fn surround_weights_follow_bs_1770() {
        let rate = 8_000;
        let tone = sine(rate, 1000.0, -20.0, 2.0);
        let silent = vec![0.0f32; tone.len()];
        let weights = channel_weights(6);
        let front = measure(
            &[&tone, &silent, &silent, &silent, &silent, &silent],
            &weights,
            rate,
        );
        let surround = measure(
            &[&silent, &silent, &silent, &silent, &tone, &silent],
            &weights,
            rate,
        );
        let lfe = measure(
            &[&silent, &silent, &silent, &tone, &silent, &silent],
            &weights,
            rate,
        );
        let gain = surround.integrated.unwrap() - front.integrated.unwrap();
        assert!((gain - 10.0 * 1.41f64.log10()).abs() < 0.01);
        assert_eq!(lfe.integrated, None);
    }
}
