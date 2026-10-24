//! NES APU model (Ricoh 2A03 for NTSC, 2A07 for PAL): two pulse channels with
//! sweep units, the triangle with its linear counter, the noise LFSR, the
//! delta modulation channel reading sample bytes from declared memory, the
//! frame counter in its 4- and 5-step modes, the $4015 enable and status
//! semantics and the documented nonlinear mixer.
//!
//! References: the
//! NESdev wiki pages "APU", "APU Pulse" (duty lookup and its reading order,
//! timer period t+1 on the APU clock, the t < 8 and target > $7FF mutes, the
//! sweep target with pulse 1's ones' complement and pulse 2's two's
//! complement), "APU Envelope", "APU Length Counter" (the 32-entry table and the
//! halt flag), "APU Triangle" (CPU-clocked timer, 32-step sequence, linear
//! counter reload flag and control flag, ultrasonic periods), "APU Noise"
//! (15-bit LFSR with the mode bit, NTSC and PAL period tables, power-up value
//! 1), "APU DMC" (rate tables, output unit, memory reader, address and length
//! formulas, loop and IRQ flag), "APU Frame Counter" (step cycles of both
//! modes on NTSC and PAL, $4017 reset and immediate clocks) and "APU Mixer"
//! (the pulse and TND formulas), and the VGM 1.71 specification (0x84 clock,
//! command 0xB4 and data block type 0xC2). Behaviour the documents leave open
//! is a declared choice in [`model_scope`].
use crate::psg::{NoiseMode, PsgNote, PsgPitch};
use crate::sound_chip::{Chip, ChipWrite, Target};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::OnceLock;

pub const NTSC_CLOCK_HZ: u32 = 1_789_773;
pub const PAL_CLOCK_HZ: u32 = 1_662_607;
pub const MIN_CLOCK_HZ: u32 = 1_000_000;
pub const MAX_CLOCK_HZ: u32 = 2_000_000;
/// One model tick is one CPU cycle.
pub const TICK_DIVIDER: u32 = 1;
/// CPU cycles averaged into one native output sample.
pub const FRAME_TICKS: u32 = 16;
/// PCM16 units per unit of mixer output (0.0..1.0), before averaging.
const OUTPUT_SCALE: f64 = 32767.0;
const OUTPUT_SUBSTEPS: i64 = 8;

/// Length counter table indexed by bits 7..3 of $4003/$4007/$400B/$400F.
pub const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22,
    192, 24, 72, 26, 16, 28, 32, 30,
];
/// Duty lookup tables; the sequencer reads them in the order 0, 7, 6, ..., 1.
pub const DUTY_LOOKUP: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1],
    [0, 0, 0, 0, 0, 0, 1, 1],
    [0, 0, 0, 0, 1, 1, 1, 1],
    [1, 1, 1, 1, 1, 1, 0, 0],
];
const CHANNEL_NAMES: [&str; 5] = ["pulse1", "pulse2", "triangle", "noise", "dmc"];
const DUTY_NAMES: [&str; 4] = ["12.5%", "25%", "50%", "75% (25% negated)"];
pub const TRIANGLE_SEQUENCE: [u8; 32] = [
    15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
    13, 14, 15,
];
/// CPU cycles between noise shift register clocks.
pub const NOISE_PERIODS: [[u16; 16]; 2] = [
    [
        4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068,
    ],
    [
        4, 8, 14, 30, 60, 88, 118, 148, 188, 236, 354, 472, 708, 944, 1890, 3778,
    ],
];
/// CPU cycles between DMC output level changes.
pub const DMC_RATES: [[u16; 16]; 2] = [
    [
        428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54,
    ],
    [
        398, 354, 316, 298, 276, 236, 210, 198, 176, 148, 132, 118, 98, 78, 66, 50,
    ],
];

/// The CPU timing variant, which selects the noise, DMC and frame counter tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NesRegion {
    /// RP2A03 (NTSC NES and Famicom), CPU clock 1789773 Hz.
    Ntsc,
    /// RP2A07 (PAL NES), CPU clock 1662607 Hz.
    Pal,
}

impl NesRegion {
    fn index(self) -> usize {
        match self {
            NesRegion::Ntsc => 0,
            NesRegion::Pal => 1,
        }
    }
    pub fn part(self) -> &'static str {
        match self {
            NesRegion::Ntsc => "RP2A03",
            NesRegion::Pal => "RP2A07",
        }
    }
}

/// Frame counter events: CPU cycle counted from 0 at the reset tick, whether
/// it also clocks the half frame, and whether it sets the IRQ flag.
struct FrameSteps {
    events: [(u32, bool, bool); 4],
    length: u32,
}

/// APU cycle values from the NESdev frame counter tables converted to CPU
/// cycles (APU cycle n: GET is CPU cycle 2n, PUT is 2n + 1).
fn frame_steps(region: NesRegion, five_step: bool) -> FrameSteps {
    match (region, five_step) {
        (NesRegion::Ntsc, false) => FrameSteps {
            events: [
                (7457, false, false),
                (14913, true, false),
                (22371, false, false),
                (29829, true, true),
            ],
            length: 29830,
        },
        (NesRegion::Ntsc, true) => FrameSteps {
            events: [
                (7457, false, false),
                (14913, true, false),
                (22371, false, false),
                (37281, true, false),
            ],
            length: 37282,
        },
        (NesRegion::Pal, false) => FrameSteps {
            events: [
                (8313, false, false),
                (16627, true, false),
                (24939, false, false),
                (33253, true, true),
            ],
            length: 33254,
        },
        (NesRegion::Pal, true) => FrameSteps {
            events: [
                (8313, false, false),
                (16627, true, false),
                (24939, false, false),
                (41565, true, false),
            ],
            length: 41566,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NesConfig {
    pub region: NesRegion,
}

impl NesConfig {
    pub fn new(region: NesRegion) -> Self {
        Self { region }
    }
    /// The PAL tables for a clock within 1% of the 2A07 clock, else NTSC.
    pub fn for_clock(clock_hz: u32) -> Self {
        let pal = f64::from(clock_hz.abs_diff(PAL_CLOCK_HZ)) <= 0.01 * f64::from(PAL_CLOCK_HZ);
        Self::new(if pal { NesRegion::Pal } else { NesRegion::Ntsc })
    }
    pub fn to_json(&self) -> Value {
        let r = self.region.index();
        let four = frame_steps(self.region, false);
        let five = frame_steps(self.region, true);
        json!({"variant":"nes_apu","region":self.region,"part":self.region.part(),
            "tick":"one CPU cycle","native_sample":format!("mean of {FRAME_TICKS} CPU cycles (clock/{FRAME_TICKS})"),
            "noise_periods_cpu_cycles":NOISE_PERIODS[r],"dmc_rates_cpu_cycles":DMC_RATES[r],
            "frame_counter_cpu_cycles":{"four_step":four.events.iter().map(|e| e.0).collect::<Vec<_>>(),"four_step_length":four.length,
                "five_step":five.events.iter().map(|e| e.0).collect::<Vec<_>>(),"five_step_length":five.length},
            "mixer":"pulse_out = 95.88 / (8128 / (p1 + p2) + 100); tnd_out = 159.79 / (1 / (t / 8227 + n / 12241 + d / 22638) + 100); output = pulse_out + tnd_out (0 for an all-zero group)",
            "output":"mono, unipolar: mixer output 0..1 times 32767 on both sides, DC kept",
            "power_on_state":"registers 0, channels disabled, frame counter reset in 4-step mode, noise LFSR 1, DMC level 0 with an empty sample buffer, DMC memory 0"})
    }
}

pub fn check_clock(clock_hz: u32) -> Result<(), String> {
    if (MIN_CLOCK_HZ..=MAX_CLOCK_HZ).contains(&clock_hz) {
        Ok(())
    } else {
        Err(format!(
            "clock_hz must be {MIN_CLOCK_HZ}..{MAX_CLOCK_HZ}, such as {NTSC_CLOCK_HZ} (NTSC 2A03) or {PAL_CLOCK_HZ} (PAL 2A07)"
        ))
    }
}

const REGISTER_NAMES: [&str; 0x18] = [
    "pulse1_control",
    "pulse1_sweep",
    "pulse1_timer_low",
    "pulse1_length_timer_high",
    "pulse2_control",
    "pulse2_sweep",
    "pulse2_timer_low",
    "pulse2_length_timer_high",
    "triangle_linear_counter",
    "unused_4009",
    "triangle_timer_low",
    "triangle_length_timer_high",
    "noise_control",
    "unused_400d",
    "noise_mode_period",
    "noise_length",
    "dmc_control",
    "dmc_direct_load",
    "dmc_sample_address",
    "dmc_sample_length",
    "oam_dma",
    "status",
    "joypad_1",
    "frame_counter",
];

/// Register name of an offset from $4000 (0x00..0x1F).
pub fn register_name(offset: u8) -> &'static str {
    REGISTER_NAMES
        .get(usize::from(offset))
        .copied()
        .unwrap_or("cpu_test_mode")
}

/// Bytes written into the DMC's view of CPU memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBlock {
    pub start: u16,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
struct Envelope {
    start: bool,
    divider: u8,
    decay: u8,
}

impl Envelope {
    fn clock(&mut self, looped: bool, period: u8) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = period;
        } else if self.divider == 0 {
            self.divider = period;
            if self.decay > 0 {
                self.decay -= 1;
            } else if looped {
                self.decay = 15;
            }
        } else {
            self.divider -= 1;
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Pulse {
    /// Last $4000/$4004 value: duty, halt/loop, constant volume, volume/period.
    control: u8,
    sweep: u8,
    sweep_reload: bool,
    sweep_divider: u8,
    period: u16,
    timer: u16,
    step: u8,
    length: u8,
    envelope: Envelope,
    ones_complement: bool,
}

impl Pulse {
    fn halt(&self) -> bool {
        self.control & 0x20 != 0
    }
    fn volume(&self) -> u8 {
        if self.control & 0x10 != 0 {
            self.control & 15
        } else {
            self.envelope.decay
        }
    }
    /// The sweep target period, clamped to 0 when negative.
    fn target(&self) -> u16 {
        let change = i32::from(self.period >> (self.sweep & 7));
        let period = i32::from(self.period);
        let target = if self.sweep & 8 != 0 {
            period - change - i32::from(self.ones_complement)
        } else {
            period + change
        };
        target.max(0) as u16
    }
    fn muted(&self) -> bool {
        self.period < 8 || self.target() > 0x7ff
    }
    fn output(&self) -> u8 {
        if self.muted()
            || self.length == 0
            || DUTY_LOOKUP[usize::from(self.control >> 6)][usize::from(self.step)] == 0
        {
            0
        } else {
            self.volume()
        }
    }
    fn clock_timer(&mut self) {
        if self.timer == 0 {
            self.timer = self.period;
            self.step = (self.step + 7) & 7;
        } else {
            self.timer -= 1;
        }
    }
    fn clock_sweep(&mut self) {
        let enabled = self.sweep & 0x80 != 0;
        if self.sweep_divider == 0 && enabled && self.sweep & 7 != 0 && !self.muted() {
            self.period = self.target();
        }
        if self.sweep_divider == 0 || self.sweep_reload {
            self.sweep_divider = (self.sweep >> 4) & 7;
            self.sweep_reload = false;
        } else {
            self.sweep_divider -= 1;
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Triangle {
    control: u8,
    reload_flag: bool,
    linear: u8,
    period: u16,
    timer: u16,
    step: u8,
    length: u8,
}

#[derive(Debug, Clone, Default)]
struct Noise {
    control: u8,
    mode: u8,
    timer: u16,
    lfsr: u16,
    length: u8,
    envelope: Envelope,
}

impl Noise {
    fn output(&self) -> u8 {
        if self.lfsr & 1 != 0 || self.length == 0 {
            0
        } else if self.control & 0x10 != 0 {
            self.control & 15
        } else {
            self.envelope.decay
        }
    }
    fn clock_lfsr(&mut self) {
        let other = if self.mode & 0x80 != 0 { 6 } else { 1 };
        let feedback = (self.lfsr ^ (self.lfsr >> other)) & 1;
        self.lfsr = (self.lfsr >> 1) | (feedback << 14);
    }
}

#[derive(Debug, Clone)]
struct Dmc {
    control: u8,
    timer: u16,
    level: u8,
    sample_address: u16,
    sample_length: u16,
    address: u16,
    bytes_remaining: u16,
    buffer: Option<u8>,
    shift: u8,
    bits_remaining: u8,
    silence: bool,
    irq_flag: bool,
    bytes_read: u64,
}

impl Default for Dmc {
    fn default() -> Self {
        Self {
            control: 0,
            timer: 0,
            level: 0,
            sample_address: 0xc000,
            sample_length: 1,
            address: 0xc000,
            bytes_remaining: 0,
            buffer: None,
            shift: 0,
            bits_remaining: 8,
            silence: true,
            irq_flag: false,
            bytes_read: 0,
        }
    }
}

impl Dmc {
    fn restart(&mut self) {
        self.address = self.sample_address;
        self.bytes_remaining = self.sample_length;
    }
    fn clock_output(&mut self) {
        if !self.silence {
            if self.shift & 1 != 0 {
                if self.level < 126 {
                    self.level += 2;
                }
            } else if self.level > 1 {
                self.level -= 2;
            }
        }
        self.shift >>= 1;
        self.bits_remaining -= 1;
        if self.bits_remaining == 0 {
            self.bits_remaining = 8;
            match self.buffer.take() {
                None => self.silence = true,
                Some(byte) => {
                    self.silence = false;
                    self.shift = byte;
                }
            }
        }
    }
}

struct MixTables {
    pulse: [f64; 31],
    tnd: Vec<f64>,
}

/// The documented mixer formulas, evaluated for every channel combination.
fn mix_tables() -> &'static MixTables {
    static TABLES: OnceLock<MixTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut pulse = [0.0; 31];
        for (n, value) in pulse.iter_mut().enumerate().skip(1) {
            *value = pulse_out(n as f64);
        }
        let mut tnd = vec![0.0; 16 * 16 * 128];
        for t in 0..16 {
            for n in 0..16 {
                for d in 0..128 {
                    tnd[(t * 16 + n) * 128 + d] = tnd_out(t as f64, n as f64, d as f64);
                }
            }
        }
        MixTables { pulse, tnd }
    })
}

/// pulse_out = 95.88 / (8128 / (pulse1 + pulse2) + 100), 0 when both are 0.
pub fn pulse_out(sum: f64) -> f64 {
    if sum == 0.0 {
        0.0
    } else {
        95.88 / (8128.0 / sum + 100.0)
    }
}

/// tnd_out = 159.79 / (1 / (t/8227 + n/12241 + d/22638) + 100), 0 when all are 0.
pub fn tnd_out(triangle: f64, noise: f64, dmc: f64) -> f64 {
    let sum = triangle / 8227.0 + noise / 12241.0 + dmc / 22638.0;
    if sum == 0.0 {
        0.0
    } else {
        159.79 / (1.0 / sum + 100.0)
    }
}

/// Register and channel state of one APU and its DMC memory view.
#[derive(Clone)]
pub struct NesApu {
    config: NesConfig,
    regs: [u8; 0x20],
    pulse: [Pulse; 2],
    triangle: Triangle,
    noise: Noise,
    dmc: Dmc,
    enabled: [bool; 5],
    five_step: bool,
    irq_inhibit: bool,
    frame_irq: bool,
    frame_cycle: u32,
    /// CPU cycle parity: pulse timers clock on every second cycle.
    odd_cycle: bool,
    /// CPU cycles run since power-on.
    cycles: u64,
    memory: Vec<u8>,
    written: Vec<bool>,
    unwritten_reads: u64,
    blocks: Vec<MemoryBlock>,
    blocks_applied: Vec<Value>,
    /// Channels left out of the output mix (bit c = channel c, in the order
    /// of [`NesApu::outputs`]); chip state is unaffected.
    output_mute: u8,
}

impl NesApu {
    /// Declared power-on state; `blocks` are memory images that
    /// [`Target::MemoryBlock`] writes copy into DMC memory at their time.
    pub fn new(config: NesConfig, blocks: Vec<MemoryBlock>) -> Self {
        let mut pulse: [Pulse; 2] = Default::default();
        pulse[0].ones_complement = true;
        Self {
            config,
            regs: [0; 0x20],
            pulse,
            triangle: Triangle::default(),
            noise: Noise {
                lfsr: 1,
                ..Default::default()
            },
            dmc: Dmc::default(),
            enabled: [false; 5],
            five_step: false,
            irq_inhibit: false,
            frame_irq: false,
            frame_cycle: 0,
            odd_cycle: false,
            cycles: 0,
            memory: vec![0; 0x10000],
            written: vec![false; 0x10000],
            unwritten_reads: 0,
            blocks,
            blocks_applied: Vec::new(),
            output_mute: 0,
        }
    }

    fn region(&self) -> usize {
        self.config.region.index()
    }

    /// $4015 as a read would return it (bit 5 open bus shown as 0); reading
    /// is not modelled, so the frame IRQ flag is not cleared.
    pub fn status(&self) -> u8 {
        let mut status = 0;
        for (bit, length) in [
            self.pulse[0].length,
            self.pulse[1].length,
            self.triangle.length,
            self.noise.length,
        ]
        .into_iter()
        .enumerate()
        {
            if length > 0 {
                status |= 1 << bit;
            }
        }
        if self.dmc.bytes_remaining > 0 {
            status |= 0x10;
        }
        if self.frame_irq {
            status |= 0x40;
        }
        if self.dmc.irq_flag {
            status |= 0x80;
        }
        status
    }

    /// Channel outputs before the mixer: pulse 1, pulse 2, triangle, noise, DMC.
    pub fn outputs(&self) -> [u8; 5] {
        [
            self.pulse[0].output(),
            self.pulse[1].output(),
            TRIANGLE_SEQUENCE[usize::from(self.triangle.step)],
            self.noise.output(),
            self.dmc.level,
        ]
    }

    /// Mixer output 0.0..1.0 of the current channel outputs.
    pub fn mix_level(&self) -> f64 {
        self.mix_level_muted(0)
    }

    /// [`NesApu::mix_level`] with the channels in `mute` (bit c = channel c)
    /// entering the mixer as 0.
    fn mix_level_muted(&self, mute: u8) -> f64 {
        let mut outputs = self.outputs();
        for (c, output) in outputs.iter_mut().enumerate() {
            if mute & (1 << c) != 0 {
                *output = 0;
            }
        }
        let [p1, p2, t, n, d] = outputs.map(usize::from);
        let tables = mix_tables();
        tables.pulse[p1 + p2] + tables.tnd[(t * 16 + n) * 128 + d]
    }

    fn load_length(&mut self, channel: usize, data: u8) -> Value {
        let value = LENGTH_TABLE[usize::from(data >> 3)];
        if !self.enabled[channel] {
            return json!({"index":data >> 3,"table_value":value,"loaded":false,
                "reason":"the channel is disabled in $4015, so the length counter stays 0"});
        }
        match channel {
            0 | 1 => self.pulse[channel].length = value,
            2 => self.triangle.length = value,
            _ => self.noise.length = value,
        }
        json!({"index":data >> 3,"table_value":value,"loaded":true})
    }

    fn write_status(&mut self, data: u8) -> Value {
        for channel in 0..5 {
            self.enabled[channel] = data & (1 << channel) != 0;
        }
        if !self.enabled[0] {
            self.pulse[0].length = 0;
        }
        if !self.enabled[1] {
            self.pulse[1].length = 0;
        }
        if !self.enabled[2] {
            self.triangle.length = 0;
        }
        if !self.enabled[3] {
            self.noise.length = 0;
        }
        let dmc = if !self.enabled[4] {
            self.dmc.bytes_remaining = 0;
            "stopped: bytes remaining set to 0; bits already in the shift register finish"
        } else if self.dmc.bytes_remaining == 0 {
            self.dmc.restart();
            "started: address and length reloaded from $4012/$4013"
        } else {
            "already playing: not restarted"
        };
        self.dmc.irq_flag = false;
        json!({"enabled":{"pulse1":self.enabled[0],"pulse2":self.enabled[1],"triangle":self.enabled[2],"noise":self.enabled[3],"dmc":self.enabled[4]},
            "dmc":dmc,"dmc_irq_flag_cleared":true,"disabled_lengths_cleared":true})
    }

    fn quarter_frame(&mut self) {
        for pulse in &mut self.pulse {
            let (looped, period) = (pulse.halt(), pulse.control & 15);
            pulse.envelope.clock(looped, period);
        }
        let (looped, period) = (self.noise.control & 0x20 != 0, self.noise.control & 15);
        self.noise.envelope.clock(looped, period);
        let triangle = &mut self.triangle;
        if triangle.reload_flag {
            triangle.linear = triangle.control & 0x7f;
        } else if triangle.linear > 0 {
            triangle.linear -= 1;
        }
        if triangle.control & 0x80 == 0 {
            triangle.reload_flag = false;
        }
    }

    fn half_frame(&mut self) {
        for pulse in &mut self.pulse {
            if pulse.length > 0 && !pulse.halt() {
                pulse.length -= 1;
            }
            pulse.clock_sweep();
        }
        if self.triangle.length > 0 && self.triangle.control & 0x80 == 0 {
            self.triangle.length -= 1;
        }
        if self.noise.length > 0 && self.noise.control & 0x20 == 0 {
            self.noise.length -= 1;
        }
    }

    fn clock_frame_counter(&mut self) {
        let steps = frame_steps(self.config.region, self.five_step);
        for (cycle, half, irq) in steps.events {
            if cycle == self.frame_cycle {
                self.quarter_frame();
                if half {
                    self.half_frame();
                }
                if irq && !self.irq_inhibit {
                    self.frame_irq = true;
                }
            }
        }
        self.frame_cycle += 1;
        if self.frame_cycle == steps.length {
            self.frame_cycle = 0;
        }
    }

    fn read_memory(&mut self, address: u16) -> u8 {
        if !self.written[usize::from(address)] {
            self.unwritten_reads += 1;
        }
        self.memory[usize::from(address)]
    }

    fn clock_dmc(&mut self) {
        if self.dmc.timer == 0 {
            self.dmc.timer = DMC_RATES[self.region()][usize::from(self.dmc.control & 15)] - 1;
            self.dmc.clock_output();
        } else {
            self.dmc.timer -= 1;
        }
        if self.dmc.buffer.is_none() && self.dmc.bytes_remaining > 0 {
            let byte = self.read_memory(self.dmc.address);
            let dmc = &mut self.dmc;
            dmc.buffer = Some(byte);
            dmc.bytes_read += 1;
            dmc.address = if dmc.address == 0xffff {
                0x8000
            } else {
                dmc.address + 1
            };
            dmc.bytes_remaining -= 1;
            if dmc.bytes_remaining == 0 {
                if dmc.control & 0x40 != 0 {
                    dmc.restart();
                } else if dmc.control & 0x80 != 0 {
                    dmc.irq_flag = true;
                }
            }
        }
    }

    /// Copy a memory block into DMC memory; bytes past $FFFF are dropped.
    pub fn write_memory(&mut self, block: &MemoryBlock) -> Value {
        let start = usize::from(block.start);
        let fit = block.data.len().min(0x10000 - start);
        self.memory[start..start + fit].copy_from_slice(&block.data[..fit]);
        self.written[start..start + fit].fill(true);
        let mut effect = json!({"memory_write":{"start":format!("${:04X}",block.start),"bytes":block.data.len(),
            "end":format!("${:04X}",start + fit - usize::from(fit > 0))}});
        if fit < block.data.len() {
            effect["memory_write"]["dropped_past_ffff"] = json!(block.data.len() - fit);
        }
        if start < 0x8000 {
            effect["note"] = json!(
                "the DMC reads $C000..$FFFF (wrapping to $8000); bytes below $8000 are never fetched"
            );
        }
        self.blocks_applied.push(effect["memory_write"].clone());
        effect
    }

    /// One write to an offset from $4000 (0x00..0x1F).
    pub fn write_register(&mut self, offset: u8, data: u8) -> Value {
        let address = format!("${:04X}", 0x4000 + u16::from(offset));
        if offset >= 0x20 {
            return json!({"address":address,"data":data,"ignored":"not a 2A03 register"});
        }
        let index = usize::from(offset);
        let before = self.regs[index];
        self.regs[index] = data;
        let mut effect = json!({"address":address,"register":register_name(offset),"before":before,"after":data});
        match offset {
            0x00 | 0x04 => {
                let pulse = &mut self.pulse[index / 4];
                pulse.control = data;
                effect["channel"] = json!(format!("pulse{}", index / 4 + 1));
                effect["duty"] = json!(DUTY_NAMES[usize::from(data >> 6)]);
                effect["length_halt_envelope_loop"] = json!(data & 0x20 != 0);
                effect["constant_volume"] = json!(data & 0x10 != 0);
                effect[if data & 0x10 != 0 {
                    "volume"
                } else {
                    "envelope_period"
                }] = json!(data & 15);
            }
            0x01 | 0x05 => {
                let pulse = &mut self.pulse[index / 4];
                pulse.sweep = data;
                pulse.sweep_reload = true;
                effect["channel"] = json!(format!("pulse{}", index / 4 + 1));
                effect["sweep"] = json!({"enabled":data & 0x80 != 0,"divider_period":(data >> 4) & 7,
                    "negate":data & 8 != 0,"shift":data & 7,"target_period":pulse.target(),"muted":pulse.muted()});
            }
            0x02 | 0x06 => {
                let pulse = &mut self.pulse[index / 4];
                pulse.period = (pulse.period & 0x700) | u16::from(data);
                effect["channel"] = json!(format!("pulse{}", index / 4 + 1));
                effect["period"] = json!(pulse.period);
                effect["target_period"] = json!(pulse.target());
                effect["muted"] = json!(pulse.muted());
            }
            0x03 | 0x07 => {
                let channel = index / 4;
                let pulse = &mut self.pulse[channel];
                pulse.period = (pulse.period & 0xff) | (u16::from(data & 7) << 8);
                pulse.step = 0;
                pulse.envelope.start = true;
                effect["channel"] = json!(format!("pulse{}", channel + 1));
                effect["period"] = json!(pulse.period);
                effect["target_period"] = json!(pulse.target());
                effect["muted"] = json!(pulse.muted());
                effect["phase_reset"] = json!(true);
                effect["envelope_restart"] = json!(true);
                effect["length"] = self.load_length(channel, data);
            }
            0x08 => {
                self.triangle.control = data;
                effect["channel"] = json!("triangle");
                effect["control_and_length_halt"] = json!(data & 0x80 != 0);
                effect["linear_reload_value"] = json!(data & 0x7f);
            }
            0x0a => {
                let t = &mut self.triangle;
                t.period = (t.period & 0x700) | u16::from(data);
                effect["channel"] = json!("triangle");
                effect["period"] = json!(t.period);
            }
            0x0b => {
                let t = &mut self.triangle;
                t.period = (t.period & 0xff) | (u16::from(data & 7) << 8);
                t.reload_flag = true;
                effect["channel"] = json!("triangle");
                effect["period"] = json!(t.period);
                effect["linear_reload_flag"] = json!(true);
                effect["length"] = self.load_length(2, data);
            }
            0x0c => {
                self.noise.control = data;
                effect["channel"] = json!("noise");
                effect["length_halt_envelope_loop"] = json!(data & 0x20 != 0);
                effect["constant_volume"] = json!(data & 0x10 != 0);
                effect[if data & 0x10 != 0 {
                    "volume"
                } else {
                    "envelope_period"
                }] = json!(data & 15);
            }
            0x0e => {
                self.noise.mode = data;
                effect["channel"] = json!("noise");
                effect["mode"] = json!(if data & 0x80 != 0 {
                    "periodic (feedback from bit 6)"
                } else {
                    "random (feedback from bit 1)"
                });
                effect["period_index"] = json!(data & 15);
                effect["period_cpu_cycles"] =
                    json!(NOISE_PERIODS[self.region()][usize::from(data & 15)]);
            }
            0x0f => {
                self.noise.envelope.start = true;
                effect["channel"] = json!("noise");
                effect["envelope_restart"] = json!(true);
                effect["length"] = self.load_length(3, data);
            }
            0x10 => {
                self.dmc.control = data;
                if data & 0x80 == 0 {
                    self.dmc.irq_flag = false;
                }
                effect["channel"] = json!("dmc");
                effect["irq_enable"] = json!(data & 0x80 != 0);
                effect["loop"] = json!(data & 0x40 != 0);
                effect["rate_index"] = json!(data & 15);
                effect["rate_cpu_cycles"] = json!(DMC_RATES[self.region()][usize::from(data & 15)]);
            }
            0x11 => {
                self.dmc.level = data & 0x7f;
                effect["channel"] = json!("dmc");
                effect["level"] = json!(data & 0x7f);
            }
            0x12 => {
                self.dmc.sample_address = 0xc000 + u16::from(data) * 64;
                effect["channel"] = json!("dmc");
                effect["sample_address"] = json!(format!("${:04X}", self.dmc.sample_address));
            }
            0x13 => {
                self.dmc.sample_length = u16::from(data) * 16 + 1;
                effect["channel"] = json!("dmc");
                effect["sample_length_bytes"] = json!(self.dmc.sample_length);
            }
            0x15 => effect["status"] = self.write_status(data),
            0x17 => {
                self.five_step = data & 0x80 != 0;
                self.irq_inhibit = data & 0x40 != 0;
                if self.irq_inhibit {
                    self.frame_irq = false;
                }
                self.frame_cycle = 0;
                if self.five_step {
                    self.quarter_frame();
                    self.half_frame();
                }
                effect["mode"] = json!(if self.five_step { "5-step" } else { "4-step" });
                effect["irq_inhibit"] = json!(self.irq_inhibit);
                effect["frame_counter"] = json!(if self.five_step {
                    "reset; quarter and half frame clocked immediately"
                } else {
                    "reset"
                });
            }
            0x09 | 0x0d => effect["note"] = json!("unused register: no effect"),
            0x14 => effect["ignored"] = json!("OAM DMA belongs to the PPU, not the APU"),
            0x16 => effect["ignored"] = json!("controller port strobe, not the APU"),
            _ => effect["ignored"] = json!("CPU test-mode register, not modelled"),
        }
        effect
    }

    fn step(&mut self) {
        self.cycles += 1;
        self.clock_frame_counter();
        let t = &mut self.triangle;
        if t.timer == 0 {
            t.timer = t.period;
            if t.linear > 0 && t.length > 0 {
                t.step = (t.step + 1) & 31;
            }
        } else {
            t.timer -= 1;
        }
        if self.odd_cycle {
            self.pulse[0].clock_timer();
            self.pulse[1].clock_timer();
        }
        self.odd_cycle = !self.odd_cycle;
        if self.noise.timer == 0 {
            self.noise.timer = NOISE_PERIODS[self.region()][usize::from(self.noise.mode & 15)] - 1;
            self.noise.clock_lfsr();
        } else {
            self.noise.timer -= 1;
        }
        self.clock_dmc();
    }
}

impl Chip for NesApu {
    const TICKS_PER_FRAME: u32 = FRAME_TICKS;
    const OUTPUT_DIVISOR: i64 = OUTPUT_SUBSTEPS;
    fn mute_output(&mut self, mask: u8) {
        self.output_mute = mask;
    }
    fn apply(&mut self, target: Target, data: u8) -> Value {
        match target {
            Target::Register(offset) => self.write_register(offset, data),
            Target::MemoryBlock(index) => match self.blocks.get(usize::from(index)).cloned() {
                Some(block) => self.write_memory(&block),
                None => json!({"ignored":"no such memory block"}),
            },
            _ => json!({"ignored":"not an NES APU port","data":data}),
        }
    }
    /// One CPU cycle: frame counter, channel timers, DMC reader, then the mix.
    fn tick(&mut self) -> [i32; 2] {
        self.step();
        let value = (self.mix_level_muted(self.output_mute) * OUTPUT_SCALE * OUTPUT_SUBSTEPS as f64)
            .round() as i32;
        [value, value]
    }
    fn state(&self) -> Value {
        json!({"registers":self.regs[..0x18].iter().map(|r| format!("0x{r:02x}")).collect::<Vec<_>>(),
            "status_read":format!("0x{:02x}", self.status()),
            "enabled":self.enabled,
            "lengths":{"pulse1":self.pulse[0].length,"pulse2":self.pulse[1].length,"triangle":self.triangle.length,"noise":self.noise.length},
            "pulse_periods":[self.pulse[0].period,self.pulse[1].period],
            "envelope_decay":{"pulse1":self.pulse[0].envelope.decay,"pulse2":self.pulse[1].envelope.decay,"noise":self.noise.envelope.decay},
            "triangle":{"period":self.triangle.period,"linear_counter":self.triangle.linear,"reload_flag":self.triangle.reload_flag,"step":self.triangle.step},
            "noise_lfsr":format!("0x{:04x}",self.noise.lfsr),
            "dmc":{"level":self.dmc.level,"address":format!("${:04X}",self.dmc.address),"bytes_remaining":self.dmc.bytes_remaining,
                "bytes_read":self.dmc.bytes_read,"irq_flag":self.dmc.irq_flag,"reads_of_unwritten_memory":self.unwritten_reads},
            "memory_blocks_applied":self.blocks_applied,
            "frame_counter":{"mode":if self.five_step {"5-step"} else {"4-step"},"cycle":self.frame_cycle,"irq_inhibit":self.irq_inhibit,"irq_flag":self.frame_irq}})
    }
}

/// Pulse frequency f = clock / (16 (t + 1)).
pub fn pulse_hz(clock_hz: u32, period: u16) -> f64 {
    f64::from(clock_hz) / (16.0 * (f64::from(period) + 1.0))
}

/// Triangle frequency f = clock / (32 (t + 1)).
pub fn triangle_hz(clock_hz: u32, period: u16) -> f64 {
    f64::from(clock_hz) / (32.0 * (f64::from(period) + 1.0))
}

/// A DPCM sample for a DMC note.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NesDpcm {
    /// Sample bytes as hexadecimal (two digits per byte, spaces allowed); the
    /// length must be 16n + 1 bytes, 1..4081. Each bit moves the 7-bit output
    /// level by 2, least significant bit first.
    pub data_hex: String,
    /// $4010 rate index 0..15 (NTSC 428..54 CPU cycles per bit).
    pub rate: u8,
    /// Loop the sample until the note ends. Default false.
    #[serde(default)]
    pub looped: bool,
    /// Optional $4011 level 0..127 written at the note start.
    #[serde(default)]
    pub start_level: Option<u8>,
}

/// Bytes of a hexadecimal string, ignoring whitespace.
pub fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err("hexadecimal data needs two digits per byte".into());
    }
    digits
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or_else(|| "data_hex must contain only hexadecimal digits".to_owned())
        })
        .collect()
}

/// Note writes, per-note reports and the DMC memory blocks they refer to.
pub type NoteRender = (Vec<ChipWrite>, Vec<Value>, Vec<MemoryBlock>);

/// Setup written at tick 0 by note renders: channels 1..4 enabled, frame
/// counter reset in 4-step mode with the IRQ inhibited.
pub const NOTE_SETUP: [(u8, u8); 2] = [(0x15, 0x0f), (0x17, 0x40)];

/// Register writes for NES notes. Channels: 0 pulse 1, 1 pulse 2, 2 triangle,
/// 3 noise, 4 DMC. Returns the writes, per-note reports and the memory blocks
/// that `Target::MemoryBlock` writes refer to.
pub fn note_writes(
    clock_hz: u32,
    config: NesConfig,
    notes: &[PsgNote],
) -> Result<NoteRender, (usize, String)> {
    let mut writes: Vec<(u64, u8, usize, Target, u8)> = Vec::new();
    let mut reports = Vec::new();
    let mut spans: Vec<(u8, u64, u64)> = Vec::new();
    let mut blocks: Vec<MemoryBlock> = Vec::new();
    let mut next_address: u32 = 0xc000;
    for (order, (offset, data)) in NOTE_SETUP.iter().enumerate() {
        writes.push((0, 0, order, Target::Register(*offset), *data));
    }
    for (index, note) in notes.iter().enumerate() {
        let fail = |message: String| (index, message);
        let max = crate::psg::MAX_RENDER_SECONDS as f64;
        let at = |s: f64| -> Result<u64, (usize, String)> {
            if !(s.is_finite() && (0.0..=max).contains(&s)) {
                return Err((index, format!("times must be 0..{max} seconds")));
            }
            Ok((s * f64::from(clock_hz)).round() as u64)
        };
        if note.channel > 4 {
            return Err(fail(
                "NES channels are 0..4 (pulse 1, pulse 2, triangle, noise, DMC)".into(),
            ));
        }
        if note.attenuation.is_some() || note.envelope.is_some() || note.wave.is_some() {
            return Err(fail(
                "NES notes take pitch, volume, duty, noise or dpcm; use writes for hardware envelopes".into(),
            ));
        }
        if !(note.duration_seconds.is_finite() && note.duration_seconds > 0.0) {
            return Err(fail("duration_seconds must be positive".into()));
        }
        let start = at(note.start_seconds)?;
        let end = at(note.start_seconds + note.duration_seconds)?;
        if end <= start {
            return Err(fail("the note is shorter than one CPU cycle".into()));
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
        let mut report = json!({"index":index,"channel":note.channel,
            "hardware_channel":CHANNEL_NAMES[usize::from(note.channel)],
            "start_tick":start,"end_tick":end});
        let volume = || -> Result<u8, (usize, String)> {
            note.volume
                .filter(|v| *v <= 15)
                .ok_or_else(|| fail("volume must be 0..15 (constant volume)".into()))
        };
        let pitch = |per_cycle: f64, min: u16| -> Result<(u16, Value), (usize, String)> {
            let Some(pitch) = &note.pitch else {
                return Err(fail("pulse and triangle notes take pitch".into()));
            };
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
                    let period = (f64::from(clock_hz) / (per_cycle * target) - 1.0).round();
                    let actual = f64::from(clock_hz) / (per_cycle * (period + 1.0));
                    (
                        period,
                        json!({"source":"midi_note","note":note,"a4_hz":a4_hz,"target_hz":target,
                            "cents_error":1200.0 * (actual / target).log2()}),
                    )
                }
            };
            if !(f64::from(min)..=2047.0).contains(&period) {
                return Err(fail(format!(
                    "period {period} is outside {min}..2047 at this clock"
                )));
            }
            Ok((period as u16, report))
        };
        let reg = |offset: u8, data: u8| (Target::Register(offset), data);
        let mut at_start: Vec<(Target, u8)> = Vec::new();
        let release: Vec<(Target, u8)> = match note.channel {
            0 | 1 => {
                if note.noise.is_some() || note.dpcm.is_some() {
                    return Err(fail(
                        "pulse notes take pitch, volume and optional duty".into(),
                    ));
                }
                let duty = note.duty.unwrap_or(2);
                if duty > 3 {
                    return Err(fail("duty must be 0..3 (12.5%, 25%, 50%, 75%)".into()));
                }
                let (period, pitch_report) = pitch(16.0, 8)?;
                let volume = volume()?;
                let base = note.channel * 4;
                report["period"] = json!(period);
                report["frequency_hz"] = json!(pulse_hz(clock_hz, period));
                report["pitch"] = pitch_report;
                report["duty"] = json!(DUTY_NAMES[usize::from(duty)]);
                report["volume"] = json!(volume);
                at_start.extend([
                    reg(base, (duty << 6) | 0x30 | volume),
                    reg(base + 1, 0x08),
                    reg(base + 2, (period & 0xff) as u8),
                    reg(base + 3, 0x08 | (period >> 8) as u8),
                ]);
                vec![reg(base, (duty << 6) | 0x30)]
            }
            2 => {
                if note.noise.is_some()
                    || note.dpcm.is_some()
                    || note.volume.is_some()
                    || note.duty.is_some()
                {
                    return Err(fail(
                        "triangle notes take pitch only (the triangle has no volume control)"
                            .into(),
                    ));
                }
                let (period, pitch_report) = pitch(32.0, 0)?;
                report["period"] = json!(period);
                report["frequency_hz"] = json!(triangle_hz(clock_hz, period));
                report["pitch"] = pitch_report;
                report["timing"] = json!(
                    "the linear counter loads at the next quarter frame, so the triangle starts and stops up to one quarter frame (about 4.2 ms NTSC) after the note times and holds its last step when stopped"
                );
                if period < 2 {
                    report["ultrasonic"] = json!(
                        "periods 0 and 1 are above 27 kHz and play as an ultrasonic wave the output filters average"
                    );
                }
                at_start.extend([
                    reg(0x08, 0xff),
                    reg(0x0a, (period & 0xff) as u8),
                    reg(0x0b, 0x08 | (period >> 8) as u8),
                ]);
                vec![reg(0x08, 0x80)]
            }
            3 => {
                if note.pitch.is_some() || note.dpcm.is_some() || note.duty.is_some() {
                    return Err(fail(
                        "noise notes take noise (mode, period 0..15) and volume".into(),
                    ));
                }
                let noise = note
                    .noise
                    .as_ref()
                    .ok_or_else(|| fail("noise notes need noise".into()))?;
                if noise.rate.is_some() || noise.divisor.is_some() || noise.shift.is_some() {
                    return Err(fail("NES noise takes mode and period (0..15)".into()));
                }
                let (Some(mode), Some(period)) = (noise.mode, noise.period) else {
                    return Err(fail("NES noise needs mode and period".into()));
                };
                if period > 15 {
                    return Err(fail("noise.period must be 0..15 on the NES".into()));
                }
                let volume = volume()?;
                let periodic = mode == NoiseMode::Periodic;
                let region = config.region.index();
                report["noise"] = json!({"mode":mode,"period":period,
                    "shift_rate_hz":f64::from(clock_hz) / f64::from(NOISE_PERIODS[region][usize::from(period)])});
                report["volume"] = json!(volume);
                at_start.extend([
                    reg(0x0c, 0x30 | volume),
                    reg(0x0e, (u8::from(periodic) << 7) | period),
                    reg(0x0f, 0x08),
                ]);
                vec![reg(0x0c, 0x30)]
            }
            _ => {
                if note.pitch.is_some()
                    || note.noise.is_some()
                    || note.volume.is_some()
                    || note.duty.is_some()
                {
                    return Err(fail("DMC notes take dpcm only".into()));
                }
                let dpcm = note
                    .dpcm
                    .as_ref()
                    .ok_or_else(|| fail("DMC notes need dpcm".into()))?;
                let data = parse_hex(&dpcm.data_hex).map_err(fail)?;
                if data.is_empty() || data.len() > 4081 || data.len() % 16 != 1 {
                    return Err(fail(format!(
                        "dpcm data is {} bytes; the DMC plays 16n + 1 bytes (1..4081)",
                        data.len()
                    )));
                }
                if dpcm.rate > 15 || dpcm.start_level.is_some_and(|l| l > 127) {
                    return Err(fail(
                        "dpcm.rate must be 0..15 and start_level 0..127".into(),
                    ));
                }
                let address = match blocks.iter().find(|b| b.data == data) {
                    Some(block) => block.start,
                    None => {
                        if next_address + data.len() as u32 > 0x10000 {
                            return Err(fail("the DPCM samples do not fit in $C000..$FFFF".into()));
                        }
                        let start = next_address as u16;
                        blocks.push(MemoryBlock {
                            start,
                            data: data.clone(),
                        });
                        next_address = (next_address + data.len() as u32).div_ceil(64) * 64;
                        start
                    }
                };
                report["dpcm"] = json!({"address":format!("${address:04X}"),"bytes":data.len(),"rate":dpcm.rate,
                    "rate_cpu_cycles":DMC_RATES[config.region.index()][usize::from(dpcm.rate)],
                    "looped":dpcm.looped,"start_level":dpcm.start_level});
                at_start.push(reg(0x10, (u8::from(dpcm.looped) << 6) | dpcm.rate));
                if let Some(level) = dpcm.start_level {
                    at_start.push(reg(0x11, level));
                }
                at_start.extend([
                    reg(0x12, ((address - 0xc000) / 64) as u8),
                    reg(0x13, ((data.len() - 1) / 16) as u8),
                    reg(0x15, 0x1f),
                ]);
                vec![reg(0x15, 0x0f)]
            }
        };
        let show = |w: &[(Target, u8)]| {
            w.iter()
                .map(|(t, d)| match t {
                    Target::Register(o) => {
                        json!({"register":register_name(*o),"offset":o,"data":d})
                    }
                    _ => json!({}),
                })
                .collect::<Vec<_>>()
        };
        report["writes"] = json!(show(&at_start));
        report["release_writes"] = json!(show(&release));
        for (target, data) in at_start {
            writes.push((start, 2, index, target, data));
        }
        for (target, data) in release {
            writes.push((end, 1, index, target, data));
        }
        reports.push(report);
    }
    for (i, _) in blocks.iter().enumerate() {
        writes.push((0, 0, NOTE_SETUP.len() + i, Target::MemoryBlock(i as u16), 0));
    }
    // At one tick: setup and memory, then releases, then starts; note order within each.
    writes.sort_by_key(|&(tick, class, index, _, _)| (tick, class, index));
    Ok((
        writes
            .into_iter()
            .map(|(tick, _, _, target, data)| ChipWrite { tick, target, data })
            .collect(),
        reports,
        blocks,
    ))
}

pub fn model_scope() -> Value {
    json!({
        "renderer": "Tunemend NES APU (2A03/2A07) model; not original hardware audio or driver playback",
        "sources": ["NESdev wiki: APU, APU Pulse, APU Envelope, APU Length Counter, APU Triangle, APU Noise, APU DMC, APU Frame Counter, APU Mixer",
            "VGM 1.71 specification (NES APU clock 0x84 with the FDS bit, command 0xB4, data block type 0xC2)"],
        "modelled": ["pulse 1 and 2: duty lookups read in the order 0,7,6..1, timer period t+1 on every second CPU cycle, frequency clock/(16(t+1)); $4003/$4007 reset the phase and restart the envelope",
            "sweep units: target = period +- (period >> shift), pulse 1 adds the ones' complement (-c-1), pulse 2 the two's complement (-c), clamped at 0; t < 8 or target > $7FF mutes even with the sweep disabled; half-frame updates when the divider is 0, the sweep enabled, the shift nonzero and the channel not muted; divider reload on 0 or the reload flag",
            "length counters: the 32-entry table, loaded only while enabled in $4015, clocked on half frames unless halted; $4015 disable clears them",
            "envelopes: start flag, divider period V+1 quarter frames, decay 15..0, loop; constant volume selects V",
            "triangle: CPU-clocked timer, frequency clock/(32(t+1)), 32-step sequence 15..0,0..15 advancing while both the linear and length counters are nonzero, holding its last step otherwise; linear counter reload flag set by $400B, cleared on a quarter frame only when the control flag is clear; ultrasonic periods play unchanged",
            "noise: 15-bit LFSR (bit 0 xor bit 1, or bit 6 in mode 1, into bit 14), period tables for NTSC and PAL in CPU cycles, power-up value 1, output while bit 0 is clear and the length counter is nonzero",
            "DMC: rate tables for NTSC and PAL, output unit (bit 1 adds 2 below 126, bit 0 subtracts 2 above 1, 8-bit shift register, silence on an empty buffer), memory reader from $C000 + 64A for 16L + 1 bytes with $FFFF wrapping to $8000, loop restart, IRQ flag kept in state; $4011 direct load",
            "frame counter: 4-step (quarter frames at CPU cycles 7457, 14913, 22371, 29829, half frames at 14913 and 29829, period 29830 NTSC) and 5-step (37281, period 37282 NTSC) with the PAL values; $4017 resets it and clocks both frames immediately in 5-step mode; the frame IRQ flag is kept in state, not used",
            "$4015 write: channel enables, DMC start only when no bytes remain, DMC stop, DMC IRQ flag cleared; the status read value is reported in state",
            "mixer: pulse_out = 95.88/(8128/(p1+p2)+100) and tnd_out = 159.79/(1/(t/8227+n/12241+d/22638)+100), evaluated exactly per combination"],
        "declared_choices": ["power-on: registers 0, channels disabled, frame counter at cycle 0 in 4-step mode with the IRQ not inhibited, noise LFSR 1, triangle step 0 (output 15), pulse steps 0, DMC level 0, buffer empty, shift register silent with 8 bits remaining, DMC memory 0",
            "one tick is one CPU cycle; pulse timers clock on odd ticks counted from power-on; a native sample is the mean of 16 ticks (clock/16, 111860.8 Hz NTSC)",
            "frame counter events fall on CPU cycle 2n+1 for the documented APU cycle n (the PUT half); the extra one-cycle signal delay and the 3-4 cycle delay of a $4017 write are not modelled: a write resets the counter at its own tick",
            "the output is the mixer level before the console's high-pass and low-pass filters, so it carries DC; mono, equal on both sides",
            "DMC bytes are read from memory written by VGM 0xC2 data blocks or declared memory; unwritten addresses read 0 and are counted in state",
            "writes take effect before the tick of their time, in order; reads (and their side effects) are not modelled"],
        "not_modelled": ["output filters and analog stage", "expansion audio (FDS, VRC6, VRC7, MMC5, Namco 163, Sunsoft 5B)", "DMC DMA CPU stalls and their conflicts with register reads",
            "the $4011 write conflict with a simultaneous output clock", "early RP2A03 revisions (no periodic noise, rate $F 2046) and famiclones with swapped duties",
            "IRQ delivery to a CPU", "a second chip in dual-chip VGM files is rendered as an independent APU with its own memory"],
        "equality": "Equal register writes and clock do not establish that a game, driver or hardware unit sounds the same."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> NesApu {
        NesApu::new(NesConfig::new(NesRegion::Ntsc), Vec::new())
    }

    fn run(apu: &mut NesApu, cycles: u32) {
        for _ in 0..cycles {
            apu.step();
        }
    }

    #[test]
    fn pulse_duty_sequences_and_frequency() {
        // Documented waveforms from the first step after a $4003 write.
        let waveforms = [
            [0, 1, 0, 0, 0, 0, 0, 0],
            [0, 1, 1, 0, 0, 0, 0, 0],
            [0, 1, 1, 1, 1, 0, 0, 0],
            [1, 0, 0, 1, 1, 1, 1, 1],
        ];
        for (duty, expected) in waveforms.iter().enumerate() {
            let mut apu = fresh();
            apu.write_register(0x15, 0x01);
            apu.write_register(0x00, ((duty as u8) << 6) | 0x30 | 15);
            apu.write_register(0x01, 0x08);
            let period = 100u16;
            apu.write_register(0x02, period as u8);
            apu.write_register(0x03, 0x08);
            // Step 0 right after the write, then each following step.
            let mut seen = vec![u8::from(apu.outputs()[0] == 15)];
            let mut last = apu.pulse[0].step;
            while seen.len() < 8 {
                apu.step();
                if apu.pulse[0].step != last {
                    last = apu.pulse[0].step;
                    seen.push(u8::from(apu.outputs()[0] == 15));
                }
            }
            assert_eq!(&seen[..], expected, "duty {duty}");
        }
        // Counted cycles between sequencer steps: 2(t+1); 8 steps per period.
        for period in [8u16, 100, 0x3f9, 2047] {
            let mut apu = fresh();
            apu.write_register(0x15, 0x01);
            apu.write_register(0x00, 0xb0 | 15);
            apu.write_register(0x01, 0x08);
            apu.write_register(0x02, period as u8);
            apu.write_register(0x03, 0x08 | (period >> 8) as u8);
            let mut changes = Vec::new();
            let mut last = apu.pulse[0].step;
            for cycle in 0..40 * (u32::from(period) + 1) {
                apu.step();
                if apu.pulse[0].step != last {
                    changes.push(cycle);
                    last = apu.pulse[0].step;
                }
            }
            let gaps: Vec<u32> = changes.windows(2).map(|w| w[1] - w[0]).collect();
            assert!(
                gaps.iter().all(|&g| g == 2 * (u32::from(period) + 1)),
                "{gaps:?}"
            );
            let hz = f64::from(NTSC_CLOCK_HZ) / (8.0 * f64::from(gaps[0]));
            assert!((hz - pulse_hz(NTSC_CLOCK_HZ, period)).abs() < 1e-9);
        }
        assert!((pulse_hz(NTSC_CLOCK_HZ, 253) - 1789773.0 / 4064.0).abs() < 1e-9);
    }

    #[test]
    fn sweep_targets_and_mutes() {
        let mut apu = fresh();
        for base in [0u8, 4] {
            apu.write_register(base + 1, 0x09); // negate, shift 1, disabled
            apu.write_register(base + 2, 40);
            apu.write_register(base + 3, 0);
        }
        // 40 >> 1 = 20: pulse 1 adds -21, pulse 2 adds -20.
        assert_eq!((apu.pulse[0].target(), apu.pulse[1].target()), (19, 20));
        // Negating with shift 0 clamps at 0 on pulse 1 (-c-1) and reaches 0 on pulse 2.
        apu.write_register(0x01, 0x08);
        apu.write_register(0x05, 0x08);
        assert_eq!((apu.pulse[0].target(), apu.pulse[1].target()), (0, 0));
        // A target above $7FF mutes even with the sweep disabled.
        let mut apu = fresh();
        apu.write_register(0x15, 0x01);
        apu.write_register(0x00, 0xbf);
        apu.write_register(0x01, 0x01);
        apu.write_register(0x02, 0x00);
        apu.write_register(0x03, 0x06);
        assert_eq!(apu.pulse[0].target(), 0x900);
        assert!(apu.pulse[0].muted());
        assert!((0..2000).all(|_| {
            apu.step();
            apu.outputs()[0] == 0
        }));
        apu.write_register(0x01, 0x09);
        assert!(!apu.pulse[0].muted());
        // A period below 8 mutes.
        apu.write_register(0x02, 7);
        apu.write_register(0x03, 0);
        assert!(apu.pulse[0].muted());
        // Enabled sweep, divider period 1, shift 1: the divider starts at 0, so
        // the first half frame updates; then every second half frame.
        let mut apu = fresh();
        apu.write_register(0x15, 0x02);
        apu.write_register(0x04, 0xbf);
        apu.write_register(0x05, 0x91);
        apu.write_register(0x06, 0x00);
        apu.write_register(0x07, 0x01);
        let mut by_half_frame = Vec::new();
        for _ in 0..3 * 29830u32 {
            apu.step();
            if apu.frame_cycle == 14914 || apu.frame_cycle == 0 {
                by_half_frame.push(apu.pulse[1].period);
            }
        }
        assert_eq!(by_half_frame, [0x180, 0x180, 0x240, 0x240, 0x360, 0x360]);
    }

    #[test]
    fn length_table_halt_and_status() {
        assert_eq!(LENGTH_TABLE[0x01], 254);
        assert_eq!(LENGTH_TABLE[0x18], 192);
        assert_eq!(LENGTH_TABLE[0x1f], 30);
        // Index 3 loads 2: silent after the second half frame (cycle 29829).
        let mut apu = fresh();
        apu.write_register(0x15, 0x01);
        apu.write_register(0x00, 0x9f);
        apu.write_register(0x01, 0x08);
        apu.write_register(0x02, 0xfd);
        apu.write_register(0x03, 0x18);
        assert_eq!(apu.status() & 1, 1);
        run(&mut apu, 14914);
        assert_eq!(apu.pulse[0].length, 1);
        run(&mut apu, 29829 - 14914);
        assert_eq!(apu.pulse[0].length, 1);
        apu.step();
        assert_eq!((apu.pulse[0].length, apu.status() & 1), (0, 0));
        // Halt keeps the counter.
        let mut apu = fresh();
        apu.write_register(0x15, 0x08);
        apu.write_register(0x0c, 0x3f);
        apu.write_register(0x0f, 0x18);
        run(&mut apu, 4 * 29830);
        assert_eq!(apu.noise.length, 2);
        // A disabled channel ignores loads; disabling clears.
        let mut apu = fresh();
        let effect = apu.write_register(0x0b, 0x08);
        assert_eq!(effect["length"]["loaded"], false);
        assert_eq!(apu.triangle.length, 0);
        apu.write_register(0x15, 0x04);
        apu.write_register(0x0b, 0x08);
        assert_eq!(apu.triangle.length, 254);
        apu.write_register(0x15, 0x00);
        assert_eq!(apu.triangle.length, 0);
    }

    #[test]
    fn envelope_decays_on_quarter_frames_and_loops() {
        let mut apu = fresh();
        apu.write_register(0x15, 0x01);
        apu.write_register(0x00, 0x82); // decay, V = 2: one step per 3 quarter frames
        apu.write_register(0x01, 0x08);
        apu.write_register(0x02, 0xfd);
        apu.write_register(0x03, 0x08);
        let quarters = [7457u32, 14913, 22371, 29829];
        let mut decays = Vec::new();
        for frame in 0..4u32 {
            for &q in &quarters {
                let target = frame * 29830 + q;
                let now = apu_cycle(&apu);
                run(&mut apu, target - now + 1);
                decays.push(apu.pulse[0].envelope.decay);
            }
        }
        // First quarter frame: start flag -> 15; then 3 quarter frames per step.
        assert_eq!(
            decays,
            [
                15, 15, 15, 14, 14, 14, 13, 13, 13, 12, 12, 12, 11, 11, 11, 10
            ]
        );
        // Loop: after 0 the decay reloads 15.
        let mut env = Envelope {
            start: false,
            divider: 0,
            decay: 0,
        };
        env.clock(true, 0);
        assert_eq!(env.decay, 15);
        env.decay = 0;
        env.clock(false, 0);
        assert_eq!(env.decay, 0);
    }

    /// Cycles since power-on for an APU that has not written $4017.
    fn apu_cycle(apu: &NesApu) -> u32 {
        apu.cycles as u32
    }

    #[test]
    fn triangle_frequency_linear_counter_and_ultrasonic_period() {
        for period in [0u16, 50, 0x3f9] {
            let mut apu = fresh();
            apu.write_register(0x15, 0x04);
            apu.write_register(0x08, 0xff);
            apu.write_register(0x0a, period as u8);
            apu.write_register(0x0b, 0x08 | (period >> 8) as u8);
            run(&mut apu, 7458); // first quarter frame loads the linear counter
            let mut changes = Vec::new();
            let mut last = apu.triangle.step;
            for cycle in 0..70 * (u32::from(period) + 1) {
                apu.step();
                if apu.triangle.step != last {
                    changes.push(cycle);
                    last = apu.triangle.step;
                }
            }
            assert!(
                changes
                    .windows(2)
                    .all(|w| w[1] - w[0] == u32::from(period) + 1)
            );
            let hz = f64::from(NTSC_CLOCK_HZ) / (32.0 * f64::from(changes[1] - changes[0]));
            assert!((hz - triangle_hz(NTSC_CLOCK_HZ, period)).abs() < 1e-9);
        }
        // Period 0 (55.9 kHz) still plays; its mean step value is 7.5.
        let mut apu = fresh();
        apu.write_register(0x15, 0x04);
        apu.write_register(0x08, 0xff);
        apu.write_register(0x0b, 0x08);
        run(&mut apu, 7458);
        let mean: f64 = (0..3200)
            .map(|_| {
                apu.step();
                f64::from(apu.outputs()[2])
            })
            .sum::<f64>()
            / 3200.0;
        assert!((mean - 7.5).abs() < 1e-9, "{mean}");
        // Linear counter: control clear, reload 3; loaded at the first quarter
        // frame, then counted down; the sequencer stops at 0 and holds.
        let mut apu = fresh();
        apu.write_register(0x15, 0x04);
        apu.write_register(0x08, 0x03);
        apu.write_register(0x0a, 10);
        apu.write_register(0x0b, 0x08);
        let mut linear = Vec::new();
        for &q in &[7457u32, 14913, 22371, 29829, 29830 + 7457] {
            let now = apu_cycle(&apu);
            run(&mut apu, q - now + 1);
            linear.push(apu.triangle.linear);
        }
        assert_eq!(linear, [3, 2, 1, 0, 0]);
        assert!(!apu.triangle.reload_flag);
        let held = apu.triangle.step;
        run(&mut apu, 1000);
        assert_eq!(apu.triangle.step, held);
        // Control set: the counter reloads on every quarter frame.
        let mut apu = fresh();
        apu.write_register(0x15, 0x04);
        apu.write_register(0x08, 0x83);
        apu.write_register(0x0b, 0x08);
        run(&mut apu, 4 * 29830);
        assert_eq!((apu.triangle.linear, apu.triangle.reload_flag), (3, true));
    }

    /// Hand trace of the documented rule as a separate formulation: bits are
    /// held in a Vec, feedback taken before the shift.
    fn reference_noise(mode_bit: usize, count: usize) -> String {
        let mut bits = vec![0u8; 15];
        bits[0] = 1;
        let mut out = String::new();
        for _ in 0..count {
            let feedback = bits[0] ^ bits[mode_bit];
            bits.remove(0);
            bits.push(feedback);
            out.push(if bits[0] == 0 { '1' } else { '0' });
        }
        out
    }

    #[test]
    fn noise_first_outputs_in_both_modes() {
        // From the power-up value 1, the first shift feeds 1 into bit 14.
        // Bit 0 stays clear for 14 shifts, is set on the 15th, then clear again.
        assert_eq!(&reference_noise(1, 16), "1111111111111101");
        assert_eq!(&reference_noise(6, 16), "1111111111111101");
        for (mode, tap) in [(0x00u8, 1usize), (0x80, 6)] {
            let mut apu = fresh();
            apu.write_register(0x15, 0x08);
            apu.write_register(0x0c, 0x3f);
            apu.write_register(0x0e, mode); // period index 0: 4 CPU cycles
            apu.write_register(0x0f, 0x08);
            // The first tick clocks at once (timer 0), then every 4 cycles.
            let mut bits = String::new();
            apu.step();
            bits.push(if apu.outputs()[3] == 15 { '1' } else { '0' });
            while bits.len() < 200 {
                run(&mut apu, 4);
                bits.push(if apu.outputs()[3] == 15 { '1' } else { '0' });
            }
            assert_eq!(bits, reference_noise(tap, 200), "mode {mode:#x}");
        }
        // Mode 1 from the power-up value repeats with period 31 (or 93).
        let mut lfsr = Noise {
            lfsr: 1,
            mode: 0x80,
            ..Default::default()
        };
        let mut states = vec![lfsr.lfsr];
        for _ in 0..200 {
            lfsr.clock_lfsr();
            states.push(lfsr.lfsr);
        }
        let period = (1..200).find(|&p| states[100 + p] == states[100]).unwrap();
        assert!(period == 31 || period == 93, "{period}");
        let mut lfsr = Noise {
            lfsr: 1,
            ..Default::default()
        };
        let mut n = 0;
        loop {
            lfsr.clock_lfsr();
            n += 1;
            if lfsr.lfsr == 1 {
                break;
            }
        }
        assert_eq!(n, 32767);
        // Period table use: index 5 shifts every 96 NTSC / 88 PAL CPU cycles.
        for (region, cycles) in [(NesRegion::Ntsc, 96u32), (NesRegion::Pal, 88)] {
            let mut apu = NesApu::new(NesConfig::new(region), Vec::new());
            apu.write_register(0x0e, 5);
            apu.step();
            let first = apu.noise.lfsr;
            run(&mut apu, cycles - 1);
            assert_eq!(apu.noise.lfsr, first);
            apu.step();
            assert_ne!(apu.noise.lfsr, first);
        }
    }

    #[test]
    fn dmc_rates_output_delta_and_memory_reader() {
        assert_eq!(DMC_RATES[0][0], 428);
        assert_eq!(DMC_RATES[0][15], 54);
        assert_eq!(DMC_RATES[1][15], 50);
        // Two bytes of ones from $C040 at rate $F: +2 every 54 cycles.
        let mut sample = vec![0xffu8; 17];
        sample.extend([0x00; 15]);
        let block = MemoryBlock {
            start: 0xc040,
            data: sample,
        };
        let mut apu = NesApu::new(NesConfig::new(NesRegion::Ntsc), vec![block.clone()]);
        apu.apply(Target::MemoryBlock(0), 0);
        apu.write_register(0x10, 0x8f); // IRQ enable, rate $F
        apu.write_register(0x11, 0x10);
        apu.write_register(0x12, 0x01); // $C040
        apu.write_register(0x13, 0x01); // 17 bytes
        assert_eq!(
            (apu.dmc.sample_address, apu.dmc.sample_length),
            (0xc040, 17)
        );
        apu.write_register(0x15, 0x10);
        assert_eq!(apu.status() & 0x10, 0x10);
        // The shift register starts silent: the first 8 output clocks
        // (8 x 54 cycles, the first at tick 0) leave the level alone.
        let mut levels = Vec::new();
        for _ in 0..24 {
            levels.push(apu.dmc.level);
            run(&mut apu, 54);
        }
        assert!(levels[..9].iter().all(|&l| l == 0x10), "{levels:?}");
        let rising: Vec<u8> = levels[9..17].to_vec();
        assert_eq!(rising, (1..=8).map(|k| 0x10 + 2 * k).collect::<Vec<u8>>());
        // Saturation: the level never exceeds 127 (stops below 126 + 2).
        run(&mut apu, 54 * 8 * 20);
        assert!(apu.dmc.level == 126 || apu.dmc.level == 127);
        assert!(apu.dmc.irq_flag, "IRQ flag at the end without loop");
        assert_eq!(apu.status() & 0x90, 0x80);
        assert_eq!(apu.dmc.bytes_read, 17);
        assert_eq!(apu.unwritten_reads, 0);
        // $4015 clears the IRQ flag; $4011 loads the level directly.
        apu.write_register(0x15, 0x00);
        assert!(!apu.dmc.irq_flag);
        apu.write_register(0x11, 0xff);
        assert_eq!(apu.dmc.level, 0x7f);
        // Zeros move down by 2 and stop above 1.
        let mut apu = NesApu::new(
            NesConfig::new(NesRegion::Ntsc),
            vec![MemoryBlock {
                start: 0xc000,
                data: vec![0; 1],
            }],
        );
        apu.apply(Target::MemoryBlock(0), 0);
        apu.write_register(0x10, 0x4f); // loop
        apu.write_register(0x11, 0x05);
        apu.write_register(0x15, 0x10);
        run(&mut apu, 54 * 40);
        assert_eq!(apu.dmc.level, 1);
        assert!(apu.dmc.bytes_read > 3 && !apu.dmc.irq_flag, "looping");
        // The address wraps from $FFFF to $8000, which was never written.
        let mut apu = NesApu::new(NesConfig::new(NesRegion::Ntsc), Vec::new());
        apu.write_register(0x10, 0x0f);
        apu.write_register(0x12, 0xff); // $FFC0
        apu.write_register(0x13, 0x04); // 65 bytes
        apu.write_register(0x15, 0x10);
        run(&mut apu, 54 * 8 * 70);
        assert_eq!(apu.dmc.address, 0x8001);
        assert_eq!(apu.unwritten_reads, 65);
        // Rate index 0 on PAL: 398 CPU cycles per bit.
        let mut apu = NesApu::new(
            NesConfig::new(NesRegion::Pal),
            vec![MemoryBlock {
                start: 0xc000,
                data: vec![0xff],
            }],
        );
        apu.apply(Target::MemoryBlock(0), 0);
        apu.write_register(0x15, 0x10);
        run(&mut apu, 398 * 8 + 1);
        let before = apu.dmc.level;
        run(&mut apu, 397);
        assert_eq!(apu.dmc.level, before);
        apu.step();
        assert_eq!(apu.dmc.level, before + 2);
    }

    #[test]
    fn frame_counter_steps_in_both_modes() {
        for (region, five, expected) in [
            (
                NesRegion::Ntsc,
                false,
                vec![
                    (7457, false),
                    (14913, true),
                    (22371, false),
                    (29829, true),
                    (29830 + 7457, false),
                ],
            ),
            (
                NesRegion::Ntsc,
                true,
                vec![
                    (7457, false),
                    (14913, true),
                    (22371, false),
                    (37281, true),
                    (37282 + 7457, false),
                ],
            ),
            (
                NesRegion::Pal,
                false,
                vec![
                    (8313, false),
                    (16627, true),
                    (24939, false),
                    (33253, true),
                    (33254 + 8313, false),
                ],
            ),
            (
                NesRegion::Pal,
                true,
                vec![
                    (8313, false),
                    (16627, true),
                    (24939, false),
                    (41565, true),
                    (41566 + 8313, false),
                ],
            ),
        ] {
            let mut apu = NesApu::new(NesConfig::new(region), Vec::new());
            apu.write_register(0x15, 0x01);
            // A counter that only half frames clock: length with halt clear.
            apu.write_register(0x00, 0x10); // envelope decay, V = 0: one step per quarter frame
            apu.write_register(0x03, 0x08); // length 254, envelope start
            if five {
                // Written at tick 0: reset plus an immediate quarter and half frame.
                apu.write_register(0x17, 0xc0);
                assert_eq!(
                    (apu.pulse[0].envelope.decay, apu.pulse[0].length),
                    (15, 253)
                );
            }
            let mut events = Vec::new();
            let (mut decay, mut length) = (apu.pulse[0].envelope.decay, apu.pulse[0].length);
            for cycle in 0..expected.last().unwrap().0 + 10 {
                apu.step();
                let (d, l) = (apu.pulse[0].envelope.decay, apu.pulse[0].length);
                if d != decay {
                    events.push((cycle, l != length));
                }
                decay = d;
                length = l;
            }
            assert_eq!(events, expected, "{region:?} five {five}");
        }
        // The 4-step IRQ flag is set at the last step unless inhibited.
        let mut apu = fresh();
        run(&mut apu, 29830);
        assert!(apu.frame_irq);
        apu.write_register(0x17, 0x40);
        assert!(!apu.frame_irq);
        run(&mut apu, 2 * 29830);
        assert!(!apu.frame_irq);
        assert_eq!(apu.status() & 0x40, 0);
    }

    #[test]
    fn mixer_formulas() {
        let tables = mix_tables();
        assert_eq!(tables.pulse[0], 0.0);
        assert!((tables.pulse[1] - 95.88 / 8228.0).abs() < 1e-15);
        assert!((tables.pulse[30] - 95.88 / (8128.0 / 30.0 + 100.0)).abs() < 1e-15);
        assert!((tables.pulse[30] - 0.258_483).abs() < 1e-6);
        let full = tnd_out(15.0, 15.0, 127.0);
        assert!((full - 0.741_516).abs() < 1e-6, "{full}");
        assert_eq!(tables.tnd[0], 0.0);
        assert!((tables.tnd[(15 * 16) * 128] - tnd_out(15.0, 0.0, 0.0)).abs() < 1e-15);
        // The documented lookup approximations agree with the formulas: the
        // TND table is within 4% except 26 DMC-only levels 1..26 (up to 4.7%).
        for n in 1..31 {
            let approx = 95.52 / (8128.0 / n as f64 + 100.0);
            assert!((approx / tables.pulse[n] - 1.0).abs() < 0.005);
        }
        let mut above = 0;
        for t in 0..16 {
            for n in 0..16 {
                for d in 0..128 {
                    let index = 3 * t + 2 * n + d;
                    if index == 0 {
                        continue;
                    }
                    let approx = 163.67 / (24329.0 / index as f64 + 100.0);
                    let exact = tnd_out(t as f64, n as f64, d as f64);
                    let error = (approx / exact - 1.0).abs();
                    assert!(
                        error < 0.047 && (approx - exact).abs() < 0.013,
                        "{t} {n} {d}"
                    );
                    if error >= 0.04 {
                        assert_eq!((t, n), (0, 0));
                        above += 1;
                    }
                }
            }
        }
        assert_eq!(above, 26);
        // A high DMC level lowers the triangle's contribution (nonlinear mix).
        let low = tnd_out(15.0, 0.0, 0.0) - tnd_out(0.0, 0.0, 0.0);
        let high = tnd_out(15.0, 0.0, 120.0) - tnd_out(0.0, 0.0, 120.0);
        assert!(high < 0.8 * low);
        // The tick output is the mixer level times 32767 (power-on: triangle step 0 outputs 15).
        let mut apu = fresh();
        let out = apu.tick();
        let expected = (tnd_out(15.0, 0.0, 0.0) * 32767.0 * 8.0).round() as i32;
        assert_eq!(out, [expected, expected]);
    }

    #[test]
    fn note_writes_map_channels() {
        let note = |channel: u8| PsgNote {
            channel,
            start_seconds: 0.0,
            duration_seconds: 0.1,
            pitch: (channel < 3).then_some(PsgPitch::MidiNote {
                note: 69,
                a4_hz: 440.0,
            }),
            noise: (channel == 3).then_some(crate::psg::PsgNoise {
                mode: Some(NoiseMode::White),
                rate: None,
                period: Some(4),
                divisor: None,
                shift: None,
                taps: None,
            }),
            attenuation: None,
            volume: (channel == 0 || channel == 1 || channel == 3).then_some(12),
            duty: None,
            envelope: None,
            wave: None,
            dpcm: (channel == 4).then(|| NesDpcm {
                data_hex: "ff".repeat(17),
                rate: 15,
                looped: false,
                start_level: Some(64),
            }),
            wavetable: None,
        };
        let notes: Vec<PsgNote> = (0..5).map(note).collect();
        let (writes, reports, blocks) =
            note_writes(NTSC_CLOCK_HZ, NesConfig::new(NesRegion::Ntsc), &notes).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start, 0xc000);
        assert_eq!(reports[0]["period"], 253);
        assert_eq!(reports[2]["period"], 126);
        assert!(writes.windows(2).all(|w| w[0].tick <= w[1].tick));
        let mut apu = NesApu::new(NesConfig::new(NesRegion::Ntsc), blocks);
        for w in writes.iter().filter(|w| w.tick == 0) {
            apu.apply(w.target, w.data);
        }
        assert_eq!(apu.status() & 0x1f, 0x1f);
        let mut bad = note(4);
        bad.dpcm.as_mut().unwrap().data_hex = "ff".repeat(16);
        assert!(note_writes(NTSC_CLOCK_HZ, NesConfig::new(NesRegion::Ntsc), &[bad]).is_err());
    }
}
