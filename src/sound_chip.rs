//! Shared rendering for the PSG-family chip models: timed writes, the tick
//! loop that turns a model into native-rate frames, and the policy that mixes
//! several rendered chips at one output rate.
use crate::ay::{Ay, AyConfig};
use crate::gb_apu::{GbApu, GbConfig};
use crate::huc6280::{Huc6280, PceConfig};
use crate::nes_apu::{MemoryBlock, NesApu, NesConfig};
use crate::psg::{PsgConfig, PsgPort, Rendered, TimedWrite};
use crate::wonderswan::{WonderSwan, WsConfig};
use serde_json::{Value, json};

/// Where one written byte goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The SN76489 data port.
    Psg,
    /// The Game Gear stereo port 0x06.
    Stereo,
    /// An AY register (0..15), a Game Boy register offset from 0xFF10, an
    /// NES offset from $4000, a HuC6280 PSG register (R0..R9) or a
    /// WonderSwan I/O port minus 0x80.
    Register(u8),
    /// The VGM AY8910 stereo mask (command 0x31).
    StereoMask,
    /// A memory image (by index into the chip's block list) copied into the
    /// NES APU's DMC memory; the data byte is unused.
    MemoryBlock(u16),
    /// One byte written to the WonderSwan's internal RAM at this address.
    Memory(u16),
}

/// One byte at a model tick (a native tick of the chip, counted from power-on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipWrite {
    pub tick: u64,
    pub target: Target,
    pub data: u8,
}

/// A chip model driven tick by tick.
pub trait Chip {
    /// Ticks averaged into one native output frame.
    const TICKS_PER_FRAME: u32;
    /// Each tick's output is divided by this (after averaging) to reach PCM16.
    const OUTPUT_DIVISOR: i64;
    fn apply(&mut self, target: Target, data: u8) -> Value;
    /// Leave the channels in `mask` (bit c = channel c) out of the output
    /// mix; registers, counters and phases keep running.
    fn mute_output(&mut self, mask: u8);
    fn tick(&mut self) -> [i32; 2];
    fn state(&self) -> Value;
}

/// Division rounded half away from zero.
fn round_div(value: i64, divisor: i64) -> i64 {
    let q = (2 * value.abs() + divisor) / (2 * divisor);
    if value < 0 { -q } else { q }
}

/// Run a model from power-on through frame `end`, applying writes before the
/// tick they name in list order, and keep frames `start..end`. A frame is the
/// mean of `TICKS_PER_FRAME` consecutive ticks.
pub fn render_chip<C: Chip>(
    mut chip: C,
    writes: &[ChipWrite],
    start: u64,
    end: u64,
) -> Result<Rendered, String> {
    if writes.windows(2).any(|w| w[1].tick < w[0].tick) {
        return Err("writes must be in time order".into());
    }
    let per = u64::from(C::TICKS_PER_FRAME);
    let divisor = i64::from(C::TICKS_PER_FRAME) * C::OUTPUT_DIVISOR;
    let mut frames = Vec::with_capacity(end.saturating_sub(start) as usize);
    let mut effects = Vec::with_capacity(writes.len());
    let mut next = writes.iter().peekable();
    let mut peak = 0;
    for frame in 0..end {
        let mut sum = [0i64; 2];
        for tick in frame * per..(frame + 1) * per {
            while let Some(write) = next.next_if(|w| w.tick == tick) {
                effects.push(chip.apply(write.target, write.data));
            }
            let out = chip.tick();
            sum[0] += i64::from(out[0]);
            sum[1] += i64::from(out[1]);
        }
        if frame >= start {
            let out = sum.map(|v| round_div(v, divisor));
            peak = peak.max(out[0].abs() as i32).max(out[1].abs() as i32);
            frames.push(out.map(|v| v.clamp(-32768, 32767) as i16));
        }
    }
    Ok(Rendered {
        frames,
        effects,
        peak,
        final_state: chip.state(),
    })
}

/// A configured chip family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChipModel {
    Sn(PsgConfig),
    Ay(AyConfig),
    Gb(GbConfig),
    Nes(NesConfig),
    Pce(PceConfig),
    Ws(WsConfig),
}

impl ChipModel {
    pub fn family(&self) -> &'static str {
        match self {
            ChipModel::Sn(_) => "sn76489",
            ChipModel::Ay(_) => "ay8910",
            ChipModel::Gb(_) => "gb_dmg",
            ChipModel::Nes(_) => "nes_apu",
            ChipModel::Pce(_) => "huc6280",
            ChipModel::Ws(_) => "wonderswan",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            ChipModel::Sn(_) => "SN76489-family PSG",
            ChipModel::Ay(c) => c.variant.label(),
            ChipModel::Gb(_) => "Game Boy DMG APU",
            ChipModel::Nes(_) => "NES APU",
            ChipModel::Pce(_) => "HuC6280 PSG",
            ChipModel::Ws(_) => "WonderSwan sound unit",
        }
    }
    /// Input clock cycles per model tick.
    pub fn tick_divider(&self) -> u32 {
        match self {
            ChipModel::Sn(_) => crate::psg::DIVIDER,
            ChipModel::Ay(c) => c.clock_divider,
            ChipModel::Gb(_) => crate::gb_apu::TICK_DIVIDER,
            ChipModel::Nes(_) => crate::nes_apu::TICK_DIVIDER,
            ChipModel::Pce(_) => crate::huc6280::TICK_DIVIDER,
            ChipModel::Ws(_) => crate::wonderswan::TICK_DIVIDER,
        }
    }
    pub fn ticks_per_frame(&self) -> u32 {
        match self {
            ChipModel::Gb(_) => GbApu::TICKS_PER_FRAME,
            ChipModel::Nes(_) => NesApu::TICKS_PER_FRAME,
            ChipModel::Pce(_) => Huc6280::TICKS_PER_FRAME,
            ChipModel::Ws(_) => WonderSwan::TICKS_PER_FRAME,
            _ => 1,
        }
    }
    /// Input clock cycles per native output frame.
    pub fn frame_divider(&self) -> u32 {
        self.tick_divider() * self.ticks_per_frame()
    }
    pub fn config_json(&self) -> Value {
        match self {
            ChipModel::Sn(c) => c.to_json(),
            ChipModel::Ay(c) => c.to_json(),
            ChipModel::Gb(c) => c.to_json(),
            ChipModel::Nes(c) => c.to_json(),
            ChipModel::Pce(c) => c.to_json(),
            ChipModel::Ws(c) => c.to_json(),
        }
    }
    pub fn model_scope(&self) -> Value {
        match self {
            ChipModel::Sn(_) => crate::psg::model_scope(),
            ChipModel::Ay(_) => crate::ay::model_scope(),
            ChipModel::Gb(_) => crate::gb_apu::model_scope(),
            ChipModel::Nes(_) => crate::nes_apu::model_scope(),
            ChipModel::Pce(_) => crate::huc6280::model_scope(),
            ChipModel::Ws(_) => crate::wonderswan::model_scope(),
        }
    }
    /// Render native frames `start..end` from power-on.
    pub fn render(&self, writes: &[ChipWrite], start: u64, end: u64) -> Result<Rendered, String> {
        self.render_with_memory(writes, &[], start, end)
    }
    /// [`ChipModel::render`] with the memory blocks that `Target::MemoryBlock`
    /// writes refer to (NES APU DMC memory; other chips have none).
    pub fn render_with_memory(
        &self,
        writes: &[ChipWrite],
        memory: &[MemoryBlock],
        start: u64,
        end: u64,
    ) -> Result<Rendered, String> {
        self.render_muted(writes, memory, start, end, 0)
    }
    /// The channels this family's output mix sums, in bit order of a mute mask.
    pub fn channel_names(&self) -> &'static [&'static str] {
        match self {
            ChipModel::Sn(_) => &["tone 0", "tone 1", "tone 2", "noise"],
            ChipModel::Ay(_) => &["A", "B", "C"],
            ChipModel::Gb(_) => &["pulse 1", "pulse 2", "wave", "noise"],
            ChipModel::Nes(_) => &["pulse 1", "pulse 2", "triangle", "noise", "dmc"],
            ChipModel::Pce(_) => &[
                "channel 1",
                "channel 2",
                "channel 3",
                "channel 4",
                "channel 5",
                "channel 6",
            ],
            ChipModel::Ws(_) => &["channel 1", "channel 2", "channel 3", "channel 4"],
        }
    }
    /// [`ChipModel::render_with_memory`] with the channels in `mute` (bit c =
    /// channel c of [`ChipModel::channel_names`]) left out of the output mix.
    pub fn render_muted(
        &self,
        writes: &[ChipWrite],
        memory: &[MemoryBlock],
        start: u64,
        end: u64,
        mute: u8,
    ) -> Result<Rendered, String> {
        fn muted<C: Chip>(mut chip: C, mute: u8) -> C {
            chip.mute_output(mute);
            chip
        }
        match self {
            ChipModel::Sn(config) => {
                let timed: Vec<TimedWrite> = writes
                    .iter()
                    .map(|w| TimedWrite {
                        native_sample: w.tick,
                        port: if w.target == Target::Stereo {
                            PsgPort::Stereo
                        } else {
                            PsgPort::Psg
                        },
                        data: w.data,
                    })
                    .collect();
                crate::psg::render_muted(*config, &timed, start, end, mute)
            }
            ChipModel::Ay(config) => render_chip(muted(Ay::new(*config), mute), writes, start, end),
            ChipModel::Gb(config) => {
                render_chip(muted(GbApu::new(*config), mute), writes, start, end)
            }
            ChipModel::Nes(config) => render_chip(
                muted(NesApu::new(*config, memory.to_vec()), mute),
                writes,
                start,
                end,
            ),
            ChipModel::Pce(config) => {
                render_chip(muted(Huc6280::new(*config), mute), writes, start, end)
            }
            ChipModel::Ws(config) => {
                render_chip(muted(WonderSwan::new(*config), mute), writes, start, end)
            }
        }
    }
    /// A decoder for inspection: applies writes without time.
    pub fn decoder(&self) -> Decoder {
        match self {
            ChipModel::Sn(config) => {
                Decoder::Sn(crate::psg::Psg::new(*config).expect("checked config"))
            }
            ChipModel::Ay(config) => Decoder::Ay(Box::new(Ay::new(*config))),
            ChipModel::Gb(config) => Decoder::Gb(Box::new(GbApu::new(*config))),
            ChipModel::Nes(config) => Decoder::Nes(Box::new(NesApu::new(*config, Vec::new()))),
            ChipModel::Pce(config) => Decoder::Pce(Box::new(Huc6280::new(*config))),
            ChipModel::Ws(config) => Decoder::Ws(Box::new(WonderSwan::new(*config))),
        }
    }
}

/// Register trackers that decode writes in order, without running time.
pub enum Decoder {
    Sn(crate::psg::Psg),
    Ay(Box<Ay>),
    Gb(Box<GbApu>),
    Nes(Box<NesApu>),
    Pce(Box<Huc6280>),
    Ws(Box<WonderSwan>),
}

impl Decoder {
    pub fn apply(&mut self, target: Target, data: u8) -> Value {
        match self {
            Decoder::Sn(psg) if target == Target::Stereo => {
                let before = psg.write_stereo(data);
                json!({"register":"stereo","before":format!("0x{before:02x}"),"after":format!("0x{data:02x}")})
            }
            Decoder::Sn(psg) => psg.write(data).to_json(),
            Decoder::Ay(ay) => ay.apply(target, data),
            Decoder::Gb(gb) => gb.apply(target, data),
            Decoder::Nes(nes) => nes.apply(target, data),
            Decoder::Pce(pce) => pce.apply(target, data),
            Decoder::Ws(ws) => ws.apply(target, data),
        }
    }
}

/// Mix several chips' resampled outputs: each source is scaled by 1/N (N
/// sources) and summed, then rounded half away from zero to PCM16. Shorter
/// sources are padded with silence.
pub fn mix(sources: &[Vec<[f64; 2]>]) -> (Vec<[i16; 2]>, Value) {
    let n = sources.len().max(1);
    let gain = 1.0 / n as f64;
    let length = sources.iter().map(Vec::len).max().unwrap_or(0);
    let mut clipped = 0u64;
    let mut peak = 0f64;
    let out = (0..length)
        .map(|i| {
            let mut sum = [0f64; 2];
            for source in sources {
                if let Some(frame) = source.get(i) {
                    sum[0] += frame[0] * gain;
                    sum[1] += frame[1] * gain;
                }
            }
            sum.map(|v| {
                let r = v.round();
                peak = peak.max(r.abs());
                if !(-32768.0..=32767.0).contains(&r) {
                    clipped += 1;
                }
                r.clamp(-32768.0, 32767.0) as i16
            })
        })
        .collect();
    let report = json!({"policy":"each rendered chip is resampled to the output rate with the same kernel, scaled by 1/N for N rendered chips and summed; the sum is rounded half away from zero to PCM16",
        "sources":n,"gain_per_source":gain,"frames":length,"padding":"a shorter source is padded with silence",
        "peak":peak,"clipped_samples":clipped,"header_volume_modifier_applied":false});
    (out, report)
}
