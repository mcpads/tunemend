//! Tunemend's SoundFont 2 synthesizer, used by the reference preview.
//!
//! References:
//!
//! - *SoundFont® Technical Specification*, version 2.04 (E-mu Systems,
//!   2002), for the file structure (§3–§7), generators (§8.1), modulator
//!   sources, transforms and defaults (§8.2–§8.4), precedence (§8.5, §9.4,
//!   §9.5) and the synthesis model (§9.1).
//! - *MIDI 1.0 Detailed Specification* (MMA) and RP-015 *Response to Reset
//!   All Controllers*, for channel messages, RPN 0–2, sustain and sostenuto,
//!   All Sound Off, All Notes Off and controller defaults.
//! - *General MIDI System Level 1* (MMA), for the percussion channel (MIDI
//!   channel 10, bank 128 in SoundFont terms) and power-on defaults (volume
//!   100, pan 64, expression 127, pitch bend range ±2 semitones).
//! - R. Bristow-Johnson, *Cookbook formulae for audio EQ biquad filter
//!   coefficients*, for the low-pass filter, and the published Freeverb
//!   description (Jezar, 2000) for the reverb tank shared with the mixer.
//!
//! Behaviour summary:
//!
//! - Each Note On is a separate instance of its key; a Note Off ends the
//!   earliest instance of that channel and key still held (first in, first
//!   out), as the MIDI 1.0 note model allows repeated Note Ons of one key.
//! - Every zone matching a note's key and velocity starts one voice. Sample
//!   playback uses 4-point Catmull–Rom interpolation; loop modes 0/2 (none),
//!   1 (continuous) and 3 (until release, then to the sample end).
//! - Volume envelope per sample; modulation envelope, both LFOs, pitch,
//!   filter, attenuation, pan and sends every [`voice::CONTROL_PERIOD`]
//!   samples and at once after a controller change.
//! - Reverb (CC91) and chorus (CC93) sends of every voice are summed without
//!   any audibility threshold into one shared reverb and one shared chorus.
//! - At most [`MAX_VOICES`] voices. A new voice beyond that replaces the
//!   released voice with the lowest envelope gain, or else the oldest voice;
//!   the replaced voice stops at once.
//!
//! Determinism: all arithmetic is IEEE 754 `f32`/`f64` in a fixed order,
//! and exponentials, logarithms and sines come from [`math`], which avoids
//! the platform math library. Voices are mixed in start order.
//!
//! Omitted: linked modulators (ignored), the SoundFont NRPN controls of
//! §9.6, poly pressure, the soft pedal (CC66), bank select LSB (CC32), data
//! increment/decrement, synchronised stereo sample pairs (each sample plays
//! as its own voice, panned by its zone) and ROM samples (refused).

mod chorus;
pub mod math;
pub mod modulator;
mod sf2;
pub mod voice;
mod zones;

use std::sync::Arc;

pub use sf2::{Instrument, Preset, SampleHeader, SoundFont, SoundFontError, Zone};
pub use zones::Region;

use crate::effects::ReverbTank;
use chorus::Chorus;
use modulator::SourceInputs;
use voice::{ChannelInputs, NoteStart, Outputs, Voice};

/// Simultaneous voices per synthesizer.
pub const MAX_VOICES: usize = 64;
/// The General MIDI percussion channel (MIDI channel 10).
pub const PERCUSSION_CHANNEL: u8 = 9;
/// Reverb room size and damping, in the mixer reverb's percent units.
pub const REVERB_ROOM_SIZE_PERCENT: f64 = 50.0;
pub const REVERB_DAMPING_PERCENT: f64 = 50.0;
/// Samples rendered per internal block.
const BLOCK: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynthError {
    SampleRate(u32),
}

impl std::fmt::Display for SynthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SampleRate(rate) => write!(f, "sample rate {rate} is outside 8000–384000"),
        }
    }
}

impl std::error::Error for SynthError {}

#[derive(Debug, Clone)]
struct Channel {
    controllers: [u8; 128],
    bank: u16,
    program: u8,
    preset: Option<usize>,
    pitch_wheel: u16,
    channel_pressure: u8,
    /// Registered parameter selected by CC101/CC100; `None` when an NRPN
    /// was selected more recently or RPN null.
    rpn: Option<(u8, u8)>,
    bend_range: (u8, u8),
    fine_tuning: (u8, u8),
    coarse_tuning: u8,
}

impl Channel {
    fn new(number: u8) -> Self {
        let mut controllers = [0; 128];
        controllers[7] = 100;
        controllers[10] = 64;
        controllers[11] = 127;
        controllers[100] = 127;
        controllers[101] = 127;
        Self {
            controllers,
            bank: if number == PERCUSSION_CHANNEL { 128 } else { 0 },
            program: 0,
            preset: None,
            pitch_wheel: 8192,
            channel_pressure: 0,
            rpn: None,
            bend_range: (2, 0),
            fine_tuning: (64, 0),
            coarse_tuning: 64,
        }
    }

    fn inputs(&self) -> ChannelInputs<'_> {
        let fine = (i32::from(self.fine_tuning.0) << 7 | i32::from(self.fine_tuning.1)) - 8192;
        ChannelInputs {
            sources: SourceInputs {
                velocity: 0,
                key: 0,
                controllers: &self.controllers,
                channel_pressure: self.channel_pressure,
                pitch_wheel: self.pitch_wheel,
                pitch_wheel_semitones: f64::from(self.bend_range.0)
                    + f64::from(self.bend_range.1) / 100.0,
            },
            tuning: f64::from(fine) * 100.0 / 8192.0
                + (f64::from(self.coarse_tuning) - 64.0) * 100.0,
        }
    }

    fn sustain(&self) -> bool {
        self.controllers[64] >= 64
    }
}

/// A SoundFont synthesizer rendering 16 MIDI channels to stereo.
pub struct Synthesizer {
    font: Arc<SoundFont>,
    rate: u32,
    effects: bool,
    channels: Vec<Channel>,
    voices: Vec<Voice>,
    next_note: u64,
    /// Note instances still keyed down, per channel and key, earliest
    /// first; each Note Off ends the earliest one.
    keyed: Vec<std::collections::VecDeque<u64>>,
    reverb: ReverbTank,
    chorus: Chorus,
    reverb_send: Vec<f32>,
    chorus_send: Vec<f32>,
}

impl Synthesizer {
    /// `effects` enables the shared reverb and chorus; without it the sends
    /// are not processed.
    pub fn new(font: Arc<SoundFont>, sample_rate: u32, effects: bool) -> Result<Self, SynthError> {
        if !(8_000..=384_000).contains(&sample_rate) {
            return Err(SynthError::SampleRate(sample_rate));
        }
        let rate = f64::from(sample_rate);
        let mut synthesizer = Self {
            font,
            rate: sample_rate,
            effects,
            channels: (0..16).map(Channel::new).collect(),
            voices: Vec::new(),
            next_note: 0,
            keyed: vec![std::collections::VecDeque::new(); 16 * 128],
            reverb: ReverbTank::new(REVERB_ROOM_SIZE_PERCENT, REVERB_DAMPING_PERCENT, rate),
            chorus: Chorus::new(rate),
            reverb_send: vec![0.0; BLOCK],
            chorus_send: vec![0.0; BLOCK],
        };
        for channel in 0..16 {
            synthesizer.select_preset(channel);
        }
        Ok(synthesizer)
    }

    /// Voices currently sounding.
    pub fn active_voices(&self) -> usize {
        self.voices.len()
    }

    /// Preset lookup: the channel's bank and program; then bank 0 (melodic)
    /// or bank 128 program 0 (percussion); then, for percussion, the melodic
    /// program, so a font without drum kits still sounds; then the first
    /// preset in (bank, program) order.
    fn select_preset(&mut self, channel: usize) {
        let c = &self.channels[channel];
        let program = u16::from(c.program);
        let font = &self.font;
        let preset = font.preset_index(c.bank, program).or_else(|| {
            if c.bank == 128 {
                font.preset_index(128, 0)
                    .or_else(|| font.preset_index(0, program))
            } else {
                font.preset_index(0, program)
            }
        });
        self.channels[channel].preset = preset.or_else(|| font.first_preset());
    }

    pub fn note_on(&mut self, channel: u8, key: u8, velocity: u8) {
        let (channel, key) = (channel & 15, key & 127);
        if velocity == 0 {
            return self.note_off(channel, key);
        }
        let note = self.next_note;
        self.next_note += 1;
        self.keyed[usize::from(channel) * 128 + usize::from(key)].push_back(note);
        let Some(preset) = self.channels[usize::from(channel)].preset else {
            return;
        };
        let regions = self.font.regions(preset, key, velocity.min(127));
        for region in regions {
            let class = region.generators[57];
            if class != 0 {
                for voice in &mut self.voices {
                    if voice.channel == channel
                        && voice.preset == preset
                        && voice.exclusive_class == class
                        && voice.note != note
                    {
                        voice.cut();
                    }
                }
            }
            let start = NoteStart {
                font: &self.font,
                region,
                channel,
                key,
                velocity: velocity.min(127),
                preset,
                note,
                rate: f64::from(self.rate),
            };
            let Some(voice) = Voice::start(start, &self.channels[usize::from(channel)].inputs())
            else {
                continue;
            };
            if self.voices.len() >= MAX_VOICES {
                self.steal();
            }
            self.voices.push(voice);
        }
    }

    /// Removes the released voice with the lowest gain, or the oldest voice.
    fn steal(&mut self) {
        let victim = self
            .voices
            .iter()
            .enumerate()
            .filter(|(_, v)| v.released)
            .min_by(|(_, a), (_, b)| a.level().total_cmp(&b.level()).then(a.note.cmp(&b.note)))
            .or_else(|| self.voices.iter().enumerate().min_by_key(|(_, v)| v.note))
            .map(|(i, _)| i);
        if let Some(i) = victim {
            self.voices.remove(i);
        }
    }

    /// Ends the earliest instance of the key still held on the channel.
    /// Repeated Note Ons of one key are separate instances (MIDI 1.0), so
    /// stacked notes release one at a time, first in, first out.
    pub fn note_off(&mut self, channel: u8, key: u8) {
        let (channel, key) = (channel & 15, key & 127);
        let Some(note) = self.keyed[usize::from(channel) * 128 + usize::from(key)].pop_front()
        else {
            return;
        };
        let sustain = self.channels[usize::from(channel)].sustain();
        for voice in &mut self.voices {
            if voice.note == note && voice.held {
                voice.held = false;
                if !sustain && !voice.sostenuto {
                    voice.release();
                }
            }
        }
    }

    pub fn program_change(&mut self, channel: u8, program: u8) {
        let channel = usize::from(channel & 15);
        self.channels[channel].program = program & 127;
        self.select_preset(channel);
    }

    /// Pitch wheel, 0–16383 with 8192 at rest.
    pub fn pitch_bend(&mut self, channel: u8, value: u16) {
        let channel = channel & 15;
        self.channels[usize::from(channel)].pitch_wheel = value.min(16383);
        self.touch(channel);
    }

    pub fn channel_pressure(&mut self, channel: u8, value: u8) {
        let channel = channel & 15;
        self.channels[usize::from(channel)].channel_pressure = value & 127;
        self.touch(channel);
    }

    /// Voices of the channel re-evaluate their modulators now.
    fn touch(&mut self, channel: u8) {
        for voice in &mut self.voices {
            if voice.channel == channel {
                voice.dirty = true;
            }
        }
    }

    pub fn control_change(&mut self, channel: u8, controller: u8, value: u8) {
        let (number, controller, value) = (channel & 15, controller & 127, value & 127);
        let channel = usize::from(number);
        let was_sustained = self.channels[channel].sustain();
        self.channels[channel].controllers[usize::from(controller)] = value;
        match controller {
            0 if number != PERCUSSION_CHANNEL => self.channels[channel].bank = u16::from(value),
            6 | 38 => self.data_entry(channel, controller == 6, value),
            64 if was_sustained && value < 64 => {
                for voice in &mut self.voices {
                    if voice.channel == number && !voice.held && !voice.sostenuto {
                        voice.release();
                    }
                }
            }
            66 => {
                for voice in &mut self.voices {
                    if voice.channel != number {
                        continue;
                    }
                    if value >= 64 {
                        voice.sostenuto |= voice.held && !voice.released;
                    } else if voice.sostenuto {
                        voice.sostenuto = false;
                        if !voice.held && !self.channels[channel].sustain() {
                            voice.release();
                        }
                    }
                }
            }
            98 | 99 => self.channels[channel].rpn = None,
            100 | 101 => {
                let c = &mut self.channels[channel];
                c.rpn = Some((c.controllers[101], c.controllers[100]));
            }
            120 => {
                self.voices.retain(|v| v.channel != number);
                self.unkey(channel);
            }
            121 => self.reset_controllers(channel),
            123..=127 => {
                self.unkey(channel);
                for voice in &mut self.voices {
                    if voice.channel == number && voice.held {
                        voice.held = false;
                        if !self.channels[channel].sustain() && !voice.sostenuto {
                            voice.release();
                        }
                    }
                }
            }
            _ => {}
        }
        self.touch(number);
    }

    /// Forgets the keyed instances of a channel whose notes were all ended.
    fn unkey(&mut self, channel: usize) {
        for keyed in &mut self.keyed[channel * 128..(channel + 1) * 128] {
            keyed.clear();
        }
    }

    fn data_entry(&mut self, channel: usize, msb: bool, value: u8) {
        let c = &mut self.channels[channel];
        match (c.rpn, msb) {
            (Some((0, 0)), true) => c.bend_range.0 = value,
            (Some((0, 0)), false) => c.bend_range.1 = value.min(99),
            (Some((0, 1)), true) => c.fine_tuning.0 = value,
            (Some((0, 1)), false) => c.fine_tuning.1 = value,
            (Some((0, 2)), true) => c.coarse_tuning = value,
            _ => {}
        }
    }

    /// RP-015: modulation, expression, pedals, pitch wheel, channel pressure
    /// and the parameter selection reset; volume, pan, sends, bank and
    /// program keep their values.
    fn reset_controllers(&mut self, channel: usize) {
        let c = &mut self.channels[channel];
        c.controllers[1] = 0;
        c.controllers[11] = 127;
        for pedal in 64..=69 {
            c.controllers[pedal] = 0;
        }
        c.controllers[100] = 127;
        c.controllers[101] = 127;
        c.pitch_wheel = 8192;
        c.channel_pressure = 0;
        c.rpn = None;
        let number = channel as u8;
        for voice in &mut self.voices {
            if voice.channel == number {
                voice.sostenuto = false;
                if !voice.held {
                    voice.release();
                }
            }
        }
    }

    /// Renders the next `left.len()` samples into both buffers.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        assert_eq!(left.len(), right.len(), "channel buffers differ in length");
        left.fill(0.0);
        right.fill(0.0);
        let mut at = 0;
        while at < left.len() {
            let n = BLOCK.min(left.len() - at);
            let (l, r) = (&mut left[at..at + n], &mut right[at..at + n]);
            let reverb = &mut self.reverb_send[..n];
            let chorus = &mut self.chorus_send[..n];
            reverb.fill(0.0);
            chorus.fill(0.0);
            for voice in &mut self.voices {
                let inputs = self.channels[usize::from(voice.channel)].inputs();
                voice.render(
                    &inputs,
                    Outputs {
                        left: &mut *l,
                        right: &mut *r,
                        reverb: &mut *reverb,
                        chorus: &mut *chorus,
                    },
                );
            }
            self.voices.retain(|v| !v.finished);
            if self.effects {
                for i in 0..n {
                    // The mono send enters both tank inputs, like the left
                    // plus right sum the mixer reverb receives.
                    let wet = self.reverb.process(2.0 * f64::from(reverb[i]));
                    let sung = self.chorus.process(f64::from(chorus[i]));
                    l[i] += (wet[0] + sung[0]) as f32;
                    r[i] += (wet[1] + sung[1]) as f32;
                }
            }
            at += n;
        }
    }
}

#[cfg(test)]
mod tests;
