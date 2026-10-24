//! Game Boy DMG APU model: two pulse channels (the first with a frequency
//! sweep), a 32-sample 4-bit wave channel, a noise channel, the 512 Hz frame
//! sequencer, NR50/NR51 mixing and NR52 power control.
//!
//! References: Pan Docs
//! ("Audio", "Audio Registers" and "Audio details" on gbdev.io: register map,
//! duty table, period dividers at 1048576 and 2097152 Hz, length timers of 64
//! and 256, the 64 Hz envelope, sweep arithmetic and overflow, wave RAM nibble
//! order and the first sample after a trigger, the XNOR noise LFSR with its
//! 7-bit mode, DAC enables, the DAC's digital-to-analog mapping, NR50/NR51,
//! and power-off clearing with the monochrome length-timer exception) and the
//! VGM 1.71 specification (0x80 clock and the 0xB3 command). Behaviour the
//! documents leave open is a declared choice in [`model_scope`].
use crate::psg::{PsgNote, PsgPitch};
use crate::sound_chip::{Chip, ChipWrite, Target};
use serde_json::{Value, json};

pub const DMG_CLOCK_HZ: u32 = 4_194_304;
pub const MIN_CLOCK_HZ: u32 = 1_000_000;
pub const MAX_CLOCK_HZ: u32 = 8_388_608;
/// Input clock cycles per model tick (the 2097152 Hz wave timer rate).
pub const TICK_DIVIDER: u32 = 2;
/// Ticks per frame-sequencer step: 8192 input clocks, 512 Hz at 4194304 Hz.
pub const SEQUENCER_TICKS: u32 = 4096;
/// Analog units per digital step: the DAC maps digital 0..15 to +8190..-8190.
pub const DAC_STEP: i32 = 546;

/// Duty waveforms, one entry per duty step 0..7.
pub const DUTY: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 1, 1, 1],
    [0, 1, 1, 1, 1, 1, 1, 0],
];
const DUTY_NAMES: [&str; 4] = ["12.5%", "25%", "50%", "75%"];
const NOISE_DIVISORS: [u32; 8] = [8, 16, 32, 48, 64, 80, 96, 112];
const OUTPUT_LEVELS: [&str; 4] = ["mute", "100%", "50%", "25%"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GbConfig {
    /// DMG behaviour: length timers keep their value through power-off and
    /// accept NRx1 length writes while the APU is off.
    pub dmg_length_while_off: bool,
}

impl GbConfig {
    pub fn dmg() -> Self {
        Self {
            dmg_length_while_off: true,
        }
    }
    pub fn to_json(&self) -> Value {
        json!({"variant":"dmg","tick":format!("clock/{TICK_DIVIDER}"),"native_sample":format!("mean of {} ticks (clock/{})", GbApu::TICKS_PER_FRAME, TICK_DIVIDER * GbApu::TICKS_PER_FRAME),
            "frame_sequencer":"512 Hz at 4194304 Hz: length on even steps (256 Hz), sweep on steps 2 and 6 (128 Hz), envelope on step 7 (64 Hz)",
            "dac":"digital 0..15 to +1..-1 (8190 per unit), 0 when the DAC is off","mixer":"NR51 routing, NR50 volume (v+1)/8 per side",
            "length_through_power_off":self.dmg_length_while_off,"power_on_state":"powered on, registers 0"})
    }
}

pub fn check_clock(clock_hz: u32) -> Result<(), String> {
    if (MIN_CLOCK_HZ..=MAX_CLOCK_HZ).contains(&clock_hz) {
        Ok(())
    } else {
        Err(format!(
            "clock_hz must be {MIN_CLOCK_HZ}..{MAX_CLOCK_HZ}, such as {DMG_CLOCK_HZ} (Game Boy)"
        ))
    }
}

/// Register name of an offset from 0xFF10, or `None` for an unused address.
pub fn register_name(offset: u8) -> Option<&'static str> {
    const NAMES: [&str; 23] = [
        "NR10", "NR11", "NR12", "NR13", "NR14", "", "NR21", "NR22", "NR23", "NR24", "NR30", "NR31",
        "NR32", "NR33", "NR34", "", "NR41", "NR42", "NR43", "NR44", "NR50", "NR51", "NR52",
    ];
    match offset {
        0x20..=0x2f => Some("wave_ram"),
        _ => NAMES
            .get(usize::from(offset))
            .copied()
            .filter(|n| !n.is_empty()),
    }
}

#[derive(Debug, Clone, Default)]
struct Channel {
    enabled: bool,
    timer: u32,
    length: u16,
    volume: u8,
    envelope_timer: u8,
    envelope_period: u8,
    envelope_up: bool,
    /// Pulse duty step, wave position or unused.
    position: u8,
}

/// Register and channel state of one DMG APU.
#[derive(Debug, Clone)]
pub struct GbApu {
    config: GbConfig,
    /// Offsets 0x00..0x2f from 0xFF10; 0x20..0x2f is wave RAM.
    regs: [u8; 0x30],
    power: bool,
    sequencer_counter: u32,
    sequencer_step: u8,
    channels: [Channel; 4],
    sweep_shadow: u16,
    sweep_timer: u8,
    sweep_enabled: bool,
    wave_buffer: u8,
    lfsr: u16,
    /// Channels left out of the output mix (bit c = channel c); chip state is unaffected.
    output_mute: u8,
}

const BASE: [usize; 4] = [0x01, 0x06, 0x0b, 0x10];

impl GbApu {
    /// Declared initial state: powered on, all registers and wave RAM 0,
    /// channels off, the frame sequencer at step 0, LFSR 0.
    pub fn new(config: GbConfig) -> Self {
        Self {
            config,
            regs: [0; 0x30],
            power: true,
            sequencer_counter: 0,
            sequencer_step: 0,
            channels: Default::default(),
            sweep_shadow: 0,
            sweep_timer: 0,
            sweep_enabled: false,
            wave_buffer: 0,
            lfsr: 0,
            output_mute: 0,
        }
    }
    fn period(&self, channel: usize) -> u16 {
        let base = BASE[channel];
        u16::from(self.regs[base + 2]) | (u16::from(self.regs[base + 3] & 7) << 8)
    }
    fn set_period(&mut self, period: u16) {
        self.regs[0x03] = (period & 0xff) as u8;
        self.regs[0x04] = (self.regs[0x04] & !7) | ((period >> 8) & 7) as u8;
    }
    fn dac_on(&self, channel: usize) -> bool {
        match channel {
            2 => self.regs[0x0a] & 0x80 != 0,
            _ => self.regs[BASE[channel] + 1] & 0xf8 != 0,
        }
    }
    fn timer_reload(&self, channel: usize) -> u32 {
        match channel {
            0 | 1 => 2 * (2048 - u32::from(self.period(channel))),
            2 => 2048 - u32::from(self.period(2)),
            _ => {
                let nr43 = self.regs[0x12];
                (NOISE_DIVISORS[usize::from(nr43 & 7)] << (nr43 >> 4)) / TICK_DIVIDER
            }
        }
    }
    /// The sweep calculation with its overflow check.
    fn sweep_next(&mut self) -> u16 {
        let nr10 = self.regs[0x00];
        let delta = self.sweep_shadow >> (nr10 & 7);
        let next = if nr10 & 8 != 0 {
            self.sweep_shadow - delta
        } else {
            self.sweep_shadow + delta
        };
        if next > 2047 {
            self.channels[0].enabled = false;
        }
        next
    }
    fn load_length(&mut self, channel: usize, data: u8) {
        self.channels[channel].length = match channel {
            2 => 256 - u16::from(data),
            _ => 64 - u16::from(data & 63),
        };
    }
    fn trigger(&mut self, channel: usize) {
        let full = if channel == 2 { 256 } else { 64 };
        let reload = self.timer_reload(channel);
        let nrx2 = self.regs[BASE[channel] + 1];
        let ch = &mut self.channels[channel];
        ch.enabled = true;
        if ch.length == 0 {
            ch.length = full;
        }
        ch.timer = reload;
        if channel == 2 {
            ch.position = 0;
        } else {
            ch.volume = nrx2 >> 4;
            ch.envelope_up = nrx2 & 8 != 0;
            ch.envelope_period = nrx2 & 7;
            ch.envelope_timer = ch.envelope_period;
        }
        if channel == 3 {
            self.lfsr = 0;
        }
        if channel == 0 {
            let nr10 = self.regs[0x00];
            self.sweep_shadow = self.period(0);
            self.sweep_timer = match (nr10 >> 4) & 7 {
                0 => 8,
                p => p,
            };
            self.sweep_enabled = nr10 & 0x77 != 0;
            if nr10 & 7 != 0 {
                self.sweep_next();
            }
        }
        if !self.dac_on(channel) {
            self.channels[channel].enabled = false;
        }
    }
    fn power_off(&mut self) {
        let lengths: Vec<u16> = self.channels.iter().map(|c| c.length).collect();
        self.regs[..0x16].fill(0);
        self.channels = Default::default();
        if self.config.dmg_length_while_off {
            for (channel, length) in self.channels.iter_mut().zip(lengths) {
                channel.length = length;
            }
        }
        self.sweep_enabled = false;
        self.power = false;
    }
    fn power_on(&mut self) {
        self.power = true;
        self.sequencer_counter = 0;
        self.sequencer_step = 0;
        self.wave_buffer = 0;
    }
    /// One write to an offset from 0xFF10 (0x00..0x2f).
    pub fn write_register(&mut self, offset: u8, data: u8) -> Value {
        let address = format!("0x{:04X}", 0xff10 + u16::from(offset));
        let Some(name) = register_name(offset) else {
            return json!({"address":address,"offset":offset,"data":data,"ignored":"not a sound register (unused address)"});
        };
        let index = usize::from(offset);
        let before = self.regs[index];
        let mut effect = json!({"address":address,"register":name,"before":before,"data":data});
        if (0x20..0x30).contains(&offset) {
            self.regs[index] = data;
            let n = (index - 0x20) * 2;
            effect["samples"] = json!({"index":[n, n + 1],"values":[data >> 4, data & 15]});
            effect["after"] = json!(data);
            return effect;
        }
        if offset == 0x16 {
            let on = data & 0x80 != 0;
            effect["power"] = json!(if on { "on" } else { "off" });
            match (self.power, on) {
                (true, false) => {
                    self.power_off();
                    effect["power_change"] = json!(
                        "off: NR10..NR51 cleared, channels off, duty steps reset; further writes ignored until power returns"
                    );
                }
                (false, true) => {
                    self.power_on();
                    effect["power_change"] = json!(
                        "on: frame sequencer reset so the next step is 0, wave sample buffer cleared"
                    );
                }
                _ => {}
            }
            self.regs[index] = data & 0x80;
            effect["after"] = json!(self.regs[index]);
            return effect;
        }
        if !self.power {
            let length_register = BASE.iter().position(|&b| b == index);
            if let (true, Some(channel)) = (self.config.dmg_length_while_off, length_register) {
                self.load_length(channel, data);
                effect["after"] = json!(before);
                effect["ignored"] = json!("APU off: only the length timer is loaded (DMG)");
                effect["length_timer"] = json!(self.channels[channel].length);
            } else {
                effect["after"] = json!(before);
                effect["ignored"] = json!("APU off: register writes are ignored");
            }
            return effect;
        }
        self.regs[index] = data;
        effect["after"] = json!(data);
        let channel = BASE
            .iter()
            .rposition(|&b| index >= b && index < b + 4)
            .filter(|_| index < 0x14);
        match (offset, channel) {
            (0x00, _) => {
                effect["sweep_pace"] = json!((data >> 4) & 7);
                effect["sweep_direction"] = json!(if data & 8 != 0 { "subtract" } else { "add" });
                effect["sweep_step"] = json!(data & 7);
            }
            (0x14, _) => {
                effect["left_volume"] = json!((data >> 4) & 7);
                effect["right_volume"] = json!(data & 7);
                effect["vin_left"] = json!(data & 0x80 != 0);
                effect["vin_right"] = json!(data & 8 != 0);
            }
            (0x15, _) => {
                effect["left_channels"] = json!(
                    (0..4)
                        .filter(|c| data & (0x10 << c) != 0)
                        .map(|c| c + 1)
                        .collect::<Vec<_>>()
                );
                effect["right_channels"] = json!(
                    (0..4)
                        .filter(|c| data & (1 << c) != 0)
                        .map(|c| c + 1)
                        .collect::<Vec<_>>()
                );
            }
            (_, Some(channel)) => {
                effect["channel"] = json!(channel + 1);
                match index - BASE[channel] {
                    0 => {
                        if channel < 2 {
                            effect["duty"] = json!(DUTY_NAMES[usize::from(data >> 6)]);
                        }
                        self.load_length(channel, data);
                        effect["length_timer"] = json!(self.channels[channel].length);
                    }
                    1 if channel == 2 => {
                        effect["output_level"] = json!(OUTPUT_LEVELS[usize::from((data >> 5) & 3)])
                    }
                    1 => {
                        effect["initial_volume"] = json!(data >> 4);
                        effect["envelope_direction"] =
                            json!(if data & 8 != 0 { "up" } else { "down" });
                        effect["envelope_pace"] = json!(data & 7);
                        effect["dac_on"] = json!(data & 0xf8 != 0);
                        if data & 0xf8 == 0 {
                            self.channels[channel].enabled = false;
                        }
                    }
                    2 if channel == 3 => {
                        effect["clock_shift"] = json!(data >> 4);
                        effect["lfsr_width"] = json!(if data & 8 != 0 { 7 } else { 15 });
                        effect["divisor_code"] = json!(data & 7);
                    }
                    2 => effect["period"] = json!(self.period(channel)),
                    _ => {
                        if channel < 3 {
                            effect["period"] = json!(self.period(channel));
                        }
                        effect["length_enable"] = json!(data & 0x40 != 0);
                        effect["trigger"] = json!(data & 0x80 != 0);
                        if data & 0x80 != 0 {
                            self.trigger(channel);
                            if !self.dac_on(channel) {
                                effect["trigger_note"] =
                                    json!("the DAC is off, so the channel stays off");
                            }
                        }
                    }
                }
            }
            (0x0a, _) => {
                effect["channel"] = json!(3);
                effect["dac_on"] = json!(data & 0x80 != 0);
                if data & 0x80 == 0 {
                    self.channels[2].enabled = false;
                }
            }
            _ => {}
        }
        effect
    }
    fn clock_lengths(&mut self) {
        for (channel, state) in self.channels.iter_mut().enumerate() {
            let nrx4 = self.regs[BASE[channel] + 3];
            if nrx4 & 0x40 != 0 && state.length > 0 {
                state.length -= 1;
                if state.length == 0 {
                    state.enabled = false;
                }
            }
        }
    }
    fn clock_sweep(&mut self) {
        if self.sweep_timer > 0 {
            self.sweep_timer -= 1;
        }
        if self.sweep_timer != 0 {
            return;
        }
        let nr10 = self.regs[0x00];
        let pace = (nr10 >> 4) & 7;
        self.sweep_timer = if pace == 0 { 8 } else { pace };
        if self.sweep_enabled && pace != 0 {
            let next = self.sweep_next();
            if next <= 2047 && nr10 & 7 != 0 {
                self.sweep_shadow = next;
                self.set_period(next);
                self.sweep_next();
            }
        }
    }
    fn clock_envelopes(&mut self) {
        for channel in [0, 1, 3] {
            let state = &mut self.channels[channel];
            if state.envelope_period == 0 {
                continue;
            }
            if state.envelope_timer > 0 {
                state.envelope_timer -= 1;
            }
            if state.envelope_timer == 0 {
                state.envelope_timer = state.envelope_period;
                if state.envelope_up && state.volume < 15 {
                    state.volume += 1;
                } else if !state.envelope_up && state.volume > 0 {
                    state.volume -= 1;
                }
            }
        }
    }
    fn step_lfsr(&mut self) {
        let bit = !(self.lfsr ^ (self.lfsr >> 1)) & 1;
        self.lfsr = (self.lfsr & 0x7fff) | (bit << 15);
        if self.regs[0x12] & 8 != 0 {
            self.lfsr = (self.lfsr & !0x80) | (bit << 7);
        }
        self.lfsr >>= 1;
    }
    /// Channel status bits as NR52 reports them.
    pub fn active(&self) -> [bool; 4] {
        [0, 1, 2, 3].map(|c| self.channels[c].enabled)
    }
    #[cfg(test)]
    pub(crate) fn channel_volume(&self, channel: usize) -> u8 {
        self.channels[channel].volume
    }
    #[cfg(test)]
    pub(crate) fn lfsr(&self) -> u16 {
        self.lfsr
    }
    /// Digital output 0..15 of a channel before its DAC.
    fn digital(&self, channel: usize) -> u8 {
        let state = &self.channels[channel];
        if !state.enabled {
            return 0;
        }
        match channel {
            0 | 1 => {
                let duty = usize::from(self.regs[BASE[channel]] >> 6);
                DUTY[duty][usize::from(state.position)] * state.volume
            }
            2 => match (self.regs[0x0c] >> 5) & 3 {
                0 => 0,
                level => self.wave_buffer >> (level - 1),
            },
            _ => (self.lfsr & 1) as u8 * state.volume,
        }
    }
}

impl Chip for GbApu {
    const TICKS_PER_FRAME: u32 = 16;
    const OUTPUT_DIVISOR: i64 = 8;
    fn mute_output(&mut self, mask: u8) {
        self.output_mute = mask;
    }
    fn apply(&mut self, target: Target, data: u8) -> Value {
        match target {
            Target::Register(offset) => self.write_register(offset, data),
            _ => json!({"ignored":"not a Game Boy APU port","data":data}),
        }
    }
    /// One tick (clock/2): channel timers, the frame sequencer, then the mix.
    fn tick(&mut self) -> [i32; 2] {
        for channel in 0..4 {
            let shift = self.regs[0x12] >> 4;
            if channel == 3 && shift >= 14 {
                continue;
            }
            let state = &mut self.channels[channel];
            if state.timer > 0 {
                state.timer -= 1;
            }
            if state.timer != 0 {
                continue;
            }
            self.channels[channel].timer = self.timer_reload(channel);
            match channel {
                0 | 1 => {
                    let state = &mut self.channels[channel];
                    state.position = (state.position + 1) & 7;
                }
                2 => {
                    let state = &mut self.channels[2];
                    state.position = (state.position + 1) & 31;
                    let byte = self.regs[0x20 + usize::from(state.position >> 1)];
                    self.wave_buffer = if state.position & 1 == 0 {
                        byte >> 4
                    } else {
                        byte & 15
                    };
                }
                _ => self.step_lfsr(),
            }
        }
        if self.power {
            self.sequencer_counter += 1;
            if self.sequencer_counter == SEQUENCER_TICKS {
                self.sequencer_counter = 0;
                let step = self.sequencer_step;
                self.sequencer_step = (step + 1) & 7;
                if step.is_multiple_of(2) {
                    self.clock_lengths();
                }
                if step == 2 || step == 6 {
                    self.clock_sweep();
                }
                if step == 7 {
                    self.clock_envelopes();
                }
            }
        }
        let (nr50, nr51) = (self.regs[0x14], self.regs[0x15]);
        let mut out = [0i32; 2];
        for channel in 0..4 {
            if !self.dac_on(channel) || self.output_mute & (1 << channel) != 0 {
                continue;
            }
            let analog = (15 - 2 * i32::from(self.digital(channel))) * DAC_STEP;
            if nr51 & (0x10 << channel) != 0 {
                out[0] += analog * (i32::from((nr50 >> 4) & 7) + 1);
            }
            if nr51 & (1 << channel) != 0 {
                out[1] += analog * (i32::from(nr50 & 7) + 1);
            }
        }
        out
    }
    fn state(&self) -> Value {
        json!({"power":self.power,"registers":self.regs[..0x17].iter().map(|r| format!("0x{r:02x}")).collect::<Vec<_>>(),
            "wave_ram":self.regs[0x20..].iter().map(|r| format!("{r:02x}")).collect::<String>(),
            "active":self.active(),"lengths":self.channels.iter().map(|c| c.length).collect::<Vec<_>>(),
            "volumes":[self.channels[0].volume,self.channels[1].volume,self.channels[3].volume],
            "sweep_shadow":self.sweep_shadow,"frame_sequencer_next_step":self.sequencer_step,"lfsr":format!("0x{:04x}",self.lfsr)})
    }
}

/// Tone frequency of an 11-bit period on a pulse (0, 1) or wave (2) channel.
pub fn tone_hz(clock_hz: u32, channel: u8, period: u16) -> f64 {
    let per_cycle = if channel == 2 { 64.0 } else { 32.0 };
    f64::from(clock_hz) / (per_cycle * (2048.0 - f64::from(period)))
}

/// Setup written at tick 0 by note renders: power on, NR50 0x77, NR51 0xFF.
pub const NOTE_SETUP: [(u8, u8); 3] = [(0x16, 0x80), (0x14, 0x77), (0x15, 0xff)];

/// Register writes for Game Boy notes. Channel 0 and 1 are the pulse channels
/// (CH1, CH2), 2 the wave channel (CH3) and 3 the noise channel (CH4).
pub fn note_writes(
    clock_hz: u32,
    notes: &[PsgNote],
) -> Result<(Vec<ChipWrite>, Vec<Value>), (usize, String)> {
    let divider = f64::from(clock_hz) / f64::from(TICK_DIVIDER);
    let mut writes: Vec<(u64, u8, usize, u8, u8)> = Vec::new();
    let mut reports = Vec::new();
    let mut spans: Vec<(u8, u64, u64)> = Vec::new();
    for (order, (offset, data)) in NOTE_SETUP.iter().enumerate() {
        writes.push((0, 0, order, *offset, *data));
    }
    for (index, note) in notes.iter().enumerate() {
        let fail = |message: String| (index, message);
        let max = crate::psg::MAX_RENDER_SECONDS as f64;
        let at = |s: f64| -> Result<u64, (usize, String)> {
            if !(s.is_finite() && (0.0..=max).contains(&s)) {
                return Err((index, format!("times must be 0..{max} seconds")));
            }
            Ok((s * divider).round() as u64)
        };
        if note.channel > 3 {
            return Err(fail(
                "Game Boy channels are 0..3 (CH1 pulse with sweep, CH2 pulse, CH3 wave, CH4 noise)"
                    .into(),
            ));
        }
        if note.attenuation.is_some() {
            return Err(fail(
                "Game Boy notes take volume (0..15), not attenuation".into(),
            ));
        }
        if !(note.duration_seconds.is_finite() && note.duration_seconds > 0.0) {
            return Err(fail("duration_seconds must be positive".into()));
        }
        let start = at(note.start_seconds)?;
        let end = at(note.start_seconds + note.duration_seconds)?;
        if end <= start {
            return Err(fail("the note is shorter than one tick".into()));
        }
        if spans
            .iter()
            .any(|&(c, s, e)| c == note.channel && start < e && s < end)
        {
            return Err(fail(format!(
                "overlaps another note on channel {}",
                note.channel
            )));
        }
        spans.push((note.channel, start, end));
        let mut report = json!({"index":index,"channel":note.channel,"hardware_channel":note.channel + 1,"start_tick":start,"end_tick":end});
        let base = BASE[usize::from(note.channel)] as u8;
        let mut at_start: Vec<(u8, u8)> = Vec::new();
        let envelope_byte = |volume: Option<u8>| -> Result<u8, (usize, String)> {
            let volume = volume
                .filter(|v| *v <= 15)
                .ok_or_else(|| fail("volume must be 0..15".into()))?;
            let (up, pace) = match &note.envelope {
                None => (false, 0),
                Some(e) => {
                    if e.pace > 7 {
                        return Err(fail(
                            "envelope.pace must be 0..7 (64 Hz ticks per step)".into(),
                        ));
                    }
                    (e.direction == EnvelopeDirection::Up, e.pace)
                }
            };
            let byte = (volume << 4) | (u8::from(up) << 3) | pace;
            if byte & 0xf8 == 0 {
                return Err(fail("volume 0 with a falling envelope turns the DAC off; use volume 1..15 or an upward envelope".into()));
            }
            Ok(byte)
        };
        let pitch = |channel: u8| -> Result<(u16, Value), (usize, String)> {
            let Some(pitch) = &note.pitch else {
                return Err(fail("pulse and wave notes take pitch".into()));
            };
            let per_cycle = if channel == 2 { 64.0 } else { 32.0 };
            let (period, report) = match *pitch {
                PsgPitch::Period { period } => (
                    f64::from(period),
                    json!({"source":"period","period":period}),
                ),
                PsgPitch::MidiNote { note, a4_hz } => {
                    if note > 127 || !(a4_hz.is_finite() && (220.0..=880.0).contains(&a4_hz)) {
                        return Err(fail("midi note must be 0..127 and a4_hz 220..880".into()));
                    }
                    let target = a4_hz * ((f64::from(note) - 69.0) / 12.0).exp2();
                    let period = (2048.0 - f64::from(clock_hz) / (per_cycle * target)).round();
                    let cents = if (0.0..=2047.0).contains(&period) {
                        1200.0 * (tone_hz(clock_hz, channel, period as u16) / target).log2()
                    } else {
                        0.0
                    };
                    (
                        period,
                        json!({"source":"midi_note","note":note,"a4_hz":a4_hz,"target_hz":target,"cents_error":cents}),
                    )
                }
            };
            if !(0.0..=2047.0).contains(&period) {
                return Err(fail(format!(
                    "period {period} is outside 0..2047 at this clock"
                )));
            }
            Ok((period as u16, report))
        };
        let release: Vec<(u8, u8)> = match note.channel {
            0 | 1 => {
                if note.noise.is_some() || note.wave.is_some() {
                    return Err(fail(
                        "pulse notes take pitch, volume, optional duty and envelope".into(),
                    ));
                }
                let duty = note.duty.unwrap_or(2);
                if duty > 3 {
                    return Err(fail("duty must be 0..3 (12.5%, 25%, 50%, 75%)".into()));
                }
                let (period, pitch_report) = pitch(note.channel)?;
                report["period"] = json!(period);
                report["frequency_hz"] = json!(tone_hz(clock_hz, note.channel, period));
                report["pitch"] = pitch_report;
                report["duty"] = json!(DUTY_NAMES[usize::from(duty)]);
                let envelope = envelope_byte(note.volume)?;
                if note.channel == 0 {
                    at_start.push((0x00, 0x00));
                }
                at_start.extend([
                    (base, duty << 6),
                    (base + 1, envelope),
                    (base + 2, (period & 0xff) as u8),
                    (base + 3, 0x80 | (period >> 8) as u8),
                ]);
                vec![(base + 1, 0x00)]
            }
            2 => {
                if note.noise.is_some()
                    || note.volume.is_some()
                    || note.duty.is_some()
                    || note.envelope.is_some()
                {
                    return Err(fail(
                        "wave notes take pitch and wave (32 samples, output_level)".into(),
                    ));
                }
                let wave = note.wave.as_ref().ok_or_else(|| {
                    fail("wave notes need wave.samples and wave.output_level".into())
                })?;
                if wave.samples.len() != 32 || wave.samples.iter().any(|&s| s > 15) {
                    return Err(fail("wave.samples must be 32 values 0..15".into()));
                }
                if !(1..=3).contains(&wave.output_level) {
                    return Err(fail(
                        "wave.output_level must be 1 (100%), 2 (50%) or 3 (25%)".into(),
                    ));
                }
                let (period, pitch_report) = pitch(2)?;
                report["period"] = json!(period);
                report["frequency_hz"] = json!(tone_hz(clock_hz, 2, period));
                report["pitch"] = pitch_report;
                report["output_level"] = json!(OUTPUT_LEVELS[usize::from(wave.output_level)]);
                at_start.push((0x0a, 0x00));
                for (i, pair) in wave.samples.chunks(2).enumerate() {
                    at_start.push((0x20 + i as u8, (pair[0] << 4) | pair[1]));
                }
                at_start.extend([
                    (0x0a, 0x80),
                    (0x0b, 0x00),
                    (0x0c, wave.output_level << 5),
                    (0x0d, (period & 0xff) as u8),
                    (0x0e, 0x80 | (period >> 8) as u8),
                ]);
                vec![(0x0a, 0x00)]
            }
            _ => {
                if note.pitch.is_some() || note.wave.is_some() || note.duty.is_some() {
                    return Err(fail("noise notes take noise (mode, divisor, shift), volume and optional envelope".into()));
                }
                let noise = note
                    .noise
                    .as_ref()
                    .ok_or_else(|| fail("noise notes need noise".into()))?;
                if noise.rate.is_some() || noise.period.is_some() {
                    return Err(fail(
                        "Game Boy noise takes mode, divisor (0..7) and shift (0..13)".into(),
                    ));
                }
                let (Some(mode), Some(divisor), Some(shift)) =
                    (noise.mode, noise.divisor, noise.shift)
                else {
                    return Err(fail("Game Boy noise needs mode, divisor and shift".into()));
                };
                if divisor > 7 || shift > 13 {
                    return Err(fail(
                        "noise.divisor must be 0..7 and noise.shift 0..13".into(),
                    ));
                }
                let width = u8::from(mode == crate::psg::NoiseMode::Periodic);
                let r = if divisor == 0 {
                    0.5
                } else {
                    f64::from(divisor)
                };
                report["noise"] = json!({"mode":mode,"lfsr_width":if width == 1 { 7 } else { 15 },"divisor":divisor,"shift":shift,
                    "shift_rate_hz":f64::from(clock_hz) / 16.0 / (r * f64::from(1u32 << shift))});
                let envelope = envelope_byte(note.volume)?;
                at_start.extend([
                    (0x10, 0x00),
                    (0x11, envelope),
                    (0x12, (shift << 4) | (width << 3) | divisor),
                    (0x13, 0x80),
                ]);
                vec![(0x11, 0x00)]
            }
        };
        report["volume"] = json!(note.volume);
        let show = |w: &[(u8, u8)]| {
            w.iter()
                .map(|(o, d)| json!({"register":register_name(*o),"offset":o,"data":d}))
                .collect::<Vec<_>>()
        };
        report["writes"] = json!(show(&at_start));
        report["release_writes"] = json!(show(&release));
        for (offset, data) in at_start {
            writes.push((start, 2, index, offset, data));
        }
        for (offset, data) in release {
            writes.push((end, 1, index, offset, data));
        }
        reports.push(report);
    }
    // At one tick: setup, then releases, then starts; note order within each.
    writes.sort_by_key(|&(tick, class, index, _, _)| (tick, class, index));
    Ok((
        writes
            .into_iter()
            .map(|(tick, _, _, offset, data)| ChipWrite {
                tick,
                target: Target::Register(offset),
                data,
            })
            .collect(),
        reports,
    ))
}

/// A Game Boy volume envelope for notes (NRx2 direction and pace).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GbEnvelope {
    pub direction: EnvelopeDirection,
    /// 64 Hz ticks per volume step, 1..7; 0 holds the volume.
    pub pace: u8,
}

/// Wave channel contents for a Game Boy wave note.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GbWave {
    /// 32 4-bit samples (0..15) in playback order.
    pub samples: Vec<u8>,
    /// NR32 output level: 1 = 100%, 2 = 50%, 3 = 25%.
    pub output_level: u8,
}

/// Envelope direction.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeDirection {
    Up,
    Down,
}

pub fn model_scope() -> Value {
    json!({
        "renderer": "Tunemend Game Boy DMG APU model; not original hardware audio or driver playback",
        "sources": ["Pan Docs (gbdev.io): Audio, Audio Registers, Audio details", "VGM 1.71 specification (GameBoy DMG clock and command 0xB3)"],
        "modelled": ["pulse channels 1 and 2: duty tables 12.5/25/50/75%, period divider at clock/4 (1048576 Hz), frequency 131072/(2048-x) at 4194304 Hz",
            "channel 1 sweep: pace (128 Hz), direction, step; overflow above 2047 disables the channel, also on trigger and in the check after a write-back",
            "length timers: 64 steps (256 for the wave channel) clocked at 256 Hz when NRx4 bit 6 is set; expiry disables the channel",
            "volume envelopes at 64 Hz with initial volume, direction and pace latched at trigger",
            "wave channel: 32 4-bit samples from wave RAM (high nibble first), period divider at clock/2, frequency 65536/(2048-x), output level mute/100%/50%/25%; a trigger starts from sample 1 and plays the buffered sample until the first read",
            "noise channel: XNOR LFSR of bits 0 and 1 into bit 15 (and bit 7 in 7-bit mode), reset to 0 on trigger, bit 0 selects the volume; period divisor<<shift input clocks; shifts 14 and 15 stop it",
            "frame sequencer every 8192 input clocks (512 Hz): length on even steps, sweep on steps 2 and 6, envelope on step 7",
            "DAC enables: NRx2 bits 3..7 for channels 1, 2 and 4, NR30 bit 7 for channel 3; a DAC turned off disables its channel; DAC output digital 0..15 to +1..-1, 0 when off",
            "NR51 per-side routing and NR50 per-side volume (v+1)/8",
            "NR52 power: off clears NR10..NR51, disables channels and resets duty steps; writes are ignored while off except NR52, wave RAM and (DMG) length timers; on resets the frame sequencer to step 0"],
        "declared_choices": ["initial state: powered on, all registers and wave RAM 0, channels off, frame sequencer at step 0 with its first event after 8192 input clocks, LFSR 0",
            "native samples average 16 ticks (clock/32, 131072 Hz at 4194304 Hz)",
            "outputs are the DAC levels before the output high-pass filter, so enabled DACs carry their DC level",
            "channel timers run while a channel is off; duty position and LFSR are not reset by triggers except as documented",
            "wave RAM writes reach the addressed byte even while channel 3 plays",
            "writes take effect before the tick of their time, in order"],
        "not_modelled": ["the output high-pass filter and analog stage", "the DAC fade when a DAC is turned off", "VIN cartridge audio (NR50 bits 3 and 7)",
            "CGB differences: length timers cleared at power-off and not writable while off, CGB wave RAM access while playing, PCM12/PCM34 registers",
            "DMG wave RAM corruption on retrigger and wave RAM access quirks while playing", "the extra length clock when enabling length in the first half of a sequencer period",
            "envelope writes while a channel plays (zombie mode)", "the sweep negate-mode disable quirk", "trigger timing delays of a few clocks",
            "a second chip in dual-chip VGM files is rendered as an independent APU", "write timing inside one clock/2 tick"],
        "equality": "Equal register writes and clock do not establish that a game, driver or hardware unit sounds the same."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mixed() -> GbApu {
        let mut apu = GbApu::new(GbConfig::dmg());
        apu.write_register(0x14, 0x77);
        apu.write_register(0x15, 0xff);
        apu
    }

    fn run(apu: &mut GbApu, ticks: u32) {
        for _ in 0..ticks {
            apu.tick();
        }
    }

    #[test]
    fn pulse_duty_waveforms_and_frequency() {
        for duty in 0..4u8 {
            let mut apu = mixed();
            let period = 1750u16;
            apu.write_register(0x06, duty << 6);
            apu.write_register(0x07, 0xf0);
            apu.write_register(0x08, (period & 0xff) as u8);
            apu.write_register(0x09, 0x80 | (period >> 8) as u8);
            // Each duty step lasts 2*(2048-x) ticks of clock/2, i.e. 4*(2048-x) input clocks.
            let step = 2 * (2048 - u32::from(period));
            let mut pattern = Vec::new();
            for _ in 0..16 {
                run(&mut apu, step - 1);
                let out = apu.tick()[0];
                pattern.push(apu.channels[1].position);
                assert!(out == -8190 * 8 || out == 8190 * 8, "{out}");
                pattern.push(u8::from(out < 0));
            }
            let highs: Vec<u8> = pattern.chunks(2).map(|p| p[1]).collect();
            let positions: Vec<u8> = pattern.chunks(2).map(|p| p[0]).collect();
            let expected: Vec<u8> = positions
                .iter()
                .map(|&p| DUTY[usize::from(duty)][usize::from(p)])
                .collect();
            assert_eq!(highs, expected, "duty {duty}");
            assert_eq!(
                highs.iter().filter(|&&h| h == 1).count(),
                2 * [1, 2, 4, 6][usize::from(duty)]
            );
        }
        // Eight duty steps per cycle: 131072/(2048-x) Hz at 4194304 Hz.
        let hz = f64::from(DMG_CLOCK_HZ) / f64::from(TICK_DIVIDER) / (8.0 * 2.0 * 298.0);
        assert!((hz - 131072.0 / 298.0).abs() < 1e-9);
        assert!((tone_hz(DMG_CLOCK_HZ, 0, 1750) - 131072.0 / 298.0).abs() < 1e-9);
        assert!((tone_hz(DMG_CLOCK_HZ, 2, 1750) - 65536.0 / 298.0).abs() < 1e-9);
    }

    #[test]
    fn sweep_overflow_disables_channel_1() {
        // On trigger: 0x700 + (0x700 >> 1) overflows at once.
        let mut apu = mixed();
        apu.write_register(0x00, 0x11);
        apu.write_register(0x02, 0xf0);
        apu.write_register(0x03, 0x00);
        apu.write_register(0x04, 0x87);
        assert!(!apu.active()[0]);
        // 0x500 + 0x280 = 0x780 fits; the first sweep clock (step 2) writes it
        // back and the following check (0x780 + 0x3c0) overflows.
        let mut apu = mixed();
        apu.write_register(0x00, 0x11);
        apu.write_register(0x02, 0xf0);
        apu.write_register(0x03, 0x00);
        apu.write_register(0x04, 0x85);
        assert!(apu.active()[0]);
        run(&mut apu, 3 * SEQUENCER_TICKS - 1);
        assert!(apu.active()[0]);
        apu.tick();
        assert!(!apu.active()[0]);
        assert_eq!(apu.period(0), 0x780);
        // Subtraction never overflows and moves the period down.
        let mut apu = mixed();
        apu.write_register(0x00, 0x19);
        apu.write_register(0x02, 0xf0);
        apu.write_register(0x03, 0x00);
        apu.write_register(0x04, 0x84);
        run(&mut apu, 3 * SEQUENCER_TICKS);
        assert_eq!(apu.period(0), 0x400 - 0x200);
        assert!(apu.active()[0]);
    }

    #[test]
    fn length_counter_expires_on_256_hz_steps() {
        let mut apu = mixed();
        apu.write_register(0x07, 0xf0);
        // Length data 60: 4 steps, clocked on sequencer steps 0, 2, 4 and 6.
        apu.write_register(0x06, 60);
        apu.write_register(0x09, 0xc0);
        run(&mut apu, 7 * SEQUENCER_TICKS - 1);
        assert!(apu.active()[1]);
        apu.tick();
        assert!(!apu.active()[1]);
        // Without length enable the channel keeps playing.
        let mut apu = mixed();
        apu.write_register(0x07, 0xf0);
        apu.write_register(0x06, 63);
        apu.write_register(0x09, 0x80);
        run(&mut apu, 16 * SEQUENCER_TICKS);
        assert!(apu.active()[1]);
        // The wave channel counts 256.
        let mut apu = mixed();
        apu.write_register(0x0a, 0x80);
        apu.write_register(0x0b, 254);
        apu.write_register(0x0e, 0xc0);
        run(&mut apu, 3 * SEQUENCER_TICKS - 1);
        assert!(apu.active()[2]);
        apu.tick();
        assert!(!apu.active()[2]);
    }

    #[test]
    fn envelope_steps_at_64_hz() {
        let mut apu = mixed();
        apu.write_register(0x07, 0xf1);
        apu.write_register(0x09, 0x80);
        let mut volumes = vec![apu.channel_volume(1)];
        for _ in 0..4 {
            run(&mut apu, 8 * SEQUENCER_TICKS);
            volumes.push(apu.channel_volume(1));
        }
        assert_eq!(volumes, [15, 14, 13, 12, 11]);
        // The step falls on sequencer step 7: tick 8 * 4096.
        let mut apu = mixed();
        apu.write_register(0x11, 0x0a);
        apu.write_register(0x13, 0x80);
        run(&mut apu, 8 * SEQUENCER_TICKS - 1);
        assert_eq!(apu.channel_volume(3), 0);
        apu.tick();
        assert_eq!(apu.channel_volume(3), 0);
        run(&mut apu, 8 * SEQUENCER_TICKS);
        assert_eq!(apu.channel_volume(3), 1);
    }

    #[test]
    fn wave_channel_plays_wave_ram_with_output_shifts() {
        for (level, shift) in [(1u8, 0u8), (2, 1), (3, 2)] {
            let mut apu = mixed();
            for i in 0..8u8 {
                apu.write_register(0x20 + i, ((2 * i) << 4) | (2 * i + 1));
            }
            apu.write_register(0x0a, 0x80);
            apu.write_register(0x0c, level << 5);
            let period = 2000u16;
            apu.write_register(0x0d, (period & 0xff) as u8);
            apu.write_register(0x0e, 0x80 | (period >> 8) as u8);
            let mut samples = Vec::new();
            for _ in 0..8 {
                run(&mut apu, 2048 - u32::from(period) - 1);
                apu.tick();
                samples.push(apu.digital(2));
            }
            // The first read is sample 1 (low nibble of byte 0).
            let expected: Vec<u8> = (1..9u8).map(|n| n >> shift).collect();
            assert_eq!(samples, expected, "level {level}");
        }
        let mut apu = mixed();
        apu.write_register(0x20, 0xf0);
        apu.write_register(0x0a, 0x80);
        apu.write_register(0x0e, 0x80);
        run(&mut apu, 4096);
        assert_eq!(apu.digital(2), 0, "mute level");
    }

    #[test]
    fn noise_lfsr_first_outputs_in_both_widths() {
        // From LFSR 0, 15-bit mode: XNOR(0,0) = 1 fills from the top; bit 0 is
        // 0 for 14 shifts and 1 from the 15th (hand trace of the Pan Docs rule).
        for (width, expected) in [
            (0u8, "000000000000001111111111111101"),
            (8, "000000111111011111001111010111"),
        ] {
            let mut apu = mixed();
            apu.write_register(0x11, 0xf0);
            apu.write_register(0x12, width);
            apu.write_register(0x13, 0x80);
            // Divisor code 0, shift 0: one shift every 8 input clocks (4 ticks).
            let mut bits = String::new();
            // Independent formulation: XOR feedback from all ones, output inverted.
            let (mut reference, mut reference_bits) = (0x7fffu16, String::new());
            while bits.len() < 30 {
                run(&mut apu, 4);
                bits.push(if apu.lfsr() & 1 != 0 { '1' } else { '0' });
                let bit = (reference ^ (reference >> 1)) & 1;
                reference = (reference >> 1) | (bit << 14);
                if width != 0 {
                    reference = (reference & !(1 << 6)) | (bit << 6);
                }
                reference_bits.push(if reference & 1 == 0 { '1' } else { '0' });
            }
            assert_eq!(bits, expected, "width bit {width}");
            assert_eq!(bits, reference_bits);
        }
        // Divisor 0 counts as 8 input clocks: a shift every 4 ticks at shift 0.
        let mut apu = mixed();
        apu.write_register(0x11, 0xf0);
        apu.write_register(0x13, 0x80);
        run(&mut apu, 3);
        assert_eq!(apu.lfsr(), 0);
        apu.tick();
        assert_eq!(apu.lfsr(), 0x4000);
        // Shift 14 stops the LFSR.
        apu.write_register(0x12, 0xe0);
        let before = apu.lfsr();
        run(&mut apu, 1000);
        assert_eq!(apu.lfsr(), before);
    }

    #[test]
    fn frame_sequencer_runs_at_512_hz() {
        assert_eq!(DMG_CLOCK_HZ / TICK_DIVIDER / SEQUENCER_TICKS, 512);
        let mut apu = mixed();
        let mut steps = Vec::new();
        for tick in 1..=8 * SEQUENCER_TICKS {
            let before = apu.sequencer_step;
            apu.tick();
            if apu.sequencer_step != before {
                steps.push((tick, before));
            }
        }
        let expected: Vec<(u32, u8)> = (0..8)
            .map(|s| ((s + 1) * SEQUENCER_TICKS, s as u8))
            .collect();
        assert_eq!(steps, expected);
    }

    #[test]
    fn power_off_clears_registers_and_ignores_writes() {
        let mut apu = mixed();
        apu.write_register(0x07, 0xf0);
        apu.write_register(0x06, 50);
        apu.write_register(0x09, 0xc0);
        apu.write_register(0x20, 0x5a);
        assert!(apu.active()[1]);
        let off = apu.write_register(0x16, 0x00);
        assert!(off["power_change"].as_str().unwrap().starts_with("off"));
        assert!(apu.regs[..0x16].iter().all(|&r| r == 0));
        assert_eq!(apu.active(), [false; 4]);
        assert_eq!(apu.tick(), [0, 0]);
        // Writes are ignored, wave RAM stays writable, DMG length timers load.
        assert!(apu.write_register(0x07, 0xf0)["ignored"].is_string());
        assert_eq!(apu.regs[0x07], 0);
        apu.write_register(0x21, 0x77);
        assert_eq!((apu.regs[0x20], apu.regs[0x21]), (0x5a, 0x77));
        assert_eq!(apu.channels[1].length, 14);
        apu.write_register(0x06, 60);
        assert_eq!(apu.channels[1].length, 4);
        // Power on: the sequencer restarts at step 0.
        run(&mut apu, 100);
        apu.write_register(0x16, 0x80);
        assert_eq!((apu.sequencer_counter, apu.sequencer_step), (0, 0));
        apu.write_register(0x07, 0xf0);
        assert_eq!(apu.regs[0x07], 0xf0);
    }
}
