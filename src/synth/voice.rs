//! One sounding sample: oscillator, low-pass filter, envelopes, LFOs, gain,
//! pan and effect sends (SF2.04 §9.1).
//!
//! Control-rate values — pitch, filter cutoff, attenuation, pan, sends, the
//! LFOs and the modulation envelope — are evaluated every
//! [`CONTROL_PERIOD`] samples counted from the voice start, and at once when
//! a controller of its channel changes. Gains ramp linearly to the new value
//! over the next period. The volume envelope runs per sample.

use std::sync::Arc;

use super::math::{centibels_to_gain, db_to_gain, exp2, sin_cos};
use super::modulator::{GENERATORS, INITIAL_PITCH, Modulator, SourceInputs, convex};
use super::sf2::SoundFont;
use super::zones::Region;

/// Samples between control-rate updates.
pub const CONTROL_PERIOD: u32 = 32;
/// A volume envelope below −100 dB ends the voice (SF2.04 §8.1.2, gen 38).
const SILENT: f64 = 1.0e-5;
/// Release time of a voice cut by an exclusive class, in seconds.
pub const EXCLUSIVE_RELEASE_SECONDS: f64 = 0.005;

/// Seconds for a timecent value; `zero_floor` treats the table's minimum
/// (−12000) and below as zero, otherwise only −32768 and below are zero.
fn seconds(timecents: f64, max: f64, zero_floor: bool) -> f64 {
    if timecents <= -32768.0 || (zero_floor && timecents <= -12000.0) {
        0.0
    } else {
        exp2(timecents.clamp(-12000.0, max) / 1200.0)
    }
}

fn samples(seconds: f64, rate: f64) -> u64 {
    (seconds * rate).round() as u64
}

/// Per-sample factor that lowers a gain by 100 dB over `n` samples.
fn hundred_db_factor(n: u64) -> f64 {
    if n == 0 {
        0.0
    } else {
        db_to_gain(-100.0 / n as f64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Delay,
    Attack,
    Hold,
    Decay,
    Sustain,
    Release,
    Done,
}

/// DAHDSR volume envelope: attack linear in amplitude, decay and release
/// linear in decibels (100 dB per decay or release time).
#[derive(Debug, Clone)]
pub struct VolumeEnvelope {
    stage: Stage,
    count: u64,
    delay: u64,
    attack: u64,
    hold: u64,
    decay_factor: f64,
    sustain: f64,
    release_factor: f64,
    gain: f64,
}

impl VolumeEnvelope {
    pub fn new(delay: u64, attack: u64, hold: u64, decay: u64, sustain_centibels: f64) -> Self {
        Self {
            stage: Stage::Delay,
            count: 0,
            delay,
            attack,
            hold,
            decay_factor: hundred_db_factor(decay),
            sustain: centibels_to_gain(sustain_centibels.clamp(0.0, 1440.0)),
            release_factor: 0.0,
            gain: 0.0,
        }
    }

    /// The gain for the next sample.
    pub fn next_gain(&mut self) -> f64 {
        loop {
            match self.stage {
                Stage::Delay if self.count < self.delay => {
                    self.count += 1;
                    return 0.0;
                }
                Stage::Delay => (self.stage, self.count) = (Stage::Attack, 0),
                Stage::Attack if self.count < self.attack => {
                    self.count += 1;
                    self.gain = self.count as f64 / self.attack as f64;
                    return self.gain;
                }
                Stage::Attack => (self.stage, self.count, self.gain) = (Stage::Hold, 0, 1.0),
                Stage::Hold if self.count < self.hold => {
                    self.count += 1;
                    return self.gain;
                }
                Stage::Hold => self.stage = Stage::Decay,
                Stage::Decay if self.gain > self.sustain => {
                    self.gain = (self.gain * self.decay_factor).max(self.sustain);
                    return self.gain;
                }
                Stage::Decay => self.stage = Stage::Sustain,
                Stage::Sustain if self.sustain <= SILENT => self.stage = Stage::Done,
                Stage::Sustain => return self.gain,
                Stage::Release => {
                    self.gain *= self.release_factor;
                    if self.gain < SILENT {
                        self.stage = Stage::Done;
                        self.gain = 0.0;
                    }
                    return self.gain;
                }
                Stage::Done => return 0.0,
            }
        }
    }

    /// Enters release: 100 dB of decay per `samples`.
    pub fn release(&mut self, samples: u64) {
        if self.stage == Stage::Done {
            return;
        }
        if self.stage == Stage::Delay || samples == 0 {
            self.stage = Stage::Done;
            self.gain = 0.0;
            return;
        }
        self.stage = Stage::Release;
        self.release_factor = hundred_db_factor(samples);
    }

    pub fn done(&self) -> bool {
        self.stage == Stage::Done
    }
}

/// DAHDSR modulation envelope, 0…1: convex attack, linear decay and
/// release (a full-scale change per decay or release time).
#[derive(Debug, Clone)]
pub struct ModulationEnvelope {
    stage: Stage,
    count: u64,
    delay: u64,
    attack: u64,
    hold: u64,
    decay: u64,
    sustain: f64,
    release: u64,
    release_from: f64,
    value: f64,
}

impl ModulationEnvelope {
    pub fn new(delay: u64, attack: u64, hold: u64, decay: u64, sustain: f64) -> Self {
        let mut envelope = Self {
            stage: Stage::Delay,
            count: 0,
            delay,
            attack,
            hold,
            decay,
            sustain: sustain.clamp(0.0, 1.0),
            release: 0,
            release_from: 0.0,
            value: 0.0,
        };
        envelope.advance(0);
        envelope
    }

    pub fn value(&self) -> f64 {
        self.value
    }

    /// Moves `n` samples forward.
    pub fn advance(&mut self, mut n: u64) {
        loop {
            let (length, next) = match self.stage {
                Stage::Delay => (self.delay, Stage::Attack),
                Stage::Attack => (self.attack, Stage::Hold),
                Stage::Hold => (self.hold, Stage::Decay),
                Stage::Decay => {
                    let span = ((1.0 - self.sustain) * self.decay as f64).ceil() as u64;
                    (span, Stage::Sustain)
                }
                Stage::Release => {
                    let span = (self.release_from * self.release as f64).ceil() as u64;
                    (span, Stage::Done)
                }
                Stage::Sustain | Stage::Done => break,
            };
            let step = n.min(length - self.count.min(length));
            self.count += step;
            n -= step;
            if self.count >= length {
                self.stage = next;
                self.count = 0;
                continue;
            }
            break;
        }
        let t = self.count as f64;
        self.value = match self.stage {
            Stage::Delay => 0.0,
            Stage::Attack => convex(t / self.attack as f64),
            Stage::Hold => 1.0,
            Stage::Decay => (1.0 - t / self.decay as f64).max(self.sustain),
            Stage::Sustain => self.sustain,
            Stage::Release => (self.release_from - t / self.release as f64).max(0.0),
            Stage::Done => 0.0,
        };
    }

    pub fn release(&mut self, samples: u64) {
        if self.stage == Stage::Done {
            return;
        }
        self.release_from = self.value;
        self.release = samples;
        self.stage = Stage::Release;
        self.count = 0;
        self.advance(0);
    }
}

/// Triangle LFO (§9.1.6): zero during its delay, then up to +1, down to −1.
#[derive(Debug, Clone)]
pub struct Lfo {
    delay: u64,
    elapsed: u64,
    phase: f64,
    step: f64,
}

impl Lfo {
    pub fn new(delay: u64, hz: f64, rate: f64) -> Self {
        Self {
            delay,
            elapsed: 0,
            phase: 0.0,
            step: hz / rate,
        }
    }

    pub fn advance(&mut self, mut n: u64) {
        if self.elapsed < self.delay {
            let wait = n.min(self.delay - self.elapsed);
            self.elapsed += wait;
            n -= wait;
        }
        self.phase += n as f64 * self.step;
        self.phase -= self.phase.floor();
    }

    pub fn value(&self) -> f64 {
        if self.elapsed < self.delay {
            0.0
        } else if self.phase < 0.25 {
            4.0 * self.phase
        } else if self.phase < 0.75 {
            2.0 - 4.0 * self.phase
        } else {
            4.0 * self.phase - 4.0
        }
    }
}

/// Two-pole resonant low-pass (RBJ cookbook low-pass biquad). The
/// resonance in centibels sets the peak height above DC exactly
/// (Q² = (P² + P·√(P² − 1)) / 2 for peak gain P), and DC gain drops by half
/// the resonance, as §8.1.2 gen 9 describes. At the maximum cutoff (13500
/// cents) without resonance the filter is open. Cutoff is limited to 0.45
/// of the output rate.
#[derive(Debug, Clone)]
pub struct Filter {
    open: bool,
    key: (f64, f64),
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Default for Filter {
    fn default() -> Self {
        Self::new()
    }
}

impl Filter {
    pub fn new() -> Self {
        Self {
            open: true,
            key: (f64::NAN, f64::NAN),
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    pub fn configure(&mut self, cents: f64, resonance: f64, rate: f64) {
        if self.key == (cents, resonance) {
            return;
        }
        self.key = (cents, resonance);
        if cents >= 13500.0 && resonance <= 0.0 {
            self.open = true;
            return;
        }
        self.open = false;
        let hz = (440.0 * exp2((cents - 6900.0) / 1200.0)).min(0.45 * rate);
        let (sin, cos) = sin_cos(2.0 * std::f64::consts::PI * hz / rate);
        let peak = db_to_gain(resonance / 10.0);
        let q = ((peak * peak + peak * (peak * peak - 1.0).max(0.0).sqrt()) / 2.0).sqrt();
        let alpha = sin / (2.0 * q);
        let dc = db_to_gain(-resonance / 20.0);
        let a0 = 1.0 + alpha;
        self.b1 = (1.0 - cos) / a0 * dc;
        self.b0 = self.b1 / 2.0;
        self.b2 = self.b0;
        self.a1 = -2.0 * cos / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    pub fn process(&mut self, x: f64) -> f64 {
        if self.open {
            (self.x1, self.x2, self.y1, self.y2) = (x, x, x, x);
            return x;
        }
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        (self.x2, self.x1) = (self.x1, x);
        (self.y2, self.y1) = (self.y1, y);
        y
    }
}

/// Equal-power pan gains for a pan of −500 (left) to 500 (right).
pub fn pan_gains(pan: f64) -> (f64, f64) {
    let angle = (pan.clamp(-500.0, 500.0) + 500.0) / 1000.0 * std::f64::consts::FRAC_PI_2;
    let (sin, cos) = sin_cos(angle);
    (cos, sin)
}

/// What a voice reads from its channel.
pub struct ChannelInputs<'a> {
    pub sources: SourceInputs<'a>,
    /// RPN fine and coarse tuning, in cents.
    pub tuning: f64,
}

/// Buffers a voice adds into.
pub struct Outputs<'a> {
    pub left: &'a mut [f32],
    pub right: &'a mut [f32],
    pub reverb: &'a mut [f32],
    pub chorus: &'a mut [f32],
}

#[derive(Debug, Clone)]
pub struct Voice {
    pub channel: u8,
    /// The received key, for note-off.
    pub key: u8,
    /// Note-on sequence number.
    pub note: u64,
    pub preset: usize,
    pub exclusive_class: i32,
    /// The key is down.
    pub held: bool,
    /// Held by the sostenuto pedal.
    pub sostenuto: bool,
    pub released: bool,
    pub finished: bool,
    key_number: u8,
    velocity: u8,
    generators: [i32; GENERATORS],
    modulators: Vec<Modulator>,
    values: [f64; GENERATORS],
    initial_pitch: f64,
    data: Arc<[f32]>,
    start: usize,
    end: usize,
    loop_start: usize,
    loop_end: usize,
    loop_mode: u8,
    position: f64,
    root_key: f64,
    correction: f64,
    sample_ratio: f64,
    rate: f64,
    volume: VolumeEnvelope,
    modulation: ModulationEnvelope,
    mod_lfo: Lfo,
    vib_lfo: Lfo,
    filter: Filter,
    countdown: u32,
    elapsed: u64,
    pub dirty: bool,
    increment: f64,
    pan: (f64, f64, f64),
    gains: [f64; 4],
    steps: [f64; 4],
    started: bool,
}

pub struct NoteStart<'a> {
    pub font: &'a SoundFont,
    pub region: Region,
    pub channel: u8,
    pub key: u8,
    pub velocity: u8,
    pub preset: usize,
    pub note: u64,
    pub rate: f64,
}

impl Voice {
    /// Starts a voice, or returns `None` when its sample range is empty.
    pub fn start(note: NoteStart, inputs: &ChannelInputs) -> Option<Self> {
        let NoteStart {
            font,
            region,
            channel,
            key,
            velocity,
            preset,
            note,
            rate,
        } = note;
        let g = &region.generators;
        let header = &font.samples[region.sample];
        let points = font.data.len() as i64;
        let address = |base: u32, fine: usize, coarse: usize| {
            (i64::from(base) + i64::from(g[fine]) + 32768 * i64::from(g[coarse])).clamp(0, points)
                as usize
        };
        let start = address(header.start, 0, 4);
        let end = address(header.end, 1, 12);
        if start >= end {
            return None;
        }
        let loop_start = address(header.loop_start, 2, 45).clamp(start, end);
        let loop_end = address(header.loop_end, 3, 50).clamp(start, end);
        let mut loop_mode = (g[54] & 3) as u8;
        if loop_mode == 2 || loop_end <= loop_start {
            loop_mode = 0;
        }
        let key_number = if (0..=127).contains(&g[46]) {
            g[46] as u8
        } else {
            key
        };
        let velocity = if (1..=127).contains(&g[47]) {
            g[47] as u8
        } else {
            velocity
        };
        let root_key = if (0..=127).contains(&g[58]) {
            g[58]
        } else if header.original_key <= 127 {
            i32::from(header.original_key)
        } else {
            60
        };
        let sample_rate = if header.sample_rate == 0 {
            400
        } else {
            header.sample_rate
        };
        let mut voice = Voice {
            channel,
            key,
            note,
            preset,
            exclusive_class: g[57],
            held: true,
            sostenuto: false,
            released: false,
            finished: false,
            key_number,
            velocity,
            generators: region.generators,
            modulators: region.modulators,
            values: [0.0; GENERATORS],
            initial_pitch: 0.0,
            data: font.data.clone(),
            start,
            end,
            loop_start,
            loop_end,
            loop_mode,
            position: start as f64,
            root_key: f64::from(root_key),
            correction: f64::from(header.correction),
            sample_ratio: f64::from(sample_rate) / rate,
            rate,
            volume: VolumeEnvelope::new(0, 0, 0, 0, 0.0),
            modulation: ModulationEnvelope::new(0, 0, 0, 0, 1.0),
            mod_lfo: Lfo::new(0, 0.0, rate),
            vib_lfo: Lfo::new(0, 0.0, rate),
            filter: Filter::new(),
            countdown: 0,
            elapsed: 0,
            dirty: true,
            increment: 0.0,
            pan: (f64::NAN, 0.0, 0.0),
            gains: [0.0; 4],
            steps: [0.0; 4],
            started: false,
        };
        voice.evaluate(inputs);
        let v = voice.values;
        let key_offset = 60.0 - f64::from(key_number);
        let time = |tc: f64, max: f64, floor: bool| samples(seconds(tc, max, floor), rate);
        voice.volume = VolumeEnvelope::new(
            time(v[33], 5000.0, true),
            time(v[34], 8000.0, false),
            time(v[35] + v[39] * key_offset, 5000.0, true),
            time(v[36] + v[40] * key_offset, 8000.0, false),
            v[37],
        );
        voice.modulation = ModulationEnvelope::new(
            time(v[25], 5000.0, true),
            time(v[26], 8000.0, false),
            time(v[27] + v[31] * key_offset, 5000.0, true),
            time(v[28] + v[32] * key_offset, 8000.0, false),
            1.0 - v[29].clamp(0.0, 1000.0) / 1000.0,
        );
        let lfo_hz = |cents: f64| 440.0 * exp2((cents.clamp(-16000.0, 4500.0) - 6900.0) / 1200.0);
        voice.mod_lfo = Lfo::new(time(v[21], 5000.0, true), lfo_hz(v[22]), rate);
        voice.vib_lfo = Lfo::new(time(v[23], 5000.0, true), lfo_hz(v[24]), rate);
        Some(voice)
    }

    /// Generators plus modulator outputs.
    fn evaluate(&mut self, inputs: &ChannelInputs) {
        let sources = SourceInputs {
            velocity: self.velocity,
            key: self.key_number,
            ..inputs.sources
        };
        for (value, generator) in self.values.iter_mut().zip(self.generators) {
            *value = f64::from(generator);
        }
        self.initial_pitch = 0.0;
        for modulator in &self.modulators {
            let amount = modulator.value(&sources);
            if modulator.destination == INITIAL_PITCH {
                self.initial_pitch += amount;
            } else {
                self.values[usize::from(modulator.destination)] += amount;
            }
        }
        self.dirty = false;
    }

    /// Key release: the envelopes enter their release phases.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let rate = self.rate;
        let release = |tc: f64| samples(seconds(tc, 8000.0, false), rate);
        let (volume, modulation) = (release(self.values[38]), release(self.values[30]));
        self.volume.release(volume);
        self.modulation.release(modulation);
    }

    /// Rapid termination by another note's exclusive class.
    pub fn cut(&mut self) {
        self.released = true;
        let n = samples(EXCLUSIVE_RELEASE_SECONDS, self.rate);
        self.volume.release(n);
        self.modulation.release(n);
    }

    /// Current volume envelope gain, for voice stealing.
    pub fn level(&self) -> f64 {
        self.volume.gain
    }

    fn looping(&self) -> bool {
        self.loop_mode == 1 || (self.loop_mode == 3 && !self.released)
    }

    fn update(&mut self, inputs: &ChannelInputs) {
        if self.dirty {
            self.evaluate(inputs);
        }
        self.mod_lfo.advance(self.elapsed);
        self.vib_lfo.advance(self.elapsed);
        self.modulation.advance(self.elapsed);
        self.elapsed = 0;
        let v = &self.values;
        let (mod_lfo, vib_lfo, envelope) = (
            self.mod_lfo.value(),
            self.vib_lfo.value(),
            self.modulation.value(),
        );

        let cents = v[56].clamp(0.0, 1200.0) * (f64::from(self.key_number) - self.root_key)
            + v[51].clamp(-120.0, 120.0) * 100.0
            + v[52].clamp(-99.0, 99.0)
            + self.correction
            + inputs.tuning
            + self.initial_pitch
            + v[5] * mod_lfo
            + v[6] * vib_lfo
            + v[7] * envelope;
        self.increment = exp2(cents / 1200.0) * self.sample_ratio;

        let cutoff = (v[8] + v[10] * mod_lfo + v[11] * envelope).clamp(1500.0, 13500.0);
        self.filter
            .configure(cutoff, v[9].clamp(0.0, 960.0), self.rate);

        let amplitude = centibels_to_gain(v[48].clamp(0.0, 1440.0) - v[13] * mod_lfo);
        if self.pan.0 != v[17] {
            let (left, right) = pan_gains(v[17]);
            self.pan = (v[17], left, right);
        }
        let targets = [
            amplitude * self.pan.1,
            amplitude * self.pan.2,
            amplitude * v[16].clamp(0.0, 1000.0) / 1000.0,
            amplitude * v[15].clamp(0.0, 1000.0) / 1000.0,
        ];
        let period = f64::from(CONTROL_PERIOD);
        for ((gain, step), target) in self.gains.iter_mut().zip(&mut self.steps).zip(targets) {
            if self.started {
                *step = (target - *gain) / period;
            } else {
                (*gain, *step) = (target, 0.0);
            }
        }
        self.started = true;
        self.countdown = CONTROL_PERIOD;
    }

    fn point(&self, index: i64, looping: bool) -> f64 {
        let mut i = index;
        if looping && i >= self.loop_end as i64 {
            let length = (self.loop_end - self.loop_start) as i64;
            i -= ((i - self.loop_end as i64) / length + 1) * length;
        }
        if i < self.start as i64 || i >= self.end as i64 {
            0.0
        } else {
            f64::from(self.data[i as usize])
        }
    }

    /// Adds this voice's next `out.left.len()` samples.
    pub fn render(&mut self, inputs: &ChannelInputs, out: Outputs) {
        for n in 0..out.left.len() {
            if self.countdown == 0 || self.dirty {
                self.update(inputs);
            }
            self.countdown -= 1;
            self.elapsed += 1;
            let envelope = self.volume.next_gain();
            if self.volume.done() {
                self.finished = true;
                return;
            }
            let looping = self.looping();
            let index = self.position as i64;
            let t = self.position - index as f64;
            let (p0, p1, p2, p3) = (
                self.point(index - 1, looping),
                self.point(index, looping),
                self.point(index + 1, looping),
                self.point(index + 2, looping),
            );
            // 4-point Catmull–Rom cubic.
            let sample = p1
                + 0.5
                    * t
                    * (p2 - p0
                        + t * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3
                            + t * (3.0 * (p1 - p2) + p3 - p0)));
            let signal = self.filter.process(sample) * envelope;
            out.left[n] += (signal * self.gains[0]) as f32;
            out.right[n] += (signal * self.gains[1]) as f32;
            out.reverb[n] += (signal * self.gains[2]) as f32;
            out.chorus[n] += (signal * self.gains[3]) as f32;
            for (gain, step) in self.gains.iter_mut().zip(self.steps) {
                *gain += step;
            }
            self.position += self.increment;
            if looping {
                let end = self.loop_end as f64;
                if self.position >= end {
                    let length = (self.loop_end - self.loop_start) as f64;
                    self.position -= length * (((self.position - end) / length).floor() + 1.0);
                }
            } else if self.position >= self.end as f64 {
                self.finished = true;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_envelope_follows_timecents() {
        // 10 ms attack, 20 ms hold, decay to −20 dB at 100 dB per second.
        let rate = 1000.0;
        let t = |tc: f64| samples(seconds(tc, 8000.0, false), rate);
        assert_eq!(t(-7973.0), 10, "1200·log2(0.01) timecents is 10 ms");
        assert_eq!(t(0.0), 1000);
        assert_eq!(samples(seconds(-12000.0, 5000.0, true), rate), 0);
        assert_eq!(t(-32768.0), 0);
        let mut e = VolumeEnvelope::new(5, 10, 20, 1000, 200.0);
        let gains: Vec<f64> = (0..400).map(|_| e.next_gain()).collect();
        assert!(gains[..5].iter().all(|g| *g == 0.0), "delay");
        assert_eq!(gains[5], 0.1);
        assert_eq!(gains[14], 1.0, "attack ends after 10 samples");
        assert!(gains[15..35].iter().all(|g| *g == 1.0), "hold");
        // −20 dB at 100 dB per 1000 samples takes 200 samples.
        let db = |g: f64| 20.0 * g.log10();
        assert!(
            (db(gains[35 + 99]) + 10.0).abs() < 0.01,
            "{}",
            db(gains[134])
        );
        assert!((gains[399] - 0.1).abs() < 1e-12, "sustain at −20 dB");
        e.release(500);
        let mut n = 0;
        while !e.done() {
            e.next_gain();
            n += 1;
        }
        // From −20 dB to −100 dB at 100 dB per 500 samples.
        assert!((399..=401).contains(&n), "{n}");
    }

    #[test]
    fn modulation_envelope_and_lfo_shapes() {
        let mut e = ModulationEnvelope::new(10, 10, 5, 100, 0.5);
        assert_eq!(e.value(), 0.0);
        e.advance(15);
        assert!(
            e.value() > 0.5 && e.value() < 1.0,
            "convex attack {}",
            e.value()
        );
        e.advance(10);
        assert_eq!(e.value(), 1.0);
        e.advance(25);
        assert!((e.value() - 0.75).abs() < 1e-12, "{}", e.value());
        e.advance(1000);
        assert_eq!(e.value(), 0.5);
        e.release(100);
        e.advance(25);
        assert!((e.value() - 0.25).abs() < 1e-12);
        e.advance(100);
        assert_eq!(e.value(), 0.0);

        let mut lfo = Lfo::new(4, 1.0, 8.0);
        assert_eq!(lfo.value(), 0.0);
        lfo.advance(4);
        assert_eq!(lfo.value(), 0.0);
        let values: Vec<f64> = (0..8)
            .map(|_| {
                lfo.advance(1);
                lfo.value()
            })
            .collect();
        assert_eq!(values, [0.5, 1.0, 0.5, 0.0, -0.5, -1.0, -0.5, 0.0]);
    }

    #[test]
    fn pan_is_equal_power() {
        let (l, r) = pan_gains(0.0);
        assert!((l - r).abs() < 1e-15 && (l * l + r * r - 1.0).abs() < 1e-15);
        assert_eq!(pan_gains(-500.0), (1.0, 0.0));
        let (l, r) = pan_gains(500.0);
        assert!(l.abs() < 1e-16 && r == 1.0);
        for pan in [-400.0, -120.0, 250.0] {
            let (l, r) = pan_gains(pan);
            assert!((l * l + r * r - 1.0).abs() < 1e-14);
        }
    }

    #[test]
    fn filter_is_open_at_maximum_and_low_passes_below() {
        let mut f = Filter::new();
        f.configure(13500.0, 0.0, 44100.0);
        assert_eq!(f.process(0.3), 0.3);
        // 1 kHz cutoff: DC passes, a 10 kHz tone is strongly attenuated.
        let response = |hz: f64, resonance: f64| {
            let mut f = Filter::new();
            f.configure(
                6900.0 + 1200.0 * (1000.0f64 / 440.0).log2(),
                resonance,
                44100.0,
            );
            let mut peak = 0.0f64;
            for n in 0..44100 {
                let y = f.process((2.0 * std::f64::consts::PI * hz * n as f64 / 44100.0).cos());
                if n > 22050 {
                    peak = peak.max(y.abs());
                }
            }
            peak
        };
        assert!((response(0.0, 0.0) - 1.0).abs() < 1e-9);
        assert!(response(10_000.0, 0.0) < 0.02);
        // 120 cB resonance: peak 12 dB above a DC gain lowered by 6 dB.
        let dc = response(0.0, 120.0);
        assert!((20.0 * dc.log10() + 6.0).abs() < 0.01, "{dc}");
        let peak = (900..1100)
            .step_by(10)
            .map(|hz| response(f64::from(hz), 120.0))
            .fold(0.0, f64::max);
        assert!((20.0 * (peak / dc).log10() - 12.0).abs() < 0.2, "{peak}");
    }
}
