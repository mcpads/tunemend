//! Deterministic Kaiser-windowed sinc resampling for explicit rate changes.
//!
//! Output sample `m` sits at the exact rational source position
//! `m * from / to`. Its value is a finite sum of source samples weighted by a
//! low-pass sinc kernel, cut off at [`CUTOFF`] of the lower Nyquist rate and
//! windowed over [`ZERO_CROSSINGS`] kernel zero crossings per side. Each
//! phase's taps are normalized to unit DC gain. The kernel uses only IEEE basic
//! operations (own sine and Bessel series), so results do not depend on the
//! platform math library. Values are rounded half away from zero to PCM16.
use crate::sample_codec::MAX_SAMPLES;
use serde_json::{Value, json};

pub const METHOD: &str = "kaiser_windowed_sinc";
pub const ZERO_CROSSINGS: f64 = 32.0;
pub const KAISER_BETA: f64 = 9.0;
/// Half-amplitude point. Measured: flat within 0.001 dB to 0.8 of the lower
/// Nyquist, -0.23 dB at 0.85, -6 dB here, at least 98 dB alias rejection from
/// 0.99 on; non-harmonic residual below -100 dB in the passband before rounding.
pub const CUTOFF: f64 = 0.9;
/// Phase tables are cached when the reduced output rate has at most this many phases.
const TABLE_PHASES: u64 = 4096;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// sin(pi * x) from range reduction and a Taylor series; exact zeros at integers.
pub(crate) fn sin_pi(x: f64) -> f64 {
    let mut y = x - 2.0 * (x / 2.0).round();
    if y > 0.5 {
        y = 1.0 - y;
    } else if y < -0.5 {
        y = -1.0 - y;
    }
    let z = std::f64::consts::PI * y;
    let (z2, mut term, mut sum) = (z * z, z, z);
    for k in 1..=12u32 {
        term *= -z2 / ((2 * k) * (2 * k + 1)) as f64;
        sum += term;
    }
    sum
}

pub(crate) fn bessel_i0(x: f64) -> f64 {
    let quarter = x * x / 4.0;
    let (mut term, mut sum) = (1.0, 1.0);
    for k in 1..=500u32 {
        term *= quarter / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-18 {
            break;
        }
    }
    sum
}

struct Kernel {
    /// Reduced source and target rates.
    p: u64,
    q: u64,
    fc: f64,
    half: f64,
    reach: i64,
    i0_beta: f64,
    table: Vec<Option<Vec<f64>>>,
}

impl Kernel {
    fn new(from: u32, to: u32) -> Self {
        let g = gcd(from as u64, to as u64);
        let (p, q) = (from as u64 / g, to as u64 / g);
        let fc = CUTOFF * (q as f64 / p as f64).min(1.0);
        let half = ZERO_CROSSINGS / fc;
        let table = if q <= TABLE_PHASES {
            vec![None; q as usize]
        } else {
            Vec::new()
        };
        Kernel {
            p,
            q,
            fc,
            half,
            reach: half.ceil() as i64,
            i0_beta: bessel_i0(KAISER_BETA),
            table,
        }
    }

    fn weight(&self, x: f64) -> f64 {
        if x.abs() >= self.half {
            return 0.0;
        }
        let u = x / self.half;
        let sinc = if x == 0.0 {
            1.0
        } else {
            sin_pi(self.fc * x) / (std::f64::consts::PI * self.fc * x)
        };
        self.fc * sinc * bessel_i0(KAISER_BETA * (1.0 - u * u).sqrt()) / self.i0_beta
    }

    /// Taps for source offsets `1 - reach ..= reach` around the integer part.
    fn phase(&self, r: u64) -> Vec<f64> {
        self.phase_at(r as f64 / self.q as f64)
    }

    /// Normalized taps for a fractional source position `frac` in `[0, 1]`.
    fn phase_at(&self, frac: f64) -> Vec<f64> {
        let taps: Vec<f64> = (1 - self.reach..=self.reach)
            .map(|k| self.weight(k as f64 - frac))
            .collect();
        let sum: f64 = taps.iter().sum();
        taps.into_iter().map(|w| w / sum).collect()
    }

    fn value(&mut self, m: u64, signal: &impl Fn(i64) -> f64) -> f64 {
        let position = m * self.p;
        let (base, r) = ((position / self.q) as i64, position % self.q);
        let computed;
        let taps = if self.table.is_empty() {
            computed = self.phase(r);
            &computed
        } else {
            if self.table[r as usize].is_none() {
                self.table[r as usize] = Some(self.phase(r));
            }
            self.table[r as usize].as_ref().expect("filled")
        };
        taps.iter()
            .zip(1 - self.reach..)
            .map(|(w, k)| w * signal(base + k))
            .sum()
    }
}

/// Resampled PCM16 values; `clipped` counts values clamped to 16 bits.
#[derive(Debug)]
pub struct Output {
    pub pcm: Vec<i32>,
    pub loop_start: Option<usize>,
    pub clipped: usize,
    pub report: Value,
}

fn round16(values: Vec<f64>) -> (Vec<i32>, usize) {
    let clipped = values
        .iter()
        .filter(|&&v| !(-32768.5..32767.5).contains(&v))
        .count();
    (
        values
            .into_iter()
            .map(|v| v.round().clamp(-32768.0, 32767.0) as i32)
            .collect(),
        clipped,
    )
}

fn report(
    from: u32,
    to: u32,
    source: usize,
    target: usize,
    clipped: usize,
    boundary: &str,
) -> Value {
    json!({"method": METHOD, "source_rate": from, "target_rate": to,
        "source_samples": source, "target_samples": target,
        "zero_crossings_per_side": ZERO_CROSSINGS, "kaiser_beta": KAISER_BETA,
        "cutoff_of_lower_nyquist": CUTOFF, "dc_gain": "normalized per phase",
        "rounding": "half away from zero to PCM16", "clipped_samples": clipped,
        "boundary": boundary})
}

/// Unrounded resampling of a finite stream that is silent outside `pcm`.
pub fn finite_values(pcm: &[f64], from: u32, to: u32) -> Vec<f64> {
    let mut kernel = Kernel::new(from, to);
    let count = (pcm.len() as u64 * kernel.q).div_ceil(kernel.p);
    let signal = |n: i64| {
        usize::try_from(n)
            .ok()
            .and_then(|n| pcm.get(n))
            .copied()
            .unwrap_or(0.0)
    };
    (0..count).map(|m| kernel.value(m, &signal)).collect()
}

/// Source samples a [`stream_values`] call reads for `count` outputs: every
/// position up to the last output plus the kernel reach.
pub fn stream_source_samples(from: u32, to: u32, count: usize) -> usize {
    let kernel = Kernel::new(from, to);
    ((count as u64 * kernel.p).div_ceil(kernel.q) + kernel.reach as u64 + 1) as usize
}

/// Unrounded resampling of long rendered streams, silent outside `channels`,
/// giving exactly `count` outputs per channel. When the reduced target rate
/// has at most 4096 phases, this equals [`finite_values`]. Otherwise each
/// output's taps are interpolated linearly between the two nearest of 4097
/// exactly computed, normalized phases (fractions `j / 4096`), so long renders
/// at rates such as an OPN clock / 144 stay fast and deterministic.
pub fn stream_values(channels: &[&[f64]], from: u32, to: u32, count: usize) -> Vec<Vec<f64>> {
    let mut kernel = Kernel::new(from, to);
    let mut out = vec![Vec::with_capacity(count); channels.len()];
    if !kernel.table.is_empty() {
        for (pcm, out) in channels.iter().zip(&mut out) {
            let signal = |n: i64| {
                usize::try_from(n)
                    .ok()
                    .and_then(|n| pcm.get(n))
                    .copied()
                    .unwrap_or(0.0)
            };
            out.extend((0..count as u64).map(|m| kernel.value(m, &signal)));
        }
        return out;
    }
    let phases = TABLE_PHASES as usize;
    let mut table: Vec<Option<Vec<f64>>> = vec![None; phases + 1];
    let width = (2 * kernel.reach) as usize;
    let mut taps = vec![0.0; width];
    for m in 0..count as u64 {
        let position = m as u128 * kernel.p as u128;
        let (base, r) = (
            (position / kernel.q as u128) as i64,
            (position % kernel.q as u128) as u64,
        );
        let scaled = r as u128 * TABLE_PHASES as u128;
        let j = (scaled / kernel.q as u128) as usize;
        let t = (scaled % kernel.q as u128) as f64 / kernel.q as f64;
        for index in [j, j + 1] {
            if table[index].is_none() {
                table[index] = Some(kernel.phase_at(index as f64 / TABLE_PHASES as f64));
            }
        }
        let (low, high) = (
            table[j].as_ref().expect("filled"),
            table[j + 1].as_ref().expect("filled"),
        );
        for (tap, (a, b)) in taps.iter_mut().zip(low.iter().zip(high)) {
            *tap = a + (b - a) * t;
        }
        for (pcm, out) in channels.iter().zip(&mut out) {
            let value: f64 = taps
                .iter()
                .zip(1 - kernel.reach..)
                .map(|(w, k)| {
                    let n = base + k;
                    let sample = usize::try_from(n)
                        .ok()
                        .and_then(|n| pcm.get(n))
                        .copied()
                        .unwrap_or(0.0);
                    w * sample
                })
                .sum();
            out.push(value);
        }
    }
    out
}

/// A one-shot stream: silence before and after, `ceil(len * to / from)` outputs.
pub fn finite(pcm: &[i32], from: u32, to: u32) -> Result<Output, String> {
    let g = gcd(from as u64, to as u64);
    let count = (pcm.len() as u64 * (to as u64 / g)).div_ceil(from as u64 / g);
    if count as usize > MAX_SAMPLES {
        return Err(format!(
            "resampling to {to} Hz yields {count} samples, above the {MAX_SAMPLES}-sample limit"
        ));
    }
    let source: Vec<f64> = pcm.iter().map(|&v| v as f64).collect();
    let (out, clipped) = round16(finite_values(&source, from, to));
    let report = report(
        from,
        to,
        pcm.len(),
        out.len(),
        clipped,
        "silence outside the finite stream",
    );
    Ok(Output {
        pcm: out,
        loop_start: None,
        clipped,
        report,
    })
}

/// A looping sample: `pcm` is the first pass and `body` the steady-state loop
/// traversal (`pcm.len() - loop_start` samples). Loop start and length must
/// scale to whole target samples, otherwise the call is refused. The attack
/// sees silence before it and the loop after it; the loop region is resampled
/// as its periodic steady state, so every target traversal is identical.
pub fn looped(
    pcm: &[i32],
    body: &[i32],
    loop_start: usize,
    from: u32,
    to: u32,
) -> Result<Output, String> {
    let g = gcd(from as u64, to as u64);
    let (p, q) = (from as u64 / g, to as u64 / g);
    let (len, start) = (pcm.len() as u64, loop_start as u64);
    if !(start * q).is_multiple_of(p) || !(len * q).is_multiple_of(p) {
        return Err(format!(
            "a preserved loop must scale to whole samples: loop start {start} and length {len} at {from} Hz are not whole at {to} Hz (multiples of {p} source samples are). Choose another rate or slice, or loop_policy=finite"
        ));
    }
    let (new_len, new_start) = ((len * q / p) as usize, (start * q / p) as usize);
    if new_len + (new_len - new_start) > MAX_SAMPLES {
        return Err(format!(
            "resampling to {to} Hz yields a {new_len}-sample loop comparison above the {MAX_SAMPLES}-sample limit"
        ));
    }
    let period = body.len() as i64;
    let at = |n: i64| body[(n - start as i64).rem_euclid(period) as usize] as f64;
    let attack = |n: i64| {
        if n < 0 {
            0.0
        } else if (n as u64) < len {
            pcm[n as usize] as f64
        } else {
            body[((n as u64 - len) % period as u64) as usize] as f64
        }
    };
    let mut kernel = Kernel::new(from, to);
    let values: Vec<f64> = (0..new_len as u64)
        .map(|m| {
            if (m as usize) < new_start {
                kernel.value(m, &attack)
            } else {
                kernel.value(m, &at)
            }
        })
        .collect();
    let (out, clipped) = round16(values);
    let mut report = report(
        from,
        to,
        pcm.len(),
        out.len(),
        clipped,
        "silence before the attack; the loop region is resampled as its periodic steady state",
    );
    report["loop"] = json!({"source_start": loop_start, "target_start": new_start,
        "source_length": len - start, "target_length": new_len - new_start});
    Ok(Output {
        pcm: out,
        loop_start: Some(new_start),
        clipped,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gain at `f` and residual (dB re. input amplitude) over a steady window.
    fn response(from: u32, to: u32, f: f64) -> (f64, f64) {
        let n = 6000usize;
        let input: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / from as f64).sin())
            .collect();
        let out = finite_values(&input, from, to);
        let range = out.len() / 4..out.len() * 3 / 4;
        let w = 2.0 * std::f64::consts::PI * f / to as f64;
        let (mut ss, mut cc, mut sc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for m in range.clone() {
            let (s, c) = ((w * m as f64).sin(), (w * m as f64).cos());
            ss += s * s;
            cc += c * c;
            sc += s * c;
            ys += out[m] * s;
            yc += out[m] * c;
        }
        let det = ss * cc - sc * sc;
        let (a, b) = ((ys * cc - yc * sc) / det, (yc * ss - ys * sc) / det);
        let residual: f64 = range
            .clone()
            .map(|m| out[m] - a * (w * m as f64).sin() - b * (w * m as f64).cos())
            .map(|e| e * e)
            .sum::<f64>()
            / range.len() as f64;
        let gain = (a * a + b * b).sqrt();
        (20.0 * gain.log10(), 10.0 * (2.0 * residual).log10())
    }

    fn stopband(from: u32, to: u32, f: f64) -> f64 {
        let n = 6000usize;
        let input: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / from as f64).sin())
            .collect();
        let out = finite_values(&input, from, to);
        let range = out.len() / 4..out.len() * 3 / 4;
        let power = range.clone().map(|m| out[m] * out[m]).sum::<f64>() / range.len() as f64;
        10.0 * (2.0 * power).log10()
    }

    #[test]
    fn sine_and_bessel_series_match_known_values() {
        assert_eq!(sin_pi(3.0), 0.0);
        assert!((sin_pi(0.5) - 1.0).abs() < 1e-15);
        assert!((sin_pi(1.0 / 6.0) - 0.5).abs() < 1e-15);
        assert!((sin_pi(-7.25) - (-7.25f64 * std::f64::consts::PI).sin()).abs() < 1e-14);
        assert!((bessel_i0(9.0) - 1_093.588_354_511_374_5).abs() < 1e-9);
    }

    /// Passband within 0.001 dB to 0.8 of the lower Nyquist with residual below
    /// -100 dB; -6 dB at the 0.9 cutoff; at least 95 dB rejection from 0.99 of
    /// the output Nyquist on when downsampling (all below the source Nyquist).
    #[test]
    fn measured_passband_and_stopband() {
        for (from, to) in [(32000, 16000), (44100, 32000), (16000, 32000)] {
            let nyquist = from.min(to) as f64 / 2.0;
            for fraction in [0.1, 0.5, 0.8] {
                let (gain, residual) = response(from, to, nyquist * fraction);
                assert!(
                    gain.abs() < 0.001,
                    "{from}->{to} {fraction}: gain {gain} dB"
                );
                assert!(
                    residual < -100.0,
                    "{from}->{to} {fraction}: residual {residual} dB"
                );
            }
            let (cutoff, _) = response(from, to, nyquist * CUTOFF);
            assert!(
                (cutoff + 6.02).abs() < 0.01,
                "{from}->{to}: cutoff {cutoff} dB"
            );
            if to < from {
                for fraction in [0.99, 1.01, 1.2, 1.35] {
                    let leak = stopband(from, to, nyquist * fraction);
                    assert!(leak < -95.0, "{from}->{to} {fraction}: alias {leak} dB");
                }
            }
        }
    }

    #[test]
    fn streams_match_exact_phases_or_interpolate_them_closely() {
        let input: Vec<f64> = (0..3000)
            .map(|i| (i as f64 * 0.05).sin() * 10000.0 + (i as f64 * 0.31).cos() * 3000.0)
            .collect();
        // A cached phase table gives exactly finite_values.
        let exact = finite_values(&input, 32000, 44100);
        let streamed = stream_values(&[&input], 32000, 44100, exact.len());
        assert_eq!(streamed[0], exact);
        // An OPN clock / 144 to 44100 Hz has over a million phases.
        let (from, to) = (7_670_454, 144 * 44100);
        let count = 2000;
        let streamed = stream_values(&[&input, &input], from, to, count);
        assert_eq!(streamed[0], streamed[1]);
        let kernel = Kernel::new(from, to);
        assert!(kernel.table.is_empty());
        let signal = |n: i64| {
            usize::try_from(n)
                .ok()
                .and_then(|n| input.get(n))
                .copied()
                .unwrap_or(0.0)
        };
        for m in (0..count as u64).step_by(97) {
            let position = m * kernel.p;
            let (base, r) = ((position / kernel.q) as i64, position % kernel.q);
            let value: f64 = kernel
                .phase(r)
                .iter()
                .zip(1 - kernel.reach..)
                .map(|(w, k)| w * signal(base + k))
                .sum();
            assert!(
                (value - streamed[0][m as usize]).abs() < 1e-3,
                "{m}: {value}"
            );
        }
        assert!(stream_source_samples(from, to, count) > count);
    }

    #[test]
    fn loops_scale_exactly_or_refuse() {
        let pcm: Vec<i32> = (0..96).map(|i| ((i % 32) * 500) - 8000).collect();
        let body = pcm[32..].to_vec();
        let out = looped(&pcm, &body, 32, 32000, 16000).unwrap();
        assert_eq!((out.pcm.len(), out.loop_start), (48, Some(16)));
        // The loop region is periodic: equal 16-sample traversals.
        assert_eq!(out.pcm[16..32], out.pcm[32..48]);
        assert!(
            looped(&pcm, &body, 33, 32000, 16000)
                .unwrap_err()
                .contains("whole samples")
        );
        assert!(looped(&pcm, &body, 32, 32000, 44100).is_err());
        let one_shot = finite(&pcm, 32000, 16000).unwrap();
        assert_eq!(one_shot.pcm.len(), 48);
        assert_eq!(finite(&pcm[..95], 32000, 16000).unwrap().pcm.len(), 48);
    }
}
