//! HuC6280 programmable sound generator (PC Engine, TurboGrafx-16): six
//! channels of 32-word, 5-bit waveform memory with 12-bit frequency dividers,
//! channel, balance and main amplitude levels on a logarithmic scale, direct
//! D/A (DDA) mode, noise on channels 5 and 6, and the low-frequency
//! oscillator in which channel 2 modulates channel 1's frequency.
//!
//! References:
//! Hudson Soft's "HuC6280 CMOS Programmable Sound Generator Manual" (register
//! functions R0-R9, the chON/DDA mode table, the amplitude levels in 1.5 dB
//! and 3 dB steps with the 45 dB cut-off and its worked example, the output,
//! LFO and noise frequency formulas, the LFO modulation table and the LF CTL
//! shifts), Paul Clifford's "PC Engine Programmable Sound Generator" notes
//! (register behaviour, the noise as a random square wave) and the VGM 1.71
//! specification (HuC6280 clock 0xA4, command 0xB9). Behaviour the documents
//! leave open is a declared choice in [`model_scope`].
use crate::psg::{PsgNote, PsgPitch};
use crate::sound_chip::{Chip, ChipWrite, Target};
use serde_json::{Value, json};

/// The VGM header's typical clock: the 7.16 MHz master clock divided by 2.
pub const CLOCK_HZ: u32 = 3_579_545;
pub const MIN_CLOCK_HZ: u32 = 1_000_000;
pub const MAX_CLOCK_HZ: u32 = 8_000_000;
/// One model tick is one cycle of the header clock.
pub const TICK_DIVIDER: u32 = 1;
/// Ticks averaged into one native output sample (clock / 32, 111860.8 Hz).
pub const FRAME_TICKS: u32 = 32;
const OUTPUT_SUBSTEPS: i64 = 8;
/// PCM16 units of one channel's full-scale word (31) at 0 dB: six channels
/// at full level fit on each side.
const LEVEL: f64 = 32767.0 / (6.0 * 31.0);
pub const CHANNELS: usize = 6;

const REGISTER_NAMES: [&str; 10] = [
    "channel_select",
    "main_amplitude",
    "frequency_low",
    "frequency_high",
    "channel_on_dda_amplitude",
    "lr_amplitude",
    "waveform",
    "noise",
    "lfo_frequency",
    "lfo_control",
];

/// Register name of R0..R9; other offsets are not PSG registers.
pub fn register_name(register: u8) -> &'static str {
    REGISTER_NAMES
        .get(usize::from(register))
        .copied()
        .unwrap_or("undefined")
}

/// The LF CTL shifts the manual gives: 0 disabled, then 0, 2 and 4 bits.
pub const LFO_SHIFTS: [u32; 3] = [0, 2, 4];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PceConfig {
    pub clock_hz: u32,
}

impl PceConfig {
    pub fn new(clock_hz: u32) -> Self {
        Self { clock_hz }
    }
    pub fn to_json(&self) -> Value {
        json!({"variant":"huc6280","clock_hz":self.clock_hz,
            "tick":"one cycle of the header clock (the 7.16 MHz master clock / 2 on the console)",
            "native_sample":format!("mean of {FRAME_TICKS} ticks (clock/{FRAME_TICKS})"),
            "waveform_frequency":"clock / (32 F), F the 12-bit R3:R2 value; F = 0 counts as 4096",
            "noise":"a new random level (word 31 or 0) every 32 NF ticks, two per noise clock period clock / (64 NF); NF = 31 - R7 bits 4..0, 0 counts as 32",
            "lfo":"channel 2 steps every F2 * FLF ticks (FLF = R8, 0 counts as 256); channel 1's divider becomes F1 + ((word - 16) << shift), shift 0, 2 or 4 for LF CTL 1..3, kept to 12 bits",
            "amplitude":"attenuation 3 dB per step below 15 of R1 and R5 and 1.5 dB per step below 31 of AL; silent when (15 - main) + (15 - AL >> 1) + (15 - balance) reaches 15 (45 dB)",
            "output":"unipolar: each sounding channel adds word * amplitude * 32767 / 186 per side, DC kept",
            "power_on_state":"registers 0, channels off, address counters 0, waveform memory 0, noise LFSR 1, LFO off"})
    }
}

pub fn check_clock(clock_hz: u32) -> Result<(), String> {
    if (MIN_CLOCK_HZ..=MAX_CLOCK_HZ).contains(&clock_hz) {
        Ok(())
    } else {
        Err(format!(
            "clock_hz must be {MIN_CLOCK_HZ}..{MAX_CLOCK_HZ}, such as {CLOCK_HZ} (the PC Engine's 7.16 MHz master clock / 2, as VGM files give it)"
        ))
    }
}

/// Waveform frequency f = clock / (32 F) for the 12-bit divider F (0 = 4096).
pub fn tone_hz(clock_hz: u32, divider: u16) -> f64 {
    f64::from(clock_hz) / (32.0 * f64::from(period(divider)))
}

fn period(divider: u16) -> u32 {
    match u32::from(divider & 0xfff) {
        0 => 4096,
        f => f,
    }
}

/// NF = 31 - the 5-bit noise frequency field; 0 counts as 32.
fn noise_period(control: u8) -> u32 {
    match 31 - u32::from(control & 0x1f) {
        0 => 32,
        nf => nf,
    }
}

/// Amplitude factor and attenuation in 1.5 dB units for one side: main
/// level, channel level AL and balance; `None` when the channel is silent.
pub fn amplitude(main: u8, al: u8, balance: u8) -> Option<(f64, u32)> {
    let (main, al, balance) = (
        u32::from(main & 15),
        u32::from(al & 31),
        u32::from(balance & 15),
    );
    let steps_3db = (15 - main) + (15 - (al >> 1)) + (15 - balance);
    if main == 0 || al == 0 || balance == 0 || steps_3db >= 15 {
        return None;
    }
    let half_steps = 2 * (15 - main) + (31 - al) + 2 * (15 - balance);
    Some((10f64.powf(-1.5 * f64::from(half_steps) / 20.0), half_steps))
}

#[derive(Debug, Clone)]
struct Channel {
    divider: u16,
    control: u8,
    balance: u8,
    wave: [u8; 32],
    address: u8,
    counter: u32,
    dda: u8,
    noise: u8,
    lfsr: u16,
    noise_counter: u32,
    gain: [f64; 2],
    wave_writes: u64,
    dda_writes: u64,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            divider: 0,
            control: 0,
            balance: 0,
            wave: [0; 32],
            address: 0,
            counter: 4096,
            dda: 0,
            noise: 0,
            lfsr: 1,
            noise_counter: 32 * 32,
            gain: [0.0; 2],
            wave_writes: 0,
            dda_writes: 0,
        }
    }
}

impl Channel {
    fn on(&self) -> bool {
        self.control & 0x80 != 0
    }
    fn dda_mode(&self) -> bool {
        self.control & 0x40 != 0
    }
    /// chON set and DDA clear: the waveform memory plays.
    fn mixing(&self) -> bool {
        self.control & 0xc0 == 0x80
    }
    fn mode(&self) -> &'static str {
        match self.control >> 6 {
            0 => "write data",
            1 => "reset counter",
            2 => "mixing (sound output)",
            _ => "direct D/A",
        }
    }
    fn clock_noise(&mut self) {
        self.noise_counter -= 1;
        if self.noise_counter == 0 {
            self.noise_counter = 32 * noise_period(self.noise);
            let bit = (self.lfsr ^ (self.lfsr >> 1)) & 1;
            self.lfsr = (self.lfsr >> 1) | (bit << 14);
        }
    }
}

#[derive(Debug, Clone)]
pub struct Huc6280 {
    config: PceConfig,
    select: u8,
    main: u8,
    lfo_frequency: u8,
    lfo_control: u8,
    lfo_counter: u32,
    channels: [Channel; CHANNELS],
    cycles: u64,
    /// Channels left out of the output mix (bit c = 0-based channel c); chip
    /// state is unaffected.
    output_mute: u8,
}

impl Huc6280 {
    /// The declared power-on state.
    pub fn new(config: PceConfig) -> Self {
        Self {
            config,
            select: 0,
            main: 0,
            lfo_frequency: 0,
            lfo_control: 0,
            lfo_counter: 256,
            channels: Default::default(),
            cycles: 0,
            output_mute: 0,
        }
    }

    fn lfo_steps(&self) -> u32 {
        match u32::from(self.lfo_frequency) {
            0 => 256,
            f => f,
        }
    }

    /// LF CTL nonzero with channels 1 and 2 both in mixing mode.
    fn lfo_active(&self) -> bool {
        self.lfo_control & 3 != 0 && self.channels[0].mixing() && self.channels[1].mixing()
    }

    /// Channel 2's current word as a modulation value (word - 16, -16..15).
    fn modulation(&self) -> i32 {
        i32::from(self.channels[1].wave[usize::from(self.channels[1].address)]) - 16
    }

    /// Channel 1's divider after modulation, kept to 12 bits (0 = 4096).
    fn modulated_period(&self) -> u32 {
        if !self.lfo_active() {
            return period(self.channels[0].divider);
        }
        let shift = LFO_SHIFTS[usize::from(self.lfo_control & 3) - 1];
        let f = i32::from(self.channels[0].divider) + (self.modulation() << shift);
        period((f & 0xfff) as u16)
    }

    fn update_gain(&mut self, index: usize) {
        let main = [self.main >> 4, self.main & 15];
        let channel = &mut self.channels[index];
        let balance = [channel.balance >> 4, channel.balance & 15];
        for side in 0..2 {
            channel.gain[side] = amplitude(main[side], channel.control & 31, balance[side])
                .map_or(0.0, |(gain, _)| gain * LEVEL);
        }
    }

    fn side_json(main: u8, al: u8, balance: u8) -> Value {
        match amplitude(main, al, balance) {
            Some((gain, half)) => {
                json!({"attenuation_db":1.5 * f64::from(half),"gain":gain})
            }
            None => json!({"silent":true}),
        }
    }

    /// One write to PSG register R0..R9 (offsets 0x0A..0x0F are not registers).
    pub fn write_register(&mut self, register: u8, data: u8) -> Value {
        let address = format!("${:04X}", 0x0800 + u16::from(register));
        if register > 9 {
            return json!({"address":address,"data":data,"ignored":"not a HuC6280 PSG register (R0..R9 are $0800..$0809)"});
        }
        let mut effect = json!({"address":address,"register":register_name(register),"data":data});
        let index = usize::from(self.select);
        if (2..=7).contains(&register) {
            if index >= CHANNELS {
                effect["ignored"] = json!(format!(
                    "R0 selects channel value {index}, which does not exist (0..5 are channels 1..6)"
                ));
                return effect;
            }
            effect["channel"] = json!(index + 1);
        }
        match register {
            0 => {
                effect["before"] = json!(self.select);
                self.select = data & 7;
                effect["channel"] = json!(usize::from(self.select) + 1);
                if self.select > 5 {
                    effect["note"] = json!(
                        "channel values 6 and 7 select no channel; R2..R7 writes are ignored until R0 changes"
                    );
                }
            }
            1 => {
                effect["before"] = json!(self.main);
                self.main = data;
                effect["left"] = json!(data >> 4);
                effect["right"] = json!(data & 15);
                for i in 0..CHANNELS {
                    self.update_gain(i);
                }
            }
            2 | 3 => {
                let clock = self.config.clock_hz;
                let channel = &mut self.channels[index];
                let before = channel.divider;
                channel.divider = if register == 2 {
                    (before & 0xf00) | u16::from(data)
                } else {
                    (before & 0x0ff) | (u16::from(data & 15) << 8)
                };
                effect["before"] = json!(before);
                effect["divider"] = json!(channel.divider);
                effect["frequency_hz"] = json!(tone_hz(clock, channel.divider));
                if channel.divider == 0 {
                    effect["note"] =
                        json!("F = 0 is forbidden by the manual; the model counts it as 4096");
                }
            }
            4 => {
                let channel = &mut self.channels[index];
                effect["before"] = json!(channel.control);
                let was_mixing = channel.mixing();
                channel.control = data;
                if channel.mixing() && !was_mixing {
                    channel.counter = period(channel.divider);
                }
                if channel.dda_mode() {
                    channel.address = 0;
                    effect["address_counter_reset"] = json!(true);
                }
                effect["mode"] = json!(channel.mode());
                effect["channel_on"] = json!(channel.on());
                effect["dda"] = json!(channel.dda_mode());
                effect["amplitude_level"] = json!(data & 31);
                self.update_gain(index);
                let channel = &self.channels[index];
                effect["left"] = Self::side_json(self.main >> 4, data & 31, channel.balance >> 4);
                effect["right"] = Self::side_json(self.main & 15, data & 31, channel.balance & 15);
            }
            5 => {
                let channel = &mut self.channels[index];
                effect["before"] = json!(channel.balance);
                channel.balance = data;
                self.update_gain(index);
                let channel = &self.channels[index];
                effect["left"] = Self::side_json(self.main >> 4, channel.control & 31, data >> 4);
                effect["right"] = Self::side_json(self.main & 15, channel.control & 31, data & 15);
            }
            6 => {
                let channel = &mut self.channels[index];
                let value = data & 31;
                match channel.control >> 6 {
                    0 => {
                        let at = channel.address;
                        channel.wave[usize::from(at)] = value;
                        channel.address = (at + 1) & 31;
                        channel.wave_writes += 1;
                        effect["waveform_write"] = json!({"word_address":at,"value":value});
                    }
                    2 => {
                        effect["ignored"] = json!(
                            "the channel is in mixing mode (chON 1, DDA 0): the write does not change waveform memory (declared)"
                        );
                    }
                    _ => {
                        channel.dda = value;
                        channel.dda_writes += 1;
                        effect["dda_level"] = json!(value);
                        if !channel.on() {
                            effect["note"] = json!(
                                "reset counter mode: the level is held for the D/A converter and sounds once chON is set"
                            );
                        }
                    }
                }
            }
            7 => {
                if index < 4 {
                    effect["ignored"] = json!("R7 exists for channels 5 and 6 only");
                    return effect;
                }
                let clock = self.config.clock_hz;
                let channel = &mut self.channels[index];
                effect["before"] = json!(channel.noise);
                channel.noise = data;
                channel.noise_counter = 32 * noise_period(data);
                let nf = noise_period(data);
                effect["noise_enable"] = json!(data & 0x80 != 0);
                effect["frequency_field"] = json!(data & 31);
                effect["nf"] = json!(nf);
                effect["noise_clock_hz"] = json!(f64::from(clock) / (64.0 * f64::from(nf)));
                effect["level_changes_hz"] = json!(f64::from(clock) / (32.0 * f64::from(nf)));
                if data & 31 == 31 {
                    effect["note"] =
                        json!("NF = 0 is undefined in the manual; the model counts it as 32");
                }
            }
            8 => {
                effect["before"] = json!(self.lfo_frequency);
                self.lfo_frequency = data;
                self.lfo_counter = self.lfo_steps();
                effect["lfo_frequency"] = json!(self.lfo_steps());
            }
            _ => {
                effect["before"] = json!(self.lfo_control);
                self.lfo_control = data;
                self.lfo_counter = self.lfo_steps();
                let control = data & 3;
                effect["trigger"] = json!(data & 0x80 != 0);
                effect["lf_ctl"] = json!(control);
                effect["modulation"] = json!(match control {
                    0 => "LFO disabled".to_owned(),
                    c => format!(
                        "word - 16 shifted left {} bits added to channel 1's divider",
                        LFO_SHIFTS[usize::from(c) - 1]
                    ),
                });
                if data & 0x80 != 0 {
                    self.channels[1].address = 0;
                    self.channels[1].counter = period(self.channels[1].divider);
                    effect["note"] = json!(
                        "LF TRG set: channel 2's address counter is reset to 0 and held; modulation uses word 0 while LF CTL is nonzero"
                    );
                }
            }
        }
        effect
    }

    /// One tick: channel dividers, the LFO and the noise generators.
    fn step(&mut self) {
        self.cycles += 1;
        let lfo = self.lfo_active();
        // Channel 1 reloads with the modulated divider in effect at its reload.
        let ch1_period = self.modulated_period();
        for (i, channel) in self.channels.iter_mut().enumerate() {
            if i >= 4 && channel.noise & 0x80 != 0 {
                channel.clock_noise();
            }
            if !channel.mixing() {
                continue;
            }
            if i == 1 && lfo && self.lfo_control & 0x80 != 0 {
                continue;
            }
            channel.counter -= 1;
            if channel.counter > 0 {
                continue;
            }
            channel.counter = if i == 0 {
                ch1_period
            } else {
                period(channel.divider)
            };
            if i == 1 && lfo {
                self.lfo_counter -= 1;
                if self.lfo_counter > 0 {
                    continue;
                }
                self.lfo_counter = match u32::from(self.lfo_frequency) {
                    0 => 256,
                    f => f,
                };
            }
            channel.address = (channel.address + 1) & 31;
        }
    }

    /// Each channel's current 5-bit output word (None when it does not sound).
    pub fn words(&self) -> [Option<u8>; CHANNELS] {
        let mut out = [None; CHANNELS];
        for (i, channel) in self.channels.iter().enumerate() {
            if !channel.on() {
                continue;
            }
            out[i] = Some(if channel.dda_mode() {
                channel.dda
            } else if i >= 4 && channel.noise & 0x80 != 0 {
                if channel.lfsr & 1 != 0 { 31 } else { 0 }
            } else {
                channel.wave[usize::from(channel.address)]
            });
        }
        out
    }

    fn output(&self) -> [f64; 2] {
        let mut sum = [0.0; 2];
        for (i, (word, channel)) in self.words().iter().zip(&self.channels).enumerate() {
            if self.output_mute & (1 << i) != 0 {
                continue;
            }
            if let Some(word) = word {
                sum[0] += f64::from(*word) * channel.gain[0];
                sum[1] += f64::from(*word) * channel.gain[1];
            }
        }
        sum
    }
}

impl Chip for Huc6280 {
    const TICKS_PER_FRAME: u32 = FRAME_TICKS;
    const OUTPUT_DIVISOR: i64 = OUTPUT_SUBSTEPS;
    fn mute_output(&mut self, mask: u8) {
        self.output_mute = mask;
    }
    fn apply(&mut self, target: Target, data: u8) -> Value {
        match target {
            Target::Register(register) => self.write_register(register, data),
            _ => json!({"ignored":"not a HuC6280 PSG register","data":data}),
        }
    }
    fn tick(&mut self) -> [i32; 2] {
        self.step();
        self.output()
            .map(|v| (v * OUTPUT_SUBSTEPS as f64).round() as i32)
    }
    fn state(&self) -> Value {
        let channels: Vec<Value> = self
            .channels
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut v = json!({"channel":i + 1,"divider":c.divider,"control":format!("0x{:02x}", c.control),"mode":c.mode(),
                    "balance":format!("0x{:02x}", c.balance),"address_counter":c.address,"dda_level":c.dda,
                    "waveform":c.wave.to_vec(),"waveform_writes":c.wave_writes,"dda_writes":c.dda_writes});
                if i >= 4 {
                    v["noise"] = json!(format!("0x{:02x}", c.noise));
                    v["noise_lfsr"] = json!(format!("0x{:04x}", c.lfsr));
                }
                v
            })
            .collect();
        json!({"channel_select":self.select,"main_amplitude":format!("0x{:02x}", self.main),
            "lfo":{"frequency":self.lfo_frequency,"control":format!("0x{:02x}", self.lfo_control),"active":self.lfo_active()},
            "channels":channels,"ticks":self.cycles})
    }
}

/// Register writes for HuC6280 notes on channels 0..5 (channels 1..6). Tone
/// notes take pitch, volume (AL 0..31) and a 32-word wavetable; noise notes
/// (channels 4 and 5) take noise.period (the R7 frequency field 0..31) and
/// volume. Each note selects its channel, loads the waveform from word 0,
/// sets the divider and balance $FF and turns the channel on; its end turns
/// the channel off. Setup at tick 0 sets the main level to $FF.
pub fn note_writes(
    clock_hz: u32,
    notes: &[PsgNote],
) -> Result<(Vec<ChipWrite>, Vec<Value>), (usize, String)> {
    let mut writes: Vec<(u64, u8, usize, u8, u8)> = vec![(0, 0, 0, 1, 0xff)];
    let mut reports = Vec::new();
    let mut spans: Vec<(u8, u64, u64)> = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        let fail = |message: String| (index, message);
        let max = crate::psg::MAX_RENDER_SECONDS as f64;
        let at = |s: f64| -> Result<u64, (usize, String)> {
            if !(s.is_finite() && (0.0..=max).contains(&s)) {
                return Err((index, format!("times must be 0..{max} seconds")));
            }
            Ok((s * f64::from(clock_hz)).round() as u64)
        };
        if usize::from(note.channel) >= CHANNELS {
            return Err(fail(
                "HuC6280 channels are 0..5 (channels 1..6; noise on 4 and 5)".into(),
            ));
        }
        if note.attenuation.is_some()
            || note.envelope.is_some()
            || note.wave.is_some()
            || note.dpcm.is_some()
            || note.duty.is_some()
        {
            return Err(fail(
                "HuC6280 notes take pitch, volume, wavetable or noise; use writes for DDA, balance and the LFO".into(),
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
        let volume = note
            .volume
            .filter(|v| *v <= 31)
            .ok_or_else(|| fail("volume must be 0..31 (the 5-bit AL, 1.5 dB steps)".into()))?;
        let ch = note.channel;
        let mut report = json!({"index":index,"channel":ch,"hardware_channel":ch + 1,
            "start_tick":start,"end_tick":end,"volume":volume,
            "attenuation_db":amplitude(15, volume, 15).map(|(_, h)| 1.5 * f64::from(h))});
        let mut at_start: Vec<(u8, u8)> = vec![(0, ch)];
        if let Some(noise) = &note.noise {
            if ch < 4 {
                return Err(fail(
                    "noise plays on channels 4 and 5 (channels 5 and 6)".into(),
                ));
            }
            if note.pitch.is_some() || note.wavetable.is_some() {
                return Err(fail("noise notes take noise.period and volume".into()));
            }
            if noise.mode.is_some()
                || noise.rate.is_some()
                || noise.divisor.is_some()
                || noise.shift.is_some()
                || noise.taps.is_some()
            {
                return Err(fail(
                    "HuC6280 noise takes period (the R7 frequency field 0..31) only".into(),
                ));
            }
            let field = noise
                .period
                .filter(|p| *p <= 31)
                .ok_or_else(|| fail("noise.period must be 0..31".into()))?;
            report["noise"] = json!({"frequency_field":field,"nf":noise_period(field),
                "noise_clock_hz":f64::from(clock_hz) / (64.0 * f64::from(noise_period(field))),
                "level_changes_hz":f64::from(clock_hz) / (32.0 * f64::from(noise_period(field)))});
            at_start.extend([(7, 0x80 | field), (5, 0xff), (4, 0x80 | volume)]);
        } else {
            let Some(pitch) = &note.pitch else {
                return Err(fail("tone notes take pitch, volume and wavetable".into()));
            };
            let wave = note
                .wavetable
                .as_ref()
                .ok_or_else(|| fail("tone notes need wavetable: 32 words 0..31".into()))?;
            if wave.len() != 32 || wave.iter().any(|w| *w > 31) {
                return Err(fail("wavetable must be 32 words 0..31".into()));
            }
            let (divider, pitch_report) = match *pitch {
                PsgPitch::Period { period } => (
                    f64::from(period),
                    json!({"source":"period","period":period}),
                ),
                PsgPitch::MidiNote { note, a4_hz } => {
                    if note > 127 || !(a4_hz.is_finite() && (220.0..=880.0).contains(&a4_hz)) {
                        return Err(fail("midi note must be 0..127 and a4_hz 220..880".into()));
                    }
                    let target = a4_hz * ((f64::from(note) - 69.0) / 12.0).exp2();
                    let divider = (f64::from(clock_hz) / (32.0 * target)).round();
                    let actual = f64::from(clock_hz) / (32.0 * divider);
                    (
                        divider,
                        json!({"source":"midi_note","note":note,"a4_hz":a4_hz,"target_hz":target,
                            "cents_error":1200.0 * (actual / target).log2()}),
                    )
                }
            };
            if !(1.0..=4095.0).contains(&divider) {
                return Err(fail(format!(
                    "divider {divider} is outside 1..4095 at this clock"
                )));
            }
            let divider = divider as u16;
            report["period"] = json!(divider);
            report["frequency_hz"] = json!(tone_hz(clock_hz, divider));
            report["pitch"] = pitch_report;
            if ch >= 4 {
                at_start.push((7, 0x00));
            }
            at_start.extend([(4, 0x40), (4, 0x00)]);
            at_start.extend(wave.iter().map(|w| (6, *w)));
            at_start.extend([
                (2, (divider & 0xff) as u8),
                (3, (divider >> 8) as u8),
                (5, 0xff),
                (4, 0x80 | volume),
            ]);
        }
        let release = [(0u8, ch), (4u8, 0x00u8)];
        let show = |w: &[(u8, u8)]| {
            w.iter()
                .map(|(r, d)| json!({"register":register_name(*r),"offset":r,"data":d}))
                .collect::<Vec<_>>()
        };
        report["writes"] = json!(show(&at_start));
        report["release_writes"] = json!(show(&release));
        for (register, data) in at_start {
            writes.push((start, 2, index, register, data));
        }
        for (register, data) in release {
            writes.push((end, 1, index, register, data));
        }
        reports.push(report);
    }
    // At one tick: setup, then releases, then starts; note order within each.
    writes.sort_by_key(|&(tick, class, index, _, _)| (tick, class, index));
    Ok((
        writes
            .into_iter()
            .map(|(tick, _, _, register, data)| ChipWrite {
                tick,
                target: Target::Register(register),
                data,
            })
            .collect(),
        reports,
    ))
}

/// Setup written at tick 0 by note renders: R1 = $FF.
pub const NOTE_SETUP: [(u8, u8); 1] = [(1, 0xff)];

pub fn model_scope() -> Value {
    json!({
        "renderer": "Tunemend HuC6280 PSG model; not original hardware audio or driver playback",
        "sources": ["Hudson Soft, HuC6280 CMOS Programmable Sound Generator Manual (registers R0-R9, modes, amplitude levels, frequency formulas, LFO table)",
            "Paul Clifford, PC Engine Programmable Sound Generator (register notes, noise as a random square wave)",
            "VGM 1.71 specification (HuC6280 clock 0xA4, command 0xB9)"],
        "modelled": ["six channels with 32 words of 5-bit waveform memory and a 12-bit divider F: one word every F ticks, f = clock/(32F)",
            "R4 modes: write data (R6 stores at the address counter and increments it), reset counter (DDA set, chON clear), mixing (the counter advances at the divider rate), direct D/A (R6 writes go to the output and are held); a write with DDA set resets the address counter",
            "amplitude: R1 main and R5 balance in 3 dB steps, AL in 1.5 dB steps, per side; silent when R1, AL or R5 is 0 or the 3 dB sum (15 - main) + (15 - AL >> 1) + (15 - balance) reaches 15 (45 dB), as the manual's example computes",
            "noise on channels 5 and 6 (R7 bit 7): a random square wave of words 31 and 0; the manual's noise clock is clock/(64 NF), NF = 31 - the 5-bit field, and each of its periods holds two independently random halves (Clifford), so the level may change every 32 NF ticks",
            "LFO (LF CTL 1..3 with channels 1 and 2 in mixing mode): channel 2 steps every F2 * FLF ticks and still sounds at its own amplitude; channel 1's divider becomes F1 + ((word - 16) << shift) with shift 0, 2 or 4; LF TRG resets channel 2's counter to 0 and holds it",
            "stereo output: each channel's word times its left and right amplitude"],
        "declared_choices": ["power-on: registers 0, channels off, address counters 0, waveform memory 0, noise LFSR 1, LFO off; R0 selects channel 1",
            "one tick is one cycle of the header clock (3579545 Hz, the master clock / 2); a native sample is the mean of 32 ticks (clock/32, 111860.8 Hz)",
            "F = 0 (forbidden by the manual) counts as 4096; NF = 0 (R7 field 31, undefined) counts as 32; R8 = 0 counts as 256",
            "a divider change takes effect at the next reload; a channel entering mixing mode starts a full divider period; channel 1's modulated divider is read at each of its reloads and kept to 12 bits; R7 writes restart the noise clock divider; R8 and R9 writes restart the LFO divider",
            "LF CTL shifts follow the manual (0, 2, 4 bits); Clifford's notes report 0, 4 and 8 bits",
            "the noise sequence is not documented: a 15-bit maximal-length LFSR (bit 0 xor bit 1 into bit 14) stands in for it, so only its rate and level follow the documents",
            "R6 writes in mixing mode leave waveform memory unchanged; R0 values 6 and 7 select no channel",
            "the output is unipolar with DC (no output coupling); the 1.5 dB and 3 dB steps are exact, where the manual says about",
            "writes take effect before the tick of their time, in order"],
        "not_modelled": ["analog output stage, DC blocking and the small leak below the 45 dB cut-off", "differences between the HuC6280 and HuC6280A (switching noise)",
            "write timing inside the CPU and the PC Engine CD ADPCM and CD audio", "a second chip in dual-chip VGM files renders as an independent PSG"],
        "equality": "Equal register writes and clock do not establish that a game, driver or hardware unit sounds the same."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip() -> Huc6280 {
        Huc6280::new(PceConfig::new(CLOCK_HZ))
    }

    fn write(chip: &mut Huc6280, writes: &[(u8, u8)]) {
        for &(r, d) in writes {
            chip.write_register(r, d);
        }
    }

    fn saw(chip: &mut Huc6280, channel: u8) {
        write(chip, &[(0, channel), (4, 0x40), (4, 0x00)]);
        for w in 0..32 {
            chip.write_register(6, w);
        }
    }

    #[test]
    fn waveform_steps_every_f_ticks_and_wraps() {
        for f in [1u16, 7, 254, 0xfff] {
            let mut c = chip();
            saw(&mut c, 0);
            write(
                &mut c,
                &[
                    (1, 0xff),
                    (2, (f & 0xff) as u8),
                    (3, (f >> 8) as u8),
                    (5, 0xff),
                    (4, 0x9f),
                ],
            );
            assert_eq!(c.words()[0], Some(0));
            let f = u32::from(f);
            for step in 1..=40u32 {
                for _ in 0..f - 1 {
                    c.step();
                }
                assert_eq!(
                    c.words()[0],
                    Some(((step - 1) % 32) as u8),
                    "f {f} step {step}"
                );
                c.step();
                assert_eq!(c.words()[0], Some((step % 32) as u8));
            }
        }
        // F = 0 counts as 4096; f = clock/(32F) for the manual's A4 (254).
        let mut c = chip();
        saw(&mut c, 0);
        write(&mut c, &[(4, 0x80)]);
        for _ in 0..4095 {
            c.step();
        }
        assert_eq!(c.channels[0].address, 0);
        c.step();
        assert_eq!(c.channels[0].address, 1);
        assert!((tone_hz(3_580_000, 254) - 440.45).abs() < 0.005);
    }

    #[test]
    fn modes_follow_the_manual_table() {
        let mut c = chip();
        write(&mut c, &[(0, 2), (4, 0x00), (6, 5), (6, 6), (6, 7)]);
        assert_eq!(c.channels[2].address, 3);
        assert_eq!(&c.channels[2].wave[..3], &[5, 6, 7]);
        // Reset counter mode, then write data mode from word 0.
        let effect = c.write_register(4, 0x40);
        assert_eq!(effect["address_counter_reset"], true);
        write(&mut c, &[(4, 0x00), (6, 9)]);
        assert_eq!(c.channels[2].wave[0], 9);
        // The DDA latch is set in reset counter mode and sounds with chON.
        write(&mut c, &[(4, 0x40), (6, 0x7f)]);
        assert_eq!(c.channels[2].dda, 31);
        assert_eq!(c.channels[2].wave[1], 6);
        assert_eq!(c.words()[2], None);
        write(&mut c, &[(4, 0xdf)]);
        assert_eq!(c.words()[2], Some(31));
        write(&mut c, &[(6, 3)]);
        assert_eq!(c.words()[2], Some(3));
        // Mixing mode ignores R6.
        write(&mut c, &[(4, 0x9f)]);
        let effect = c.write_register(6, 1);
        assert!(effect["ignored"].is_string());
        // Channel values 6 and 7 select nothing; R7 exists on channels 5 and 6 only.
        write(&mut c, &[(0, 6)]);
        assert!(c.write_register(2, 1)["ignored"].is_string());
        write(&mut c, &[(0, 3)]);
        assert!(c.write_register(7, 0x80)["ignored"].is_string());
        assert!(c.write_register(10, 0)["ignored"].is_string());
    }

    #[test]
    fn amplitude_steps_and_the_45_db_cut_off() {
        assert_eq!(amplitude(15, 31, 15).unwrap().1, 0);
        for al in 2..31u8 {
            let (a, _) = amplitude(15, al, 15).unwrap();
            let (b, _) = amplitude(15, al + 1, 15).unwrap();
            assert!((20.0 * (b / a).log10() - 1.5).abs() < 1e-9);
        }
        for level in 2..15u8 {
            let (a, _) = amplitude(level, 31, 15).unwrap();
            let (b, _) = amplitude(level + 1, 31, 15).unwrap();
            assert!((20.0 * (b / a).log10() - 3.0).abs() < 1e-9);
            let (a, _) = amplitude(15, 31, level).unwrap();
            let (b, _) = amplitude(15, 31, level + 1).unwrap();
            assert!((20.0 * (b / a).log10() - 3.0).abs() < 1e-9);
        }
        // The manual's example: main C/8, AL high bits E, balance F/8.
        for al in [0x1c, 0x1d] {
            let (_, half) = amplitude(0xc, al, 0xf).unwrap();
            assert_eq!(half / 2, 4, "left: 12 dB below the maximum");
            assert!(amplitude(0x8, al, 0x8).is_none(), "right: 45 dB, silent");
        }
        assert!(amplitude(0, 31, 15).is_none());
        assert!(amplitude(15, 0, 15).is_none());
        assert!(amplitude(15, 1, 15).is_none());
        assert!(amplitude(15, 31, 0).is_none());
        assert_eq!(amplitude(15, 2, 15).unwrap().1, 29);
        assert_eq!(amplitude(15, 4, 15).unwrap().1, 27);
    }

    #[test]
    fn noise_rate_follows_the_inverted_field() {
        for field in [0u8, 15, 30, 31] {
            let mut c = chip();
            write(
                &mut c,
                &[(0, 4), (7, 0x80 | field), (1, 0xff), (5, 0xff), (4, 0x9f)],
            );
            let nf = noise_period(field);
            let mut changes = Vec::new();
            let mut last = c.channels[4].lfsr;
            for t in 1..=(32 * nf * 4) {
                c.step();
                if c.channels[4].lfsr != last {
                    changes.push(t);
                    last = c.channels[4].lfsr;
                }
            }
            assert_eq!(changes, (1..=4).map(|k| k * 32 * nf).collect::<Vec<_>>());
            assert!(matches!(c.words()[4], Some(0 | 31)));
        }
        assert_eq!(noise_period(0), 31);
        assert_eq!(noise_period(31), 32);
        // The stand-in LFSR is maximal.
        let mut ch = Channel::default();
        let mut n = 0;
        loop {
            ch.noise_counter = 1;
            ch.clock_noise();
            n += 1;
            if ch.lfsr == 1 {
                break;
            }
        }
        assert_eq!(n, 32767);
    }

    #[test]
    fn lfo_modulates_channel_1_from_channel_2() {
        let mut c = chip();
        // Channel 2: words 16 + k for the first half (modulation +k), 16 - ... after.
        write(&mut c, &[(0, 1), (4, 0x40), (4, 0x00)]);
        let words: Vec<u8> = (0..32)
            .map(|k| if k < 16 { 16 + k } else { 31 - k })
            .collect();
        for w in &words {
            c.write_register(6, *w);
        }
        write(&mut c, &[(2, 10), (3, 0), (4, 0x9f), (8, 3)]);
        saw(&mut c, 0);
        write(&mut c, &[(2, 100), (3, 0), (4, 0x9f)]);
        // LF CTL 2: shift 2.
        let effect = c.write_register(9, 0x02);
        assert!(effect["modulation"].as_str().unwrap().contains("2 bits"));
        assert!(c.lfo_active());
        assert_eq!(c.modulated_period(), 100);
        // Channel 2 steps every F2 * FLF = 30 ticks.
        for _ in 0..30 {
            c.step();
        }
        assert_eq!(c.channels[1].address, 1);
        assert_eq!(c.modulated_period(), 104);
        for _ in 0..30 * 15 {
            c.step();
        }
        assert_eq!(c.channels[1].address, 16);
        // Word 15: -1 << 2.
        assert_eq!(c.modulated_period(), 96);
        // LF CTL 3: shift 4; word 16 + 15 -> +15 << 4 at address 15.
        write(&mut c, &[(9, 0x83)]);
        assert_eq!(c.channels[1].address, 0);
        for _ in 0..100 {
            c.step();
        }
        assert_eq!(c.channels[1].address, 0, "held while LF TRG is set");
        assert_eq!(c.modulated_period(), 100);
        write(&mut c, &[(9, 0x03)]);
        for _ in 0..30 * 15 {
            c.step();
        }
        assert_eq!(c.channels[1].address, 15);
        assert_eq!(c.modulated_period(), 100 + (15 << 4));
        // Channel 1 then advances every modulated period.
        let start = c.channels[0].address;
        let mut ticks = 0;
        while c.channels[0].address == start {
            c.step();
            ticks += 1;
        }
        assert!(ticks <= 340);
        // Turning channel 2 off stops the modulation.
        write(&mut c, &[(0, 1), (4, 0x1f)]);
        assert!(!c.lfo_active());
        assert_eq!(c.modulated_period(), 100);
    }

    #[test]
    fn output_mixes_words_by_side_amplitude() {
        let mut c = chip();
        saw(&mut c, 0);
        write(&mut c, &[(1, 0xff), (2, 0), (3, 1), (5, 0xf0), (4, 0x9f)]);
        // Word 0 sounds as 0 on both sides; move to word 31.
        c.channels[0].address = 31;
        let out = c.tick();
        assert_eq!(out[1], 0);
        assert_eq!(out[0], (31.0 * LEVEL * 8.0).round() as i32);
        // Six channels at full level stay within PCM16.
        let mut c = chip();
        write(&mut c, &[(1, 0xff)]);
        for ch in 0..6 {
            write(&mut c, &[(0, ch), (4, 0x40), (6, 31), (5, 0xff), (4, 0xdf)]);
        }
        let out = c.tick();
        assert!(out[0] / 8 <= 32767 && out[0] / 8 > 32700, "{out:?}");
    }

    fn blank(channel: u8) -> PsgNote {
        PsgNote {
            channel,
            start_seconds: 0.0,
            duration_seconds: 0.1,
            pitch: None,
            noise: None,
            attenuation: None,
            volume: None,
            duty: None,
            envelope: None,
            wave: None,
            dpcm: None,
            wavetable: None,
        }
    }

    #[test]
    fn note_writes_load_the_wave_and_switch_the_channel() {
        let wave: Vec<u8> = (0..32).collect();
        let notes = vec![
            PsgNote {
                channel: 0,
                start_seconds: 0.0,
                duration_seconds: 0.1,
                pitch: Some(PsgPitch::MidiNote {
                    note: 69,
                    a4_hz: 440.0,
                }),
                volume: Some(31),
                wavetable: Some(wave.clone()),
                ..blank(0)
            },
            PsgNote {
                channel: 5,
                start_seconds: 0.05,
                duration_seconds: 0.1,
                noise: Some(crate::psg::PsgNoise {
                    period: Some(29),
                    ..Default::default()
                }),
                volume: Some(20),
                ..blank(5)
            },
        ];
        let (writes, reports) = note_writes(CLOCK_HZ, &notes).unwrap();
        assert_eq!(reports[0]["period"], 254);
        assert_eq!(reports[1]["noise"]["nf"], 2);
        assert_eq!(writes[0].target, Target::Register(1));
        let first: Vec<(Target, u8)> = writes[1..4].iter().map(|w| (w.target, w.data)).collect();
        assert_eq!(
            first,
            [
                (Target::Register(0), 0),
                (Target::Register(4), 0x40),
                (Target::Register(4), 0x00)
            ]
        );
        let mut c = chip();
        for w in &writes {
            if w.tick == 0 {
                c.apply(w.target, w.data);
            }
        }
        assert_eq!(c.channels[0].wave.to_vec(), wave);
        assert_eq!(c.words()[0], Some(0));
        let mut bad = notes[0].clone();
        bad.wavetable = Some(vec![32; 32]);
        assert!(note_writes(CLOCK_HZ, &[bad]).is_err());
        let mut bad = notes[1].clone();
        bad.channel = 3;
        assert!(note_writes(CLOCK_HZ, &[bad]).is_err());
    }
}
