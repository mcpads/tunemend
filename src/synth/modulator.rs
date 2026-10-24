//! SoundFont modulators (SF2.04 §8.2–8.4, §9.5): sources mapped to −1…1,
//! multiplied by an amount and an amount source, transformed and summed into
//! a generator.

use super::math::log10;

/// Number of generator slots, operators 0–60 (SF2.04 §8.1.2).
pub const GENERATORS: usize = 61;
/// Internal destination for the pitch wheel's "initial pitch" summing node,
/// which has no generator number; files cannot name it.
pub const INITIAL_PITCH: u16 = 0x3fff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modulator {
    pub source: u16,
    pub destination: u16,
    pub amount: i16,
    pub amount_source: u16,
    pub transform: u16,
}

impl Modulator {
    /// §9.5.1: modulators are identical when source, destination, amount
    /// source and transform match.
    pub fn same_as(&self, other: &Modulator) -> bool {
        (
            self.source,
            self.destination,
            self.amount_source,
            self.transform,
        ) == (
            other.source,
            other.destination,
            other.amount_source,
            other.transform,
        )
    }

    /// Whether a modulator read from a file is usable. Unknown or illegal
    /// sources, link sources and destinations, non-value destinations and
    /// unknown transforms are ignored (§7.4, §8.2.1, §8.2.4, §8.3). Linked
    /// modulators are an explicit omission of this synthesizer.
    pub fn is_supported(&self) -> bool {
        valid_source(self.source)
            && valid_source(self.amount_source)
            && self.destination < GENERATORS as u16
            && is_value_generator(self.destination as usize)
            && matches!(self.transform, 0 | 2)
    }

    /// Transform(amount × source × amount source), in destination units.
    pub fn value(&self, inputs: &SourceInputs) -> f64 {
        let primary = map_source(self.source, inputs);
        let secondary = map_source(self.amount_source, inputs);
        let product = f64::from(self.amount) * primary * secondary;
        if self.transform == 2 {
            product.abs()
        } else {
            product
        }
    }
}

/// Generators a modulator may target and a preset zone may offset (§8.1.1,
/// §8.5): everything except index, range, substitution and sample
/// generators and the unused slots.
pub fn is_value_generator(operator: usize) -> bool {
    !matches!(
        operator,
        0..=4
            | 12
            | 14
            | 18..=20
            | 41..=47
            | 49
            | 50
            | 53..=55
            | 57..=60
    )
}

fn valid_source(source: u16) -> bool {
    let index = source & 0x7f;
    if source >> 10 > 3 {
        return false;
    }
    if source & 0x80 != 0 {
        return !matches!(index, 0 | 6 | 32..=63 | 98..=101 | 120..=127);
    }
    match index {
        0 | 2 | 3 | 10 | 13 | 14 | 16 => true,
        // 127 is a link (omitted); other indices are reserved.
        _ => false,
    }
}

/// The note and channel values modulator sources read.
#[derive(Debug, Clone, Copy)]
pub struct SourceInputs<'a> {
    pub velocity: u8,
    pub key: u8,
    pub controllers: &'a [u8; 128],
    pub channel_pressure: u8,
    /// 14-bit pitch wheel, 8192 at rest.
    pub pitch_wheel: u16,
    /// RPN 0 range in semitones (cents as the fraction).
    pub pitch_wheel_semitones: f64,
}

/// Maps a source to −1…1 (§8.2, §9.5.3). Linear 7-bit sources use value/128
/// and 14-bit sources value/16384 as Table 2 gives; concave, convex and
/// switch curves use value/127 (value/16383) so they reach both ends. The
/// pitch wheel sensitivity source is the RPN 0 range in semitones divided by
/// 127, so the default modulator's 12700-cent amount yields exactly the
/// requested range rather than 127/128 of it.
pub fn map_source(source: u16, inputs: &SourceInputs) -> f64 {
    let index = source & 0x7f;
    let curve = source >> 10;
    let negative = source & 0x100 != 0;
    let bipolar = source & 0x200 != 0;
    let (value, steps) = if source & 0x80 != 0 {
        (u32::from(inputs.controllers[usize::from(index)]), 128u32)
    } else {
        match index {
            0 => return 1.0,
            2 => (u32::from(inputs.velocity), 128),
            3 => (u32::from(inputs.key), 128),
            13 => (u32::from(inputs.channel_pressure), 128),
            14 => (u32::from(inputs.pitch_wheel), 16384),
            16 => {
                let u = inputs.pitch_wheel_semitones / 127.0;
                return shape(if negative { 1.0 - u } else { u }, 0, bipolar);
            }
            // Poly pressure is not received (the renderer refuses it).
            _ => (0, 128),
        }
    };
    let u = if curve == 0 {
        let value = if negative { steps - 1 - value } else { value };
        f64::from(value) / f64::from(steps)
    } else {
        let u = f64::from(value) / f64::from(steps - 1);
        if negative { 1.0 - u } else { u }
    };
    shape(u, curve, bipolar)
}

fn shape(u: f64, curve: u16, bipolar: bool) -> f64 {
    let unipolar = |x: f64| match curve {
        1 => concave(x),
        2 => convex(x),
        3 => {
            if x >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        _ => x,
    };
    if !bipolar {
        return unipolar(u);
    }
    match curve {
        0 => 2.0 * u - 1.0,
        3 => {
            if u >= 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        _ => {
            if u >= 0.5 {
                unipolar(2.0 * u - 1.0)
            } else {
                -unipolar(1.0 - 2.0 * u)
            }
        }
    }
}

/// The concave curve: −(20/96)·log10((1 − x)²), clamped to 0…1. With the
/// default 960 cB amount on a negative source, attenuation for a controller
/// value v is 400·log10(127/v) cB, i.e. gain (v/127)².
pub fn concave(x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    (-(40.0 / 96.0) * log10(1.0 - x)).min(1.0)
}

/// The convex curve: the concave curve with its end points reversed.
pub fn convex(x: f64) -> f64 {
    1.0 - concave(1.0 - x)
}

/// SF2.04 §8.4 default instrument-level modulators. One deliberate
/// deviation: CC10 → pan uses 500 tenths of a percent, not the text's 1000,
/// so CC10 0…127 spans hard left to (nearly) hard right like General MIDI
/// instead of saturating at CC10 96. The text's CC7 source enumeration
/// 0x0582 contradicts its own field breakdown (index 7); 0x0587 is used. The
/// pitch wheel modulator's destination is the internal [`INITIAL_PITCH`]
/// node.
pub const DEFAULT_MODULATORS: [Modulator; 10] = [
    modulator(0x0502, 48, 960, 0),
    modulator(0x0102, 8, -2400, 0),
    modulator(0x000d, 6, 50, 0),
    modulator(0x0081, 6, 50, 0),
    modulator(0x0587, 48, 960, 0),
    modulator(0x028a, 17, 500, 0),
    modulator(0x058b, 48, 960, 0),
    modulator(0x00db, 16, 200, 0),
    modulator(0x00dd, 15, 200, 0),
    modulator(0x020e, INITIAL_PITCH, 12700, 0x0010),
];

const fn modulator(source: u16, destination: u16, amount: i16, amount_source: u16) -> Modulator {
    Modulator {
        source,
        destination,
        amount,
        amount_source,
        transform: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(controllers: &[u8; 128]) -> SourceInputs<'_> {
        SourceInputs {
            velocity: 100,
            key: 60,
            controllers,
            channel_pressure: 0,
            pitch_wheel: 8192,
            pitch_wheel_semitones: 2.0,
        }
    }

    #[test]
    fn curves_follow_the_specification_shapes() {
        assert_eq!(concave(0.0), 0.0);
        assert_eq!(concave(1.0), 1.0);
        assert!(concave(0.5) < 0.5 && convex(0.5) > 0.5);
        assert!((concave(0.5) + convex(0.5) - 1.0).abs() < 1e-15);
        let mut c = [0u8; 128];
        // CC7 at 64: the default modulator attenuates 400·log10(127/64) cB.
        c[7] = 64;
        let cc7 = DEFAULT_MODULATORS[4].value(&inputs(&c));
        assert!(
            (cc7 - 400.0 * (127.0f64 / 64.0).log10()).abs() < 1e-9,
            "{cc7}"
        );
        c[7] = 127;
        assert_eq!(DEFAULT_MODULATORS[4].value(&inputs(&c)), 0.0);
        // Velocity 100 lowers the filter by 2400 × 27/128 cents (linear).
        let fc = DEFAULT_MODULATORS[1].value(&inputs(&c));
        assert!((fc + 2400.0 * 27.0 / 128.0).abs() < 1e-9);
        // Bipolar pan: CC10 0 is −500, 64 centre.
        c[10] = 0;
        assert_eq!(DEFAULT_MODULATORS[5].value(&inputs(&c)), -500.0);
        c[10] = 64;
        assert_eq!(DEFAULT_MODULATORS[5].value(&inputs(&c)), 0.0);
        // Switch and negative bipolar concave.
        let switch = Modulator {
            source: 0x0c81,
            ..DEFAULT_MODULATORS[3]
        };
        c[1] = 63;
        assert_eq!(switch.value(&inputs(&c)), 0.0);
        c[1] = 64;
        assert_eq!(switch.value(&inputs(&c)), 50.0);
        let mut i = inputs(&c);
        i.pitch_wheel = 16383;
        let bend = DEFAULT_MODULATORS[9].value(&i);
        assert!((bend - 200.0 * 8191.0 / 8192.0).abs() < 1e-9, "{bend}");
        i.pitch_wheel = 0;
        assert!((DEFAULT_MODULATORS[9].value(&i) + 200.0).abs() < 1e-9);
    }

    #[test]
    fn unsupported_modulators_are_recognized() {
        let ok = DEFAULT_MODULATORS[3];
        assert!(
            DEFAULT_MODULATORS
                .iter()
                .take(9)
                .all(Modulator::is_supported)
        );
        assert!(
            !Modulator {
                source: 0x0080 | 6,
                ..ok
            }
            .is_supported(),
            "data entry"
        );
        assert!(
            !Modulator {
                destination: 53,
                ..ok
            }
            .is_supported(),
            "sampleID"
        );
        assert!(
            !Modulator {
                destination: 0x8001,
                ..ok
            }
            .is_supported(),
            "link"
        );
        assert!(!Modulator { transform: 1, ..ok }.is_supported());
        assert!(!Modulator { source: 127, ..ok }.is_supported());
        assert!(
            !Modulator {
                source: 4 << 10 | 2,
                ..ok
            }
            .is_supported()
        );
    }
}
