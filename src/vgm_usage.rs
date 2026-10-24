//! Per-chip, per-channel usage of a VGM stream, read from register writes
//! without running the chip models: write and key-on counts, the time each
//! channel is keyed by register state, and channels or ports a file never
//! touches. Informational hints flag data that looks implausible; nothing is
//! changed.
use crate::psg::{Psg, PsgVariant};
use crate::vgm::{Kind, VGM_RATE, Vgm};
use serde_json::{Value, json};

/// Hints reported at most.
const MAX_HINTS: usize = 16;

#[derive(Default, Clone)]
struct Channel {
    name: &'static str,
    writes: u64,
    key_ons: u64,
    first: Option<u64>,
    last: Option<u64>,
    active_since: Option<u64>,
    active_samples: u64,
}

impl Channel {
    fn named(name: &'static str) -> Self {
        Self {
            name,
            ..Default::default()
        }
    }
    fn wrote(&mut self, at: u64) {
        self.writes += 1;
        self.first.get_or_insert(at);
        self.last = Some(at);
    }
    fn set_active(&mut self, active: bool, at: u64) {
        match (self.active_since, active) {
            (None, true) => self.active_since = Some(at),
            (Some(since), false) => {
                self.active_samples += at - since;
                self.active_since = None;
            }
            _ => {}
        }
    }
    fn json(&self, index: usize) -> Value {
        json!({"channel":index,"name":self.name,"writes":self.writes,"key_ons":self.key_ons,
            "active_seconds":self.active_samples as f64 / f64::from(VGM_RATE),
            "first_write_sample":self.first,"last_write_sample":self.last})
    }
}

struct ChipUsage {
    chip: &'static str,
    rule: &'static str,
    channels: Vec<Channel>,
    /// Named registers or ports that are not channels, with write counts.
    ports: Vec<(&'static str, u64)>,
    regs: Vec<u8>,
    extra: serde_json::Map<String, Value>,
}

impl ChipUsage {
    fn new(
        chip: &'static str,
        rule: &'static str,
        names: &[&'static str],
        ports: &[&'static str],
        regs: usize,
    ) -> Self {
        Self {
            chip,
            rule,
            channels: names.iter().map(|n| Channel::named(n)).collect(),
            ports: ports.iter().map(|p| (*p, 0)).collect(),
            regs: vec![0; regs],
            extra: Default::default(),
        }
    }
    fn port(&mut self, name: &str) {
        if let Some(p) = self.ports.iter_mut().find(|p| p.0 == name) {
            p.1 += 1;
        }
    }
    fn finish(mut self, end: u64) -> Value {
        for c in &mut self.channels {
            c.set_active(false, end);
        }
        let unused: Vec<&str> = self
            .channels
            .iter()
            .filter(|c| c.writes == 0)
            .map(|c| c.name)
            .collect();
        let never_keyed: Vec<&str> = self
            .channels
            .iter()
            .filter(|c| c.writes > 0 && c.active_samples == 0)
            .map(|c| c.name)
            .collect();
        let mut ports = serde_json::Map::new();
        for (name, count) in &self.ports {
            ports.insert((*name).to_owned(), json!(count));
        }
        let mut value = json!({"chip":self.chip,"active_rule":self.rule,
            "channels":self.channels.iter().enumerate().map(|(i, c)| c.json(i)).collect::<Vec<_>>(),
            "unused_channels":unused,"written_but_never_keyed":never_keyed,"ports":ports,
            "unused_ports":self.ports.iter().filter(|p| p.1 == 0).map(|p| p.0).collect::<Vec<_>>()});
        for (k, v) in self.extra {
            value[k] = v;
        }
        value
    }
}

const SN_RULE: &str = "keyed while the attenuation is below 15";
const AY_RULE: &str = "keyed while the amplitude is nonzero (or envelope mode) and the mixer enables the channel's tone or noise";
const GB_RULE: &str = "keyed from a trigger with the DAC on until the DAC or power goes off; length timers and envelope decay are ignored, so this is an upper bound";
const PCE_RULE: &str = "keyed while R4 chON is set and the channel's amplitude (main level, AL and balance) is above the 45 dB cut-off on either side; waveform, noise and DDA content are not inspected";
const WS_RULE: &str = "keyed while the channel's enable bit in $90 is set and its volume ($88..$8B) is nonzero on either side; channel 2 also while voice mode ($90 bit 5) is on with a nonzero $94; wavetable and sample content are not inspected";
const NES_RULE: &str = "keyed from a length load while enabled in $4015 until it is disabled, constant volume 0 is written (pulse, noise) or the linear reload value is 0 (triangle); the DMC from a $4015 start for its sample length unless looped or stopped; length counters, envelopes and the linear counter are ignored, so this is an upper bound";

/// Usage of every chip family the stream writes to.
pub fn usage(vgm: &Vgm) -> Value {
    let end = vgm.total_samples();
    let mut hints: Vec<Value> = Vec::new();
    let sn_config = vgm
        .psg_config()
        .map(|c| c.0)
        .unwrap_or_else(|_| PsgVariant::Sega.config());
    let mut sn = Psg::new(sn_config).expect("checked config");
    let mut sn_usage: Option<ChipUsage> = None;
    let mut sn_period_at_wait: [Option<u16>; 3] = [None; 3];
    let mut ay: [Option<ChipUsage>; 2] = [None, None];
    let mut gb: [Option<(ChipUsage, [bool; 4])>; 2] = [None, None];
    let mut nes: [Option<(ChipUsage, NesKeys)>; 2] = [None, None];
    let nes_clock = f64::from(
        vgm.nes_config()
            .map_or(crate::nes_apu::NTSC_CLOCK_HZ, |n| n.1),
    );
    let nes_region = vgm.nes_config().map_or(0, |n| {
        usize::from(n.0.region == crate::nes_apu::NesRegion::Pal)
    });
    let mut pce: [Option<(ChipUsage, PceKeys)>; 2] = [None, None];
    let mut ws: [Option<(ChipUsage, WsKeys)>; 2] = [None, None];
    let mut other: std::collections::BTreeMap<&'static str, u64> = Default::default();
    let mut data_blocks: Vec<Value> = Vec::new();
    for (index, c) in vgm.commands.iter().enumerate() {
        let raw = &vgm.bytes[c.offset..c.offset + c.length];
        let at = c.at;
        // A DMC sample that is not looped ends by itself at its computed time.
        for (u, keys) in nes.iter_mut().flatten() {
            if let Some(e) = keys.dmc_end.filter(|&e| e <= at) {
                u.channels[4].set_active(false, e);
                keys.dmc_end = None;
            }
        }
        match c.kind {
            Kind::PsgWrite => {
                let u = sn_usage.get_or_insert_with(|| {
                    ChipUsage::new(
                        "sn76489",
                        SN_RULE,
                        &["tone0", "tone1", "tone2", "noise"],
                        &["game_gear_stereo"],
                        0,
                    )
                });
                let effect = sn.write(raw[1]);
                let channel = usize::from(effect.register >> 1);
                u.channels[channel].wrote(at);
                if effect.register & 1 == 1 {
                    let was = effect.before < 15;
                    let now = effect.after < 15;
                    if now && !was {
                        u.channels[channel].key_ons += 1;
                    }
                    u.channels[channel].set_active(now, at);
                }
            }
            Kind::PsgStereo => {
                sn_usage
                    .get_or_insert_with(|| {
                        ChipUsage::new(
                            "sn76489",
                            SN_RULE,
                            &["tone0", "tone1", "tone2", "noise"],
                            &["game_gear_stereo"],
                            0,
                        )
                    })
                    .port("game_gear_stereo");
            }
            Kind::Wait if sn_usage.is_some() => {
                // Compare each tone period in effect when time advances.
                let state = sn.state();
                for channel in 0..3 {
                    let period = state["tone"][channel].as_u64().map(|p| p as u16);
                    if let (Some(prev), Some(now)) = (sn_period_at_wait[channel], period)
                        && hints.len() < MAX_HINTS
                        && prev >= 64
                        && now < prev / 4
                        && (f64::from(now + 1024) / f64::from(prev) - 1.0).abs() < 0.25
                    {
                        hints.push(json!({"chip":"sn76489","channel":channel,"command_index":index,"at_sample":at,
                            "period_before":prev,"period":now,"period_plus_1024":now + 1024,
                            "note":"the tone period dropped to a value close to the earlier one minus 1024; the 10-bit register may have wrapped an intended period above 1023. Informational only; the data is unchanged"}));
                    }
                    sn_period_at_wait[channel] = period;
                }
            }
            Kind::AyWrite => {
                let second = usize::from(raw[1] >> 7);
                let u = ay[second].get_or_insert_with(|| {
                    ChipUsage::new(
                        if second == 0 { "ay8910" } else { "ay8910_2" },
                        AY_RULE,
                        &["a", "b", "c"],
                        &["mixer", "noise_period", "envelope", "io_ports"],
                        16,
                    )
                });
                let reg = usize::from(raw[1] & 0x7f);
                if reg < 16 {
                    u.regs[reg] = raw[2];
                }
                match reg {
                    0..=5 => u.channels[reg / 2].wrote(at),
                    6 => u.port("noise_period"),
                    7 => u.port("mixer"),
                    8..=10 => u.channels[reg - 8].wrote(at),
                    11..=13 => u.port("envelope"),
                    14 | 15 => u.port("io_ports"),
                    _ => {}
                }
                for channel in 0..3 {
                    let amplitude = u.regs[8 + channel] & 0x1f != 0;
                    let tone = u.regs[7] & (1 << channel) == 0;
                    let noise = u.regs[7] & (8 << channel) == 0;
                    let active = amplitude && (tone || noise);
                    if active && u.channels[channel].active_since.is_none() {
                        u.channels[channel].key_ons += 1;
                    }
                    u.channels[channel].set_active(active, at);
                }
            }
            Kind::GbWrite => {
                let second = usize::from(raw[1] >> 7);
                let (u, triggered) = gb[second].get_or_insert_with(|| {
                    let mut u = ChipUsage::new(
                        if second == 0 { "gb_dmg" } else { "gb_dmg_2" },
                        GB_RULE,
                        &["pulse1", "pulse2", "wave", "noise"],
                        &["nr50_nr51", "nr52", "wave_ram"],
                        0x30,
                    );
                    u.regs[0x16] = 0x80;
                    (u, [false; 4])
                });
                let offset = usize::from(raw[1] & 0x7f);
                let data = raw[2];
                if offset < 0x30 {
                    let power = u.regs[0x16] & 0x80 != 0 || offset == 0x16;
                    if power || (0x20..0x30).contains(&offset) {
                        u.regs[offset] = data;
                    }
                }
                match offset {
                    0x00..=0x13 => {
                        let channel = [0x00usize, 0x06, 0x0a, 0x10]
                            .iter()
                            .rposition(|&b| offset >= b)
                            .unwrap_or(0);
                        u.channels[channel].wrote(at);
                        if offset == [0x04, 0x09, 0x0e, 0x13][channel] && data & 0x80 != 0 {
                            u.channels[channel].key_ons += 1;
                            triggered[channel] = true;
                        }
                    }
                    0x14 | 0x15 => u.port("nr50_nr51"),
                    0x16 => {
                        u.port("nr52");
                        if data & 0x80 == 0 {
                            *triggered = [false; 4];
                            u.regs[..0x16].fill(0);
                        }
                    }
                    0x20..=0x2f => u.port("wave_ram"),
                    _ => {}
                }
                let power = u.regs[0x16] & 0x80 != 0;
                for channel in 0..4 {
                    let dac = match channel {
                        2 => u.regs[0x0a] & 0x80 != 0,
                        _ => u.regs[[0x02, 0x07, 0, 0x11][channel]] & 0xf8 != 0,
                    };
                    if !dac || !power {
                        triggered[channel] = false;
                    }
                    u.channels[channel].set_active(triggered[channel], at);
                }
            }
            Kind::NesWrite => {
                let second = usize::from(raw[1] >> 7);
                let (u, keys) = nes[second].get_or_insert_with(|| {
                    (
                        ChipUsage::new(
                            if second == 0 { "nes_apu" } else { "nes_apu_2" },
                            NES_RULE,
                            &["pulse1", "pulse2", "triangle", "noise", "dmc"],
                            &[
                                "status_4015",
                                "frame_counter_4017",
                                "fds_expansion",
                                "other_4014_4016_4018",
                            ],
                            0x20,
                        ),
                        NesKeys::default(),
                    )
                });
                let reg = usize::from(raw[1] & 0x7f);
                let data = raw[2];
                if reg >= 0x20 {
                    u.port("fds_expansion");
                    continue;
                }
                u.regs[reg] = data;
                match reg {
                    0x00..=0x0f => {
                        let channel = reg / 4;
                        u.channels[channel].wrote(at);
                        if reg % 4 == 3 && keys.enabled[channel] {
                            u.channels[channel].key_ons += 1;
                            keys.loaded[channel] = true;
                        }
                    }
                    0x10..=0x13 => u.channels[4].wrote(at),
                    0x15 => {
                        u.port("status_4015");
                        for channel in 0..5 {
                            keys.enabled[channel] = data & (1 << channel) != 0;
                            // The DMC is tracked by `dmc_end`, not a length load.
                            if channel < 4 && !keys.enabled[channel] {
                                keys.loaded[channel] = false;
                            }
                        }
                        if data & 0x10 != 0 && keys.dmc_end.is_none() {
                            let rate = crate::nes_apu::DMC_RATES[nes_region]
                                [usize::from(u.regs[0x10] & 15)];
                            let bytes = u16::from(u.regs[0x13]) * 16 + 1;
                            let seconds = f64::from(bytes) * 8.0 * f64::from(rate) / nes_clock;
                            keys.dmc_end = Some(if u.regs[0x10] & 0x40 != 0 {
                                u64::MAX
                            } else {
                                at + (seconds * f64::from(VGM_RATE)).ceil() as u64
                            });
                            u.channels[4].key_ons += 1;
                            u.channels[4].set_active(true, at);
                        } else if data & 0x10 == 0 {
                            keys.dmc_end = None;
                            u.channels[4].set_active(false, at);
                        }
                    }
                    0x17 => u.port("frame_counter_4017"),
                    _ => u.port("other_4014_4016_4018"),
                }
                for channel in 0..4 {
                    let audible = match channel {
                        2 => u.regs[0x08] & 0x7f != 0,
                        _ => u.regs[channel * 4] & 0x1f != 0x10,
                    };
                    u.channels[channel].set_active(keys.loaded[channel] && audible, at);
                }
            }
            Kind::PceWrite => {
                let second = usize::from(raw[1] >> 7);
                let (u, keys) = pce[second].get_or_insert_with(|| {
                    (
                        ChipUsage::new(
                            if second == 0 { "huc6280" } else { "huc6280_2" },
                            PCE_RULE,
                            &["ch1", "ch2", "ch3", "ch4", "ch5", "ch6"],
                            &[
                                "channel_select",
                                "main_amplitude",
                                "lfo_frequency",
                                "lfo_control",
                                "undefined_0a_0f",
                            ],
                            0,
                        ),
                        PceKeys::default(),
                    )
                });
                let reg = raw[1] & 0x7f;
                let data = raw[2];
                let channel = usize::from(keys.select);
                match reg {
                    0 => {
                        u.port("channel_select");
                        keys.select = data & 7;
                    }
                    1 => {
                        u.port("main_amplitude");
                        keys.main = data;
                    }
                    2..=7 if channel < 6 => {
                        u.channels[channel].wrote(at);
                        match reg {
                            4 => keys.control[channel] = data,
                            5 => keys.balance[channel] = data,
                            6 if keys.control[channel] & 0x40 != 0 => keys.dda[channel] += 1,
                            6 if keys.control[channel] & 0xc0 == 0 => keys.wave[channel] += 1,
                            7 if channel >= 4 && data & 0x80 != 0 => keys.noise[channel] = true,
                            _ => {}
                        }
                    }
                    2..=7 => {}
                    8 => u.port("lfo_frequency"),
                    9 => {
                        u.port("lfo_control");
                        if data & 3 != 0 {
                            keys.lfo = true;
                        }
                    }
                    _ => u.port("undefined_0a_0f"),
                }
                for ch in 0..6 {
                    let control = keys.control[ch];
                    let audible = control & 0x80 != 0
                        && [
                            (keys.main >> 4, keys.balance[ch] >> 4),
                            (keys.main & 15, keys.balance[ch] & 15),
                        ]
                        .iter()
                        .any(|&(m, b)| crate::huc6280::amplitude(m, control & 31, b).is_some());
                    if audible && u.channels[ch].active_since.is_none() {
                        u.channels[ch].key_ons += 1;
                    }
                    u.channels[ch].set_active(audible, at);
                }
            }
            Kind::WsWrite | Kind::WsMemoryWrite => {
                let memory = c.kind == Kind::WsMemoryWrite;
                let second = if memory { 0 } else { usize::from(raw[1] >> 7) };
                let (u, keys) = ws[second].get_or_insert_with(|| {
                    (
                        ChipUsage::new(
                            if second == 0 {
                                "wonderswan"
                            } else {
                                "wonderswan_2"
                            },
                            WS_RULE,
                            &["ch1", "ch2", "ch3", "ch4"],
                            &[
                                "wavetable_address_8f",
                                "channel_control_90",
                                "output_control_91",
                                "noise_control_8e",
                                "sweep_8c_8d",
                                "voice_volume_94",
                                "sound_test_95",
                                "speaker_volume_9e",
                                "read_only_or_undefined",
                                "internal_ram",
                            ],
                            0x20,
                        ),
                        WsKeys::default(),
                    )
                });
                if memory {
                    u.port("internal_ram");
                    continue;
                }
                let port = usize::from(raw[1] & 0x7f);
                let data = raw[2];
                match port {
                    0x00..=0x07 => u.channels[port / 2].wrote(at),
                    0x08..=0x0b => u.channels[port - 8].wrote(at),
                    0x0c | 0x0d => u.port("sweep_8c_8d"),
                    0x0e => u.port("noise_control_8e"),
                    0x0f => u.port("wavetable_address_8f"),
                    0x10 => {
                        u.port("channel_control_90");
                        keys.voice |= data & 0x20 != 0;
                        keys.sweep |= data & 0x44 == 0x44;
                        keys.noise |= data & 0x88 == 0x88;
                    }
                    0x11 => u.port("output_control_91"),
                    0x14 => {
                        u.port("voice_volume_94");
                        u.channels[1].wrote(at);
                    }
                    0x15 => u.port("sound_test_95"),
                    0x1e => u.port("speaker_volume_9e"),
                    _ => u.port("read_only_or_undefined"),
                }
                if port < 0x20 && !matches!(port, 0x12 | 0x13 | 0x16..=0x1d | 0x1f) {
                    u.regs[port] = data;
                }
                let control = u.regs[0x10];
                for ch in 0..4 {
                    let wave = control & (1 << ch) != 0 && u.regs[0x08 + ch] != 0;
                    let voice = ch == 1 && control & 0x20 != 0 && u.regs[0x14] & 0x0f != 0;
                    let audible = wave || voice;
                    if audible && u.channels[ch].active_since.is_none() {
                        u.channels[ch].key_ons += 1;
                    }
                    u.channels[ch].set_active(audible, at);
                }
            }
            Kind::DataBlock => {
                if let Some((kind, second, payload)) = vgm.data_block(index) {
                    data_blocks.push(json!({"command_index":index,"type":format!("0x{kind:02x}"),"bytes":payload.len(),"second_chip":second}));
                }
            }
            Kind::ChipWrite
            | Kind::SecondPsgWrite
            | Kind::SecondPsgStereo
            | Kind::Ym2612DacWait
            | Kind::DacStream
            | Kind::PcmRamWrite => {
                let name = match c.kind {
                    Kind::SecondPsgWrite | Kind::SecondPsgStereo => "second SN76489",
                    Kind::Ym2612DacWait => "YM2612 DAC (0x8n)",
                    Kind::DacStream => "DAC stream control",
                    Kind::PcmRamWrite => "PCM RAM write",
                    _ => crate::vgm::chip_name(raw[0]),
                };
                *other.entry(name).or_default() += 1;
            }
            _ => {}
        }
    }
    let mut chips = Vec::new();
    if let Some(u) = sn_usage {
        chips.push(u.finish(end));
    }
    for u in ay.into_iter().flatten() {
        chips.push(u.finish(end));
    }
    for (u, _) in gb.into_iter().flatten() {
        chips.push(u.finish(end));
    }
    for (u, keys) in nes.into_iter().flatten() {
        let mut u = u;
        if let Some(e) = keys.dmc_end {
            u.channels[4].set_active(false, e.min(end));
        }
        chips.push(u.finish(end));
    }
    for (u, keys) in pce.into_iter().flatten() {
        let mut u = u;
        u.extra.insert("waveform_writes".into(), json!(keys.wave));
        u.extra.insert("dda_writes".into(), json!(keys.dda));
        u.extra.insert(
            "noise_enabled_channels".into(),
            json!(
                (0..6)
                    .filter(|&c| keys.noise[c])
                    .map(|c| format!("ch{}", c + 1))
                    .collect::<Vec<_>>()
            ),
        );
        u.extra.insert("lfo_enabled".into(), json!(keys.lfo));
        chips.push(u.finish(end));
    }
    for (u, keys) in ws.into_iter().flatten() {
        let mut u = u;
        u.extra.insert("voice_mode_used".into(), json!(keys.voice));
        u.extra.insert("sweep_used".into(), json!(keys.sweep));
        u.extra.insert("noise_mode_used".into(), json!(keys.noise));
        chips.push(u.finish(end));
    }
    json!({"chips":chips,"other_chip_commands":other,"data_blocks":data_blocks,"hints":hints,
        "scope":"one pass through the stream (no loop repeats); counts come from register writes, not from running the models, and say nothing about how the result sounds"})
}

#[derive(Default)]
struct WsKeys {
    voice: bool,
    sweep: bool,
    noise: bool,
}

#[derive(Default)]
struct PceKeys {
    select: u8,
    main: u8,
    control: [u8; 6],
    balance: [u8; 6],
    wave: [u64; 6],
    dda: [u64; 6],
    noise: [bool; 6],
    lfo: bool,
}

#[derive(Default)]
struct NesKeys {
    enabled: [bool; 5],
    loaded: [bool; 4],
    dmc_end: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_4015_write_counts_keyed_channels_without_the_dmc_slot() {
        let mut body = Vec::new();
        for (reg, data) in [(0x15, 0x0f), (0x00, 0xbf), (0x02, 0xfd), (0x03, 0x00)] {
            body.extend([0xb4, reg, data]);
        }
        body.extend([0x61, 0x44, 0xac, 0xb4, 0x15, 0x00, 0x66]);
        let mut bytes = crate::vgm::tests::synthetic(0x161, 0xc0, &body, false);
        bytes[0x84..0x88].copy_from_slice(&1_789_773u32.to_le_bytes());
        let report = usage(&Vgm::parse(&bytes).unwrap());
        let nes = &report["chips"][0];
        assert_eq!(nes["chip"], "nes_apu");
        assert_eq!(nes["channels"][0]["key_ons"], 1);
        assert_eq!(
            nes["unused_channels"],
            json!(["pulse2", "triangle", "noise", "dmc"])
        );
    }
}
