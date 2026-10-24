//! Transcendental functions built only from IEEE 754 basic operations.
//!
//! Rust's `f64::exp2`, `powf`, `log10` and `sin` call the platform math
//! library, whose last-bit results differ between hosts. The synthesizer uses
//! these replacements instead: they are written with `+ - * /`, `floor`,
//! `round` and bit manipulation, which IEEE 754 defines exactly, and they
//! evaluate in a fixed order. Rust does not contract `a * b + c` into a fused
//! multiply-add unless asked, so the same inputs give the same bits on every
//! host. Accuracy is about 1e-15 relative, far below audibility.

use std::f64::consts::{FRAC_PI_2, LN_2, SQRT_2};

/// The low part of π/2 for two-step argument reduction (Cody and Waite).
const FRAC_PI_2_LOW: f64 = 6.123_233_995_736_766e-17;
/// log2(10).
const LOG2_10: f64 = std::f64::consts::LOG2_10;

/// 2^x.
pub fn exp2(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x >= 1024.0 {
        return f64::INFINITY;
    }
    if x < -1074.0 {
        return 0.0;
    }
    let whole = x.floor();
    // e^(f ln 2) for f in [0, 1) by its Taylor series, evaluated by Horner's
    // rule; the 20th term is below 1e-24.
    let y = (x - whole) * LN_2;
    let mut sum = 1.0;
    for k in (1..=20).rev() {
        sum = 1.0 + sum * y / f64::from(k);
    }
    scale_by_power_of_two(sum, whole as i32)
}

/// `value` × 2^`exponent`, through normal powers of two.
fn scale_by_power_of_two(value: f64, exponent: i32) -> f64 {
    let power = |e: i32| f64::from_bits(((e + 1023) as u64) << 52);
    if exponent >= -1022 {
        value * power(exponent)
    } else {
        value * power(exponent + 100) * power(-100)
    }
}

/// log2(x) for x > 0; -∞ at 0 and NaN below.
pub fn log2(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    let (mut x, mut exponent) = (x, 0i32);
    if x < f64::MIN_POSITIVE {
        x *= f64::from_bits((54 + 1023) << 52);
        exponent -= 54;
    }
    let bits = x.to_bits();
    exponent += ((bits >> 52) & 0x7ff) as i32 - 1023;
    let mut mantissa = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | (1023 << 52));
    if mantissa > SQRT_2 {
        mantissa /= 2.0;
        exponent += 1;
    }
    // ln m = 2 atanh(s), s = (m - 1) / (m + 1), |s| < 0.172.
    let s = (mantissa - 1.0) / (mantissa + 1.0);
    let s2 = s * s;
    let mut series = 0.0;
    for k in (0..14).rev() {
        series = 1.0 / f64::from(2 * k + 1) + s2 * series;
    }
    f64::from(exponent) + 2.0 * s * series / LN_2
}

/// log10(x).
pub fn log10(x: f64) -> f64 {
    log2(x) / LOG2_10
}

/// 10^(x/20): decibels to amplitude.
pub fn db_to_gain(db: f64) -> f64 {
    exp2(db / 20.0 * LOG2_10)
}

/// Amplitude for an attenuation in centibels: 10^(-cB/200).
pub fn centibels_to_gain(centibels: f64) -> f64 {
    exp2(-centibels / 200.0 * LOG2_10)
}

/// (sin x, cos x).
pub fn sin_cos(x: f64) -> (f64, f64) {
    let quadrant = (x / FRAC_PI_2).round();
    let r = (x - quadrant * FRAC_PI_2) - quadrant * FRAC_PI_2_LOW;
    let r2 = r * r;
    // Taylor series on |r| <= π/4; the last terms are below 1e-22.
    let mut sin = 0.0;
    let mut cos = 0.0;
    for k in (0..12).rev() {
        let n = 2 * k;
        sin = 1.0 - sin * r2 / f64::from((n + 2) * (n + 3));
        cos = 1.0 - cos * r2 / f64::from((n + 1) * (n + 2));
    }
    let (sin, cos) = (sin * r, cos);
    match (quadrant as i64).rem_euclid(4) {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions_match_the_platform_library_closely() {
        for i in -400..=400 {
            let x = f64::from(i) * 0.137;
            let close = |a: f64, b: f64| (a - b).abs() <= 4e-15 * b.abs().max(1.0);
            assert!(close(exp2(x), x.exp2()), "exp2 {x}");
            assert!(close(sin_cos(x).0, x.sin()), "sin {x}");
            assert!(close(sin_cos(x).1, x.cos()), "cos {x}");
            let y = exp2(x / 7.0);
            assert!(close(log2(y), y.log2()), "log2 {y}");
        }
        assert_eq!(exp2(0.0), 1.0);
        assert_eq!(exp2(10.0), 1024.0);
        assert_eq!(log2(8.0), 3.0);
        assert_eq!(exp2(-2000.0), 0.0);
        assert!(exp2(-1070.0) > 0.0);
        assert!((log2(1e-310) - 1e-310f64.log2()).abs() < 1e-12);
        assert!((centibels_to_gain(200.0) - 0.1).abs() < 1e-16);
    }
}
