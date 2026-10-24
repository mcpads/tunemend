//! The synthesizer's chorus: a mono send read back by two delay taps whose
//! delay swings with a triangle LFO, the right tap in opposite phase.
//!
//! No feedback, so its output ends `CENTER_SECONDS + DEPTH_SECONDS` after
//! its input. The LFO is a phase accumulator, and the taps use linear
//! interpolation, so the output is reproducible from basic arithmetic.

/// Centre delay of both taps.
pub const CENTER_SECONDS: f64 = 0.015;
/// Delay swing either side of the centre.
pub const DEPTH_SECONDS: f64 = 0.0025;
/// LFO rate.
pub const RATE_HZ: f64 = 0.6;

#[derive(Debug, Clone)]
pub struct Chorus {
    line: Vec<f64>,
    write: usize,
    phase: f64,
    step: f64,
    center: f64,
    depth: f64,
}

impl Chorus {
    pub fn new(rate: f64) -> Self {
        let length = ((CENTER_SECONDS + DEPTH_SECONDS) * rate).ceil() as usize + 3;
        Self {
            line: vec![0.0; length],
            write: 0,
            phase: 0.0,
            step: RATE_HZ / rate,
            center: CENTER_SECONDS * rate,
            depth: DEPTH_SECONDS * rate,
        }
    }

    fn tap(&self, delay: f64) -> f64 {
        let whole = delay.floor();
        let fraction = delay - whole;
        let length = self.line.len();
        let at = |d: usize| self.line[(self.write + length - d) % length];
        let d = whole as usize;
        at(d) * (1.0 - fraction) + at(d + 1) * fraction
    }

    /// Wet left and right output for one mono input sample.
    pub fn process(&mut self, input: f64) -> [f64; 2] {
        self.line[self.write] = input;
        let triangle = |p: f64| {
            if p < 0.5 {
                4.0 * p - 1.0
            } else {
                3.0 - 4.0 * p
            }
        };
        let opposite = self.phase + 0.5 - (self.phase + 0.5).floor();
        let out = [
            self.tap(self.center + self.depth * triangle(self.phase)),
            self.tap(self.center + self.depth * triangle(opposite)),
        ];
        self.write = (self.write + 1) % self.line.len();
        self.phase += self.step;
        self.phase -= self.phase.floor();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_impulse_returns_after_the_modulated_delay_and_ends() {
        let rate = 44_100.0;
        let mut chorus = Chorus::new(rate);
        let mut left = Vec::new();
        for n in 0..2000 {
            left.push(chorus.process(if n == 0 { 1.0 } else { 0.0 })[0]);
        }
        let first = left.iter().position(|s| *s != 0.0).unwrap();
        // At phase 0 the left tap is at its shortest delay.
        let shortest = ((CENTER_SECONDS - DEPTH_SECONDS) * rate) as usize;
        assert!((shortest..shortest + 6).contains(&first), "{first}");
        let energy: f64 = left.iter().sum();
        assert!((energy - 1.0).abs() < 0.01, "{energy}");
        assert!(left[1000..].iter().all(|s| *s == 0.0));
    }
}
