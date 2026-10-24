//! VGM inspection and PSG-family auditions (SN76489, AY-3-8910/YM2149, the
//! Game Boy DMG APU, the NES APU, the HuC6280 PSG and the WonderSwan sound
//! unit) through the built-in models.
use super::*;
use crate::ay::{AyConfig, AyVariant};
use crate::gb_apu::GbConfig;
use crate::huc6280::PceConfig;
use crate::nes_apu::{MemoryBlock, NesConfig, NesRegion};
use crate::psg::{
    MAX_EVENTS, MAX_RENDER_SECONDS, PsgNote, PsgPort, PsgVariant, PsgWriteInput, check_clock,
    explicit_writes, note_writes, resample_divided, resample_exact,
};
use crate::sound_chip::{ChipModel, ChipWrite, Decoder, Target, mix};
use crate::vgm::{Kind, VGM_RATE, Vgm, VgmError};
use crate::wonderswan::WsConfig;

const DEFAULT_TAIL_SECONDS: f64 = 0.5;
const MAX_TAIL_SECONDS: f64 = 30.0;
const MAX_LOOP_PASSES: u32 = 16;
/// A VGM tail stops early only after 0.1 s of constant output.
const TAIL_SETTLE_SAMPLES: u64 = 4410;
/// Applied writes copied into the tool response; the report file has all.
const RESPONSE_WRITES: usize = 32;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VgmView {
    /// Version, clocks, settings of the modelled chips, totals, loop, byte layout, command kinds, GD3.
    #[default]
    Summary,
    /// Every header field with offset, raw bytes, value and whether this version defines it.
    Header,
    /// Paged commands with exact offsets, bytes and decoded meaning.
    Commands,
    /// Every command kind present, with counts, opcodes and what render_psg does with it.
    Kinds,
    /// The decoded GD3 tag.
    Gd3,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectVgmInput {
    /// A sound source retained with load_instrument_source.
    pub source: String,
    #[serde(default)]
    pub view: VgmView,
    /// Commands view: index of the first command to return, counted in the
    /// (`kind`-filtered) command list; it is a command index, not a byte
    /// offset. Each returned item gives its `index` and its byte `offset` in
    /// the file; continue with `next_cursor`.
    #[serde(default)]
    pub offset: usize,
    /// Commands per page, default 100, at most 500.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Commands view: only this kind, such as `psg_write`, `ay8910_write`,
    /// `gameboy_dmg_write`, `nes_apu_write`, `huc6280_write`, `wonderswan_write`,
    /// `wonderswan_memory_write`, `data_block` or `other_chip_write`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// A chip variant for explicit writes and notes; nothing is inferred from a
/// console name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChipVariant {
    /// SN76489 core in Sega VDPs (Master System, Game Gear, Mega Drive).
    Sega,
    /// Discrete SN76489/SN76489AN (SG-1000, SC-3000H, BBC Micro, ColecoVision).
    Sn76489,
    /// General Instrument AY-3-8910 (also AY-3-8912/8913): 16-step envelope,
    /// 3 dB DAC steps (ZX Spectrum 128, Amstrad CPC, MSX, Vectrex).
    Ay8910,
    /// Yamaha YM2149 (also YM3439): 32-step envelope, 1.5 dB DAC steps
    /// (Atari ST, some MSX machines). Give the clock reaching the sound core.
    Ym2149,
    /// Game Boy DMG APU (LR35902), clock 4194304.
    Dmg,
    /// NES APU with the RP2A03 (NTSC) noise, DMC and frame counter tables,
    /// clock 1789773.
    NesApu,
    /// NES APU with the RP2A07 (PAL) tables, clock 1662607.
    NesApuPal,
    /// HuC6280 PSG (PC Engine, TurboGrafx-16), clock 3579545 (the 7.16 MHz
    /// master clock / 2, as VGM files give it).
    Huc6280,
    /// WonderSwan sound unit (WonderSwan, WonderSwan Color, SwanCrystal),
    /// clock 3072000; Hyper Voice is not modelled.
    Wonderswan,
}

impl ChipVariant {
    fn model(self, clock_hz: u32) -> ChipModel {
        match self {
            ChipVariant::Sega => ChipModel::Sn(PsgVariant::Sega.config()),
            ChipVariant::Sn76489 => ChipModel::Sn(PsgVariant::Sn76489.config()),
            ChipVariant::Ay8910 => ChipModel::Ay(AyConfig::new(AyVariant::Ay8910)),
            ChipVariant::Ym2149 => ChipModel::Ay(AyConfig::new(AyVariant::Ym2149)),
            ChipVariant::Dmg => ChipModel::Gb(GbConfig::dmg()),
            ChipVariant::NesApu => ChipModel::Nes(NesConfig::new(NesRegion::Ntsc)),
            ChipVariant::NesApuPal => ChipModel::Nes(NesConfig::new(NesRegion::Pal)),
            ChipVariant::Huc6280 => ChipModel::Pce(PceConfig::new(clock_hz)),
            ChipVariant::Wonderswan => ChipModel::Ws(WsConfig::new(clock_hz)),
        }
    }
}

/// A chip family a VGM can declare.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ChipFamily {
    Sn76489,
    Ay8910,
    GbDmg,
    NesApu,
    Huc6280,
    Wonderswan,
}

impl ChipFamily {
    fn name(self) -> &'static str {
        match self {
            ChipFamily::Sn76489 => "sn76489",
            ChipFamily::Ay8910 => "ay8910",
            ChipFamily::GbDmg => "gb_dmg",
            ChipFamily::NesApu => "nes_apu",
            ChipFamily::Huc6280 => "huc6280",
            ChipFamily::Wonderswan => "wonderswan",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PsgChip {
    pub variant: ChipVariant,
    /// Input clock, such as 3579545 (SN76489 NTSC), 3546893 (PAL), 1789773
    /// (MSX AY-3-8910), 2000000 (Atari ST YM2149), 4194304 (Game Boy),
    /// 1789773 (NES NTSC), 1662607 (NES PAL), 3579545 (HuC6280) or 3072000
    /// (WonderSwan).
    pub clock_hz: u32,
}

/// A range of the unrolled VGM timeline in 44100 Hz VGM samples.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VgmRange {
    pub start_sample: u64,
    /// Exclusive end.
    pub end_sample: u64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderPsgInput {
    pub job_id: String,
    /// A retained VGM sound source (load_instrument_source). Its header declares
    /// the chips, clocks and variants. Exactly one of `vgm`, `writes` or `notes`.
    #[serde(default)]
    pub vgm: Option<String>,
    /// VGM only: the part of the unrolled timeline to output. Earlier writes are
    /// still applied, so the chip state and phases are exact. Default: everything.
    #[serde(default)]
    pub range: Option<VgmRange>,
    /// VGM only: passes through the header loop, 1..16. 1 plays the file once;
    /// each further pass replays from the loop offset. Default 1.
    #[serde(default)]
    pub loop_passes: Option<u32>,
    /// VGM only: render just these declared chip families (sn76489, ay8910,
    /// gb_dmg, nes_apu, huc6280, wonderswan); the others are reported as
    /// skipped.
    /// Default: every declared family Tunemend models.
    #[serde(default)]
    pub families: Option<Vec<ChipFamily>>,
    /// VGM only: multiply every rendered chip's native samples by the header
    /// volume modifier gain 2^(v/32) (0x7C, read from version 1.50). Default
    /// false: the modifier is reported, not applied.
    #[serde(default)]
    pub apply_volume_modifier: Option<bool>,
    /// Explicit writes or notes only: the chip variant and clock; nothing is inferred.
    #[serde(default)]
    pub chip: Option<PsgChip>,
    /// Explicit bytes in time order: SN76489 PSG or Game Gear stereo port
    /// bytes, AY/Game Boy/NES/HuC6280/WonderSwan register writes (`register`),
    /// or WonderSwan internal RAM bytes (`address`).
    #[serde(default)]
    pub writes: Option<Vec<PsgWriteInput>>,
    /// NES explicit writes only: bytes placed in the DMC's view of CPU memory
    /// before the first write (the DMC fetches samples from $C000 + 64A).
    #[serde(default)]
    pub dmc_memory: Option<Vec<DmcMemoryInput>>,
    /// Notes turned into register writes; notes on one channel must not overlap.
    #[serde(default)]
    pub notes: Option<Vec<PsgNote>>,
    /// Explicit writes or notes: time rendered after the last write, default
    /// 0.5. VGM: time rendered after the end of the stream with no further
    /// writes, stopping early once every rendered chip's output stays
    /// constant; only when the range reaches the end; default none (the
    /// render stops at the stream end). At most 30.
    #[serde(default)]
    pub tail_seconds: Option<f64>,
    /// Optional WAV at this rate (8000..192000), made by a reported windowed-sinc
    /// step from the native rate. Required when a VGM renders several chips:
    /// their mix is written at this rate.
    #[serde(default)]
    pub output_sample_rate: Option<u32>,
    /// Write native-rate WAVs too when `output_sample_rate` is given (one per
    /// chip when several render). Without `output_sample_rate` a single chip's
    /// native WAV is always written.
    #[serde(default)]
    pub include_native_wav: Option<bool>,
    /// Channels to hear per rendered chip, for measuring or auditioning one
    /// part. Channel numbers are 0-based: SN76489 0..2 tones, 3 noise; AY 0..2
    /// (A, B, C); Game Boy 0 pulse 1, 1 pulse 2, 2 wave, 3 noise; NES 0 pulse
    /// 1, 1 pulse 2, 2 triangle, 3 noise, 4 DMC; HuC6280 0..5 (hardware
    /// channels 1..6); WonderSwan 0..3 (channels 1..4). A muted channel is left
    /// out of the model's output mix only: every write still applies and
    /// registers, counters, envelopes and phases run exactly as without the
    /// selection; a multi-chip mix keeps its 1/N gain. render.json reports
    /// `channel_selection`. Default: every channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<ChannelSelection>>,
    /// New folder relative to the workspace, such as `out/psg-audition`.
    pub destination: String,
}

/// The channels of one rendered chip to hear: give exactly one of `solo` or
/// `mute`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelSelection {
    /// The rendered chip as render.json names it: sn76489, ay8910, ay8910_2,
    /// gb_dmg, gb_dmg_2, nes_apu, nes_apu_2, huc6280, huc6280_2, wonderswan or
    /// wonderswan_2 (the `_2` ids are the second chip of a dual-chip VGM).
    /// Optional when one chip renders; required when several do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chip: Option<String>,
    /// Hear only these channels; the chip's other channels are muted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solo: Option<Vec<u8>>,
    /// Mute these channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute: Option<Vec<u8>>,
}

/// Bytes for the NES DMC's memory view.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DmcMemoryInput {
    /// CPU address of the first byte, such as 49152 ($C000).
    pub address: u16,
    /// The bytes as hexadecimal, two digits per byte; spaces allowed.
    pub data_hex: String,
}

pub(super) fn located(field: &str, error: VgmError) -> ToolResponse {
    let mut response = ToolResponse::refused(
        Code::InvalidRange,
        &format!("{field}/bytes/{}", error.offset),
        format!(
            "byte {} (0x{:x}): {}; the retained original is unchanged",
            error.offset, error.offset, error.message
        ),
    );
    response.result = json!({"error_offset":error.offset,"message":error.message,"retained":true});
    response
}

fn refused(location: &str, message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::TargetRefused, location, message)
}

/// VGM time to model units: floor(vgm_sample * clock / (divider * 44100)).
fn vgm_to(vgm_sample: u64, clock: u32, divider: u32) -> u64 {
    (u128::from(vgm_sample) * u128::from(clock) / (u128::from(divider) * u128::from(VGM_RATE)))
        as u64
}

fn port_name(port: PsgPort) -> &'static str {
    match port {
        PsgPort::Psg => "psg",
        PsgPort::Stereo => "stereo",
    }
}

/// One chip instance to render.
struct ChipPlan {
    /// sn76489, ay8910, ay8910_2, gb_dmg, gb_dmg_2, nes_apu or nes_apu_2.
    id: &'static str,
    model: ChipModel,
    clock: u32,
    writes: Vec<ChipWrite>,
    /// NES DMC memory blocks referred to by `Target::MemoryBlock` writes, with
    /// the VGM command index each came from.
    memory: Vec<MemoryBlock>,
    memory_sources: Vec<usize>,
    /// Report entry per write, before its effect is known.
    entries: Vec<Value>,
    /// Native frames start..end.
    start: u64,
    end: u64,
    chip: Value,
}

impl ChipPlan {
    fn native_rate(&self) -> (f64, u32) {
        let rate = f64::from(self.clock) / f64::from(self.model.frame_divider());
        (rate, rate.round() as u32)
    }
}

struct Plan {
    chips: Vec<ChipPlan>,
    report: Value,
    /// VGM tail: seconds requested and the range end in VGM samples.
    tail: Option<(f64, u64)>,
    /// VGM header volume modifier gain, when applied.
    gain: Option<f64>,
}

/// Decoders for inspection, from the header where it is valid.
struct Decoders {
    sn: [crate::psg::Psg; 2],
    ay: [Decoder; 2],
    gb: [Decoder; 2],
    nes: [Decoder; 2],
    pce: [Decoder; 2],
    ws: [Decoder; 2],
}

impl Decoders {
    fn new(vgm: &Vgm) -> Self {
        let config = vgm
            .psg_config()
            .map(|c| c.0)
            .unwrap_or_else(|_| PsgVariant::Sega.config());
        let ay = match vgm.ay_config() {
            Some(Ok((config, ..))) => config,
            _ => AyConfig::new(AyVariant::Ay8910),
        };
        let ay = ChipModel::Ay(ay);
        let gb = ChipModel::Gb(GbConfig::dmg());
        let nes = ChipModel::Nes(
            vgm.nes_config()
                .map_or(NesConfig::new(NesRegion::Ntsc), |n| n.0),
        );
        let pce = ChipModel::Pce(
            vgm.pce_config()
                .map_or(PceConfig::new(crate::huc6280::CLOCK_HZ), |p| p.0),
        );
        let ws = ChipModel::Ws(
            vgm.ws_config()
                .map_or(WsConfig::new(crate::wonderswan::CLOCK_HZ), |w| w.0),
        );
        Self {
            ws: [ws.decoder(), ws.decoder()],
            pce: [pce.decoder(), pce.decoder()],
            sn: [
                crate::psg::Psg::new(config).expect("checked"),
                crate::psg::Psg::new(config).expect("checked"),
            ],
            ay: [ay.decoder(), ay.decoder()],
            gb: [gb.decoder(), gb.decoder()],
            nes: [nes.decoder(), nes.decoder()],
        }
    }
    fn decode(&mut self, kind: Kind, raw: &[u8]) -> Option<Value> {
        let second = || usize::from(raw[1] >> 7);
        Some(match kind {
            Kind::PsgWrite => self.sn[0].write(raw[1]).to_json(),
            Kind::SecondPsgWrite => self.sn[1].write(raw[1]).to_json(),
            Kind::AyWrite => self.ay[second()].apply(Target::Register(raw[1] & 0x7f), raw[2]),
            Kind::GbWrite => self.gb[second()].apply(Target::Register(raw[1] & 0x7f), raw[2]),
            Kind::NesWrite if raw[1] & 0x7f < 0x20 => {
                self.nes[second()].apply(Target::Register(raw[1] & 0x7f), raw[2])
            }
            Kind::PceWrite => self.pce[second()].apply(Target::Register(raw[1] & 0x7f), raw[2]),
            Kind::WsWrite => self.ws[second()].apply(Target::Register(raw[1] & 0x7f), raw[2]),
            Kind::WsMemoryWrite => {
                self.ws[0].apply(Target::Memory(u16::from_be_bytes([raw[1], raw[2]])), raw[3])
            }
            _ => return None,
        })
    }
}

impl Service {
    pub(super) fn inspect_vgm(&mut self, input: InspectVgmInput) -> Handled {
        let bytes = self.sound_source_bytes(&input.source)?;
        let vgm = Vgm::parse(&bytes).map_err(|e| located("/source", e))?;
        let source = json!(input.source);
        match input.view {
            VgmView::Summary => {
                let mut result = vgm.summary();
                result["source"] = source;
                result["usage"] = crate::vgm_usage::usage(&vgm);
                done(ToolResponse::ok(
                    format!(
                        "VGM {} with {} commands over {:.2} s",
                        vgm.version_text(),
                        vgm.commands.len(),
                        vgm.total_samples() as f64 / f64::from(VGM_RATE)
                    ),
                    result,
                ))
            }
            VgmView::Header => done(ToolResponse::ok(
                "VGM header fields",
                json!({"source":source,"version":vgm.version_text(),"data_start":vgm.data_start,"fields":vgm.header_rows()}),
            )),
            VgmView::Kinds => done(ToolResponse::ok(
                "command kinds present",
                json!({"source":source,"kinds":vgm.kinds()}),
            )),
            VgmView::Gd3 => done(ToolResponse::ok(
                "GD3 tag",
                json!({"source":source,"gd3":vgm.gd3_json()}),
            )),
            VgmView::Commands => {
                let selected: Vec<usize> = (0..vgm.commands.len())
                    .filter(|&i| {
                        input
                            .kind
                            .as_deref()
                            .is_none_or(|k| vgm.commands[i].kind.name() == k)
                    })
                    .collect();
                if input.offset > 0 && input.offset >= selected.len() {
                    // A byte offset passed as a command index is the usual cause.
                    let as_byte = vgm
                        .commands
                        .binary_search_by_key(&input.offset, |c| c.offset)
                        .ok()
                        .and_then(|i| selected.iter().position(|&s| s == i));
                    let hint = match as_byte {
                        Some(position) => format!(
                            "; byte {} is the start of the command at position {position} of this list, so pass offset {position}",
                            input.offset
                        ),
                        None => String::new(),
                    };
                    let mut response = ToolResponse::refused(
                        Code::InvalidRange,
                        "/offset",
                        format!(
                            "offset {} is past the end of the {} selected commands; offset counts commands (a position in the list), not bytes{hint}",
                            input.offset,
                            selected.len()
                        ),
                    );
                    response.result = json!({"source":source,"total":selected.len(),
                        "byte_offset_command_position":as_byte});
                    return Err(response);
                }
                let start = input.offset.min(selected.len());
                let end = start
                    .saturating_add(input.limit.unwrap_or(100).clamp(1, MAX_PAGE) as usize)
                    .min(selected.len());
                let page = &selected[start..end];
                // Decode chip writes with the register state of every earlier write.
                let mut decoders = Decoders::new(&vgm);
                let last = page.last().copied().unwrap_or(0);
                let mut items = Vec::with_capacity(page.len());
                let mut wanted = page.iter().peekable();
                for index in 0..=last.min(vgm.commands.len().saturating_sub(1)) {
                    let command = vgm.commands[index];
                    let raw = &bytes[command.offset..command.offset + command.length];
                    let effect = decoders.decode(command.kind, raw);
                    if wanted.next_if_eq(&&index).is_some() {
                        items.push(vgm.command_json(index, effect));
                    }
                }
                done(ToolResponse::ok(
                    format!("{} of {} commands", items.len(), selected.len()),
                    json!({"source":source,"items":items,"total":selected.len(),"next_cursor":(end<selected.len()).then_some(end),
                        "psg_decoding":"latch state from power-on through every earlier PSG write",
                        "register_decoding":"AY, Game Boy and NES APU writes are decoded against the register state of every earlier write to the same chip, without running time"}),
                ))
            }
        }
    }

    fn vgm_plan(&mut self, id: &str, input: &RenderPsgInput) -> Result<Plan, ToolResponse> {
        if input.chip.is_some() {
            return Err(refused(
                "/chip",
                "a VGM header declares the chip and length; omit chip",
            ));
        }
        if input.dmc_memory.is_some() {
            return Err(refused(
                "/dmc_memory",
                "a VGM supplies DMC memory through its 0xC2 data blocks; omit dmc_memory",
            ));
        }
        let bytes = self.sound_source_bytes(id)?;
        let vgm = Vgm::parse(&bytes).map_err(|e| located("/vgm", e))?;
        let wanted = |family: ChipFamily| {
            input
                .families
                .as_ref()
                .is_none_or(|list| list.contains(&family))
        };
        // Declared chips: a declared but unmodelled variant refuses unless deselected.
        let mut chips: Vec<ChipPlan> = Vec::new();
        let sn_clock = vgm.u32_field("sn76489_clock") & 0x3fff_ffff;
        let mut declared = Vec::new();
        if sn_clock != 0 {
            declared.push(ChipFamily::Sn76489);
        }
        if sn_clock != 0 && wanted(ChipFamily::Sn76489) {
            let (config, authority) = vgm.psg_config().map_err(|m| refused("/vgm", m))?;
            check_clock(sn_clock).map_err(|m| refused("/vgm", format!("header clock: {m}")))?;
            chips.push(ChipPlan {
                id: "sn76489",
                model: ChipModel::Sn(config),
                clock: sn_clock,
                writes: Vec::new(),
                entries: Vec::new(),
                start: 0,
                end: 0,
                memory: Vec::new(),
                memory_sources: Vec::new(),
                chip: json!({"clock_hz":sn_clock,"config":config.to_json(),"authority":"VGM header","flags_authority":authority,"psg_header":vgm.psg_header()}),
            });
        }
        let mut ay_dual = false;
        if let Some(found) = vgm.ay_config() {
            declared.push(ChipFamily::Ay8910);
            if wanted(ChipFamily::Ay8910) {
                let (config, clock, dual, header) = found.map_err(|m| refused("/vgm", m))?;
                crate::ay::check_clock(clock)
                    .map_err(|m| refused("/vgm", format!("AY8910 header clock: {m}")))?;
                ay_dual = dual;
                for id in if dual {
                    &["ay8910", "ay8910_2"][..]
                } else {
                    &["ay8910"][..]
                } {
                    let model = ChipModel::Ay(config);
                    chips.push(ChipPlan {
                        id,
                        model,
                        clock,
                        writes: Vec::new(),
                        entries: Vec::new(),
                        start: 0,
                        end: 0,
                        memory: Vec::new(),
                        memory_sources: Vec::new(),
                        chip: json!({"family":"ay8910","instance":id,"clock_hz":clock,"config":model.config_json(),"authority":"VGM header","ay8910_header":header}),
                    });
                }
            }
        }
        let mut gb_dual = false;
        if let Some((config, clock, dual, header)) = vgm.gb_config() {
            declared.push(ChipFamily::GbDmg);
            if wanted(ChipFamily::GbDmg) {
                crate::gb_apu::check_clock(clock)
                    .map_err(|m| refused("/vgm", format!("GameBoy DMG header clock: {m}")))?;
                gb_dual = dual;
                for id in if dual {
                    &["gb_dmg", "gb_dmg_2"][..]
                } else {
                    &["gb_dmg"][..]
                } {
                    let model = ChipModel::Gb(config);
                    chips.push(ChipPlan {
                        id,
                        model,
                        clock,
                        writes: Vec::new(),
                        entries: Vec::new(),
                        start: 0,
                        end: 0,
                        memory: Vec::new(),
                        memory_sources: Vec::new(),
                        chip: json!({"family":"gb_dmg","instance":id,"clock_hz":clock,"config":model.config_json(),"authority":"VGM header","gameboy_dmg_header":header}),
                    });
                }
            }
        }
        let mut nes_dual = false;
        let mut nes_fds = false;
        if let Some((config, clock, dual, fds, header)) = vgm.nes_config() {
            declared.push(ChipFamily::NesApu);
            if wanted(ChipFamily::NesApu) {
                crate::nes_apu::check_clock(clock)
                    .map_err(|m| refused("/vgm", format!("NES APU header clock: {m}")))?;
                nes_dual = dual;
                nes_fds = fds;
                for id in if dual {
                    &["nes_apu", "nes_apu_2"][..]
                } else {
                    &["nes_apu"][..]
                } {
                    let model = ChipModel::Nes(config);
                    chips.push(ChipPlan {
                        id,
                        model,
                        clock,
                        writes: Vec::new(),
                        entries: Vec::new(),
                        start: 0,
                        end: 0,
                        memory: Vec::new(),
                        memory_sources: Vec::new(),
                        chip: json!({"family":"nes_apu","instance":id,"clock_hz":clock,"config":model.config_json(),"authority":"VGM header","nes_apu_header":header}),
                    });
                }
            }
        }
        let mut pce_dual = false;
        if let Some((config, clock, dual, header)) = vgm.pce_config() {
            declared.push(ChipFamily::Huc6280);
            if wanted(ChipFamily::Huc6280) {
                crate::huc6280::check_clock(clock)
                    .map_err(|m| refused("/vgm", format!("HuC6280 header clock: {m}")))?;
                pce_dual = dual;
                for id in if dual {
                    &["huc6280", "huc6280_2"][..]
                } else {
                    &["huc6280"][..]
                } {
                    let model = ChipModel::Pce(config);
                    chips.push(ChipPlan {
                        id,
                        model,
                        clock,
                        writes: Vec::new(),
                        entries: Vec::new(),
                        start: 0,
                        end: 0,
                        memory: Vec::new(),
                        memory_sources: Vec::new(),
                        chip: json!({"family":"huc6280","instance":id,"clock_hz":clock,"config":model.config_json(),"authority":"VGM header","huc6280_header":header}),
                    });
                }
            }
        }
        let mut ws_dual = false;
        if let Some((config, clock, dual, header)) = vgm.ws_config() {
            declared.push(ChipFamily::Wonderswan);
            if wanted(ChipFamily::Wonderswan) {
                crate::wonderswan::check_clock(clock)
                    .map_err(|m| refused("/vgm", format!("WonderSwan header clock: {m}")))?;
                ws_dual = dual;
                for id in if dual {
                    &["wonderswan", "wonderswan_2"][..]
                } else {
                    &["wonderswan"][..]
                } {
                    let model = ChipModel::Ws(config);
                    chips.push(ChipPlan {
                        id,
                        model,
                        clock,
                        writes: Vec::new(),
                        entries: Vec::new(),
                        start: 0,
                        end: 0,
                        memory: Vec::new(),
                        memory_sources: Vec::new(),
                        chip: json!({"family":"wonderswan","instance":id,"clock_hz":clock,"config":model.config_json(),"authority":"VGM header","wonderswan_header":header}),
                    });
                }
            }
        }
        if let Some(list) = &input.families
            && let Some(missing) = list.iter().find(|f| !declared.contains(f))
        {
            return Err(refused(
                "/families",
                format!(
                    "the header declares no {} chip; declared: {}",
                    missing.name(),
                    declared
                        .iter()
                        .map(|f| f.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if chips.is_empty() {
            return Err(refused(
                "/vgm",
                if declared.is_empty() {
                    "the header declares no chip that render_psg models (SN76489, AY8910 family, GameBoy DMG, NES APU, HuC6280, WonderSwan)".to_owned()
                } else {
                    "families selects none of the declared chips".to_owned()
                },
            ));
        }
        let passes = input.loop_passes.unwrap_or(1);
        if !(1..=MAX_LOOP_PASSES).contains(&passes) {
            return Err(refused(
                "/loop_passes",
                format!("loop_passes must be 1..{MAX_LOOP_PASSES}"),
            ));
        }
        let total = vgm.total_samples();
        let loop_index = match (&vgm.loop_index, passes) {
            (_, 1) => None,
            (None, _) => {
                return Err(refused(
                    "/loop_passes",
                    "the header declares no loop; use loop_passes 1",
                ));
            }
            (Some(Err(e)), _) => return Err(located("/vgm", e.clone())),
            (Some(Ok(i)), _) => Some(*i),
        };
        let loop_len = loop_index.map_or(0, |i| total - vgm.commands[i].at);
        if loop_index.is_some() && loop_len == 0 {
            return Err(refused(
                "/loop_passes",
                "the loop contains no waits, so repeating it adds no time",
            ));
        }
        let unrolled = total + u64::from(passes - 1) * loop_len;
        let range = input.range.unwrap_or(VgmRange {
            start_sample: 0,
            end_sample: unrolled,
        });
        if range.start_sample >= range.end_sample || range.end_sample > unrolled {
            return Err(refused(
                "/range",
                format!(
                    "the range must satisfy start < end <= {unrolled} (the unrolled length in 44100 Hz samples)"
                ),
            ));
        }
        if range.end_sample - range.start_sample > MAX_RENDER_SECONDS * u64::from(VGM_RATE) {
            return Err(refused(
                "/range",
                format!("render at most {MAX_RENDER_SECONDS} s per call; select a range"),
            ));
        }
        let tail = match input.tail_seconds {
            None => None,
            Some(t) if !(t.is_finite() && t > 0.0 && t <= MAX_TAIL_SECONDS) => {
                return Err(refused(
                    "/tail_seconds",
                    format!("a VGM tail_seconds must be above 0 and at most {MAX_TAIL_SECONDS}"),
                ));
            }
            Some(_) if range.end_sample != unrolled => {
                return Err(refused(
                    "/tail_seconds",
                    "tail_seconds renders after the end of the stream; the range must end at the unrolled end",
                ));
            }
            Some(t) => {
                let extra = (t * f64::from(VGM_RATE)).round() as u64;
                if range.end_sample + extra - range.start_sample
                    > MAX_RENDER_SECONDS * u64::from(VGM_RATE)
                {
                    return Err(refused(
                        "/tail_seconds",
                        format!("the range with its tail would exceed {MAX_RENDER_SECONDS} s"),
                    ));
                }
                Some((t, extra))
            }
        };
        let gain = match input.apply_volume_modifier {
            Some(true) => Some(volume_modifier_gain(&vgm).1),
            _ => None,
        };
        for chip in &mut chips {
            let divider = chip.model.frame_divider();
            chip.start = vgm_to(range.start_sample, chip.clock, divider);
            chip.end = vgm_to(
                range.end_sample + tail.map_or(0, |t| t.1),
                chip.clock,
                divider,
            );
        }
        let find = |chips: &[ChipPlan], id: &str| chips.iter().position(|c| c.id == id);
        let sn_index = find(&chips, "sn76489");
        let sn_stereo = matches!(chips.get(sn_index.unwrap_or(usize::MAX)).map(|c| c.model), Some(ChipModel::Sn(c)) if c.stereo_port);
        let multi = chips.len() > 1;
        let mut skipped: std::collections::BTreeMap<String, (usize, usize, String)> =
            Default::default();
        let mut skipped_commands = Vec::new();
        let mut beyond = 0usize;
        for pass in 1..=passes {
            let (first, base) = match pass {
                1 => (0, 0),
                _ => (
                    loop_index.expect("checked"),
                    total + u64::from(pass - 2) * loop_len - (total - loop_len),
                ),
            };
            for index in first..vgm.commands.len() {
                let command = vgm.commands[index];
                let at = base + command.at;
                let raw = &bytes[command.offset..command.offset + command.length];
                // Where the command goes: (chip, target, data), or why not.
                let routed: Result<(usize, Target, u8), String> = match command.kind {
                    Kind::PsgWrite | Kind::PsgStereo if sn_index.is_none() => {
                        Err(if declared.contains(&ChipFamily::Sn76489) {
                            "not rendered: the family is not selected"
                        } else {
                            "not rendered: the header declares no SN76489 clock"
                        }
                        .to_owned())
                    }
                    Kind::PsgWrite => Ok((sn_index.expect("checked"), Target::Psg, raw[1])),
                    Kind::PsgStereo if sn_stereo => {
                        Ok((sn_index.expect("checked"), Target::Stereo, raw[1]))
                    }
                    Kind::PsgStereo => Err(
                        "not applied: header flag bit 2 turns the Game Gear stereo port off"
                            .to_owned(),
                    ),
                    Kind::AyWrite | Kind::GbWrite => {
                        let ay = command.kind == Kind::AyWrite;
                        let second = raw[1] & 0x80 != 0;
                        let id = match (ay, second) {
                            (true, false) => "ay8910",
                            (true, true) => "ay8910_2",
                            (false, false) => "gb_dmg",
                            (false, true) => "gb_dmg_2",
                        };
                        let family = if ay {
                            ChipFamily::Ay8910
                        } else {
                            ChipFamily::GbDmg
                        };
                        match find(&chips, id) {
                            Some(i) => Ok((i, Target::Register(raw[1] & 0x7f), raw[2])),
                            None if !declared.contains(&family) => Err(format!(
                                "not rendered: the header declares no {} clock",
                                if ay { "AY8910" } else { "GameBoy DMG" }
                            )),
                            None if !wanted(family) => Err("not rendered: the family is not selected".to_owned()),
                            None => Err("not rendered: a second-chip write, but the header does not set the dual-chip bit".to_owned()),
                        }
                    }
                    Kind::NesWrite if raw[1] & 0x7f >= 0x20 => Err(
                        "not rendered: FDS expansion audio registers are not modelled".to_owned(),
                    ),
                    Kind::NesWrite => {
                        let id = if raw[1] & 0x80 != 0 {
                            "nes_apu_2"
                        } else {
                            "nes_apu"
                        };
                        match find(&chips, id) {
                            Some(i) => Ok((i, Target::Register(raw[1] & 0x7f), raw[2])),
                            None if !declared.contains(&ChipFamily::NesApu) => Err(
                                "not rendered: the header declares no NES APU clock".to_owned(),
                            ),
                            None if !wanted(ChipFamily::NesApu) => {
                                Err("not rendered: the family is not selected".to_owned())
                            }
                            None => Err("not rendered: a second-chip write, but the header does not set the dual-chip bit".to_owned()),
                        }
                    }
                    Kind::PceWrite => {
                        let id = if raw[1] & 0x80 != 0 {
                            "huc6280_2"
                        } else {
                            "huc6280"
                        };
                        match find(&chips, id) {
                            Some(i) => Ok((i, Target::Register(raw[1] & 0x7f), raw[2])),
                            None if !declared.contains(&ChipFamily::Huc6280) => Err(
                                "not rendered: the header declares no HuC6280 clock".to_owned(),
                            ),
                            None if !wanted(ChipFamily::Huc6280) => {
                                Err("not rendered: the family is not selected".to_owned())
                            }
                            None => Err("not rendered: a second-chip write, but the header does not set the dual-chip bit".to_owned()),
                        }
                    }
                    Kind::WsWrite | Kind::WsMemoryWrite => {
                        let memory = command.kind == Kind::WsMemoryWrite;
                        let id = if !memory && raw[1] & 0x80 != 0 {
                            "wonderswan_2"
                        } else {
                            "wonderswan"
                        };
                        match find(&chips, id) {
                            Some(i) if memory => Ok((
                                i,
                                Target::Memory(u16::from_be_bytes([raw[1], raw[2]])),
                                raw[3],
                            )),
                            Some(i) => Ok((i, Target::Register(raw[1] & 0x7f), raw[2])),
                            None if !declared.contains(&ChipFamily::Wonderswan) => Err(
                                "not rendered: the header declares no WonderSwan clock".to_owned(),
                            ),
                            None if !wanted(ChipFamily::Wonderswan) => {
                                Err("not rendered: the family is not selected".to_owned())
                            }
                            None => Err("not rendered: a second-chip write, but the header does not set the dual-chip bit".to_owned()),
                        }
                    }
                    Kind::DataBlock
                        if vgm.data_block(index).is_some_and(|(kind, ..)| kind == 0xc2) =>
                    {
                        let (_, second, payload) = vgm.data_block(index).expect("checked");
                        let id = if second { "nes_apu_2" } else { "nes_apu" };
                        match find(&chips, id) {
                            _ if payload.len() < 2 => Err(
                                "not rendered: an NES APU RAM write without its 2-byte start address"
                                    .to_owned(),
                            ),
                            Some(i) => Ok((i, Target::MemoryBlock(0), 0)),
                            None if !declared.contains(&ChipFamily::NesApu) => Err(
                                "not rendered: the header declares no NES APU clock".to_owned(),
                            ),
                            None if !wanted(ChipFamily::NesApu) => {
                                Err("not rendered: the family is not selected".to_owned())
                            }
                            None => Err("not rendered: a second-chip data block, but the header does not set the dual-chip bit".to_owned()),
                        }
                    }
                    Kind::Ay8910StereoMask => {
                        let d = raw[1];
                        let id = if d & 0x80 != 0 { "ay8910_2" } else { "ay8910" };
                        match find(&chips, id) {
                            _ if d & 0x40 != 0 => Err(
                                "not rendered: a YM2203 SSG mask (the YM2203 is not modelled)"
                                    .to_owned(),
                            ),
                            Some(i) => Ok((i, Target::StereoMask, d & 0x3f)),
                            None => Err("not rendered: no matching AY8910 is rendered".to_owned()),
                        }
                    }
                    _ => Err(command.kind.rendered().to_owned()),
                };
                match routed {
                    Ok((i, mut target, data)) => {
                        let chip = &mut chips[i];
                        let tick = vgm_to(at, chip.clock, chip.model.tick_divider());
                        let per = u64::from(chip.model.ticks_per_frame());
                        if at >= range.end_sample || tick >= chip.end * per {
                            beyond += 1;
                            continue;
                        }
                        let mut block_report = None;
                        if let Target::MemoryBlock(_) = target {
                            let block = match chip.memory_sources.iter().position(|&c| c == index) {
                                Some(b) => b,
                                None => {
                                    let (_, _, payload) = vgm.data_block(index).expect("checked");
                                    let data = &bytes[payload];
                                    chip.memory.push(MemoryBlock {
                                        start: u16::from_le_bytes([data[0], data[1]]),
                                        data: data[2..].to_vec(),
                                    });
                                    chip.memory_sources.push(index);
                                    chip.memory.len() - 1
                                }
                            };
                            target = Target::MemoryBlock(block as u16);
                            let block = &chip.memory[block];
                            block_report = Some(
                                json!({"start":format!("${:04X}", block.start),"bytes":block.data.len()}),
                            );
                        }
                        chip.writes.push(ChipWrite { tick, target, data });
                        let mut entry = json!({"command_index":index,"offset":command.offset,"pass":pass,"at_sample":at,
                            "native_sample":tick / per,"data":format!("0x{data:02x}")});
                        if let Some(block) = block_report {
                            entry["memory_block"] = block;
                            if let Some(object) = entry.as_object_mut() {
                                object.remove("data");
                            }
                        }
                        match target {
                            Target::Psg | Target::Stereo => {
                                entry["port"] = json!(if target == Target::Psg {
                                    "psg"
                                } else {
                                    "stereo"
                                })
                            }
                            Target::Register(r) => entry["register"] = json!(r),
                            Target::StereoMask => entry["port"] = json!("stereo_mask"),
                            Target::MemoryBlock(_) => entry["port"] = json!("dmc_memory"),
                            Target::Memory(address) => {
                                entry["port"] = json!("memory");
                                entry["address"] = json!(address);
                            }
                        }
                        if per > 1 {
                            entry["tick"] = json!(tick);
                        }
                        if multi || chip.id != "sn76489" {
                            entry["chip"] = json!(chip.id);
                        }
                        chip.entries.push(entry);
                    }
                    Err(_) if at >= range.end_sample => {}
                    Err(_) if matches!(command.kind, Kind::Wait | Kind::End | Kind::NoOp) => {}
                    Err(reason) => {
                        let row = skipped.entry(command.kind.name().to_owned()).or_insert((
                            0,
                            command.offset,
                            reason,
                        ));
                        row.0 += 1;
                        skipped_commands.push(json!([index, command.offset, pass]));
                    }
                }
            }
        }
        let skipped: Vec<Value> = skipped
            .into_iter()
            .map(|(kind, (count, first, reason))| json!({"kind":kind,"count":count,"first_offset":first,"render":reason}))
            .collect();
        let header_total = vgm.u32_field("total_samples");
        let only_sn = chips.len() == 1 && chips[0].id == "sn76489";
        let mut not_applied = vec!["loop base and loop modifier", "rate field"];
        if gain.is_none() {
            not_applied.insert(0, "header volume modifier");
        }
        if only_sn {
            not_applied.extend(["second chip of a dual-chip file", "DAC streams"]);
        } else {
            not_applied.extend(["the second SN76489 of a dual-chip file", "DAC streams"]);
        }
        let mut timing = json!({"range":{"start_sample":range.start_sample,"end_sample":range.end_sample,
                "seconds":(range.end_sample - range.start_sample) as f64 / f64::from(VGM_RATE)},
                "loop":{"passes":passes,"command_index":loop_index,"loop_samples":loop_len,"unrolled_samples":unrolled}});
        if !multi {
            let chip = &chips[0];
            timing["vgm_to_native"] = json!(format!(
                "floor(vgm_sample * clock / ({} * 44100))",
                chip.model.frame_divider()
            ));
            if chip.model.ticks_per_frame() > 1 {
                timing["vgm_to_tick"] = json!(format!(
                    "floor(vgm_sample * clock / ({} * 44100))",
                    chip.model.tick_divider()
                ));
            }
            timing["range"]["start_native"] = json!(chip.start);
            timing["range"]["end_native"] = json!(chip.end);
        }
        let mut report = json!({
            "source":{"kind":"vgm","source":id,"version":vgm.version_text(),"bytes":bytes.len(),"commands":vgm.commands.len()},
            "timing":timing,
            "header_consistency":{"header_total_samples":header_total,"counted_total_samples":total,"consistent":u64::from(header_total)==total,
                "loop":vgm.loop_json(),"warnings":vgm.warnings},
            "skipped":skipped,"skipped_commands":skipped_commands,"skipped_command_columns":["command_index","offset","pass"],
            "writes_after_range":beyond,
            "not_applied":not_applied
        });
        if !multi {
            report["chip"] = chips[0].chip.clone();
        }
        if nes_dual {
            report["dual_chip"] = json!(
                "each chip of a dual-chip AY8910, GameBoy DMG or NES APU file renders as an independent instance; bit 7 of the register byte (bit 31 of an NES RAM write block's size) selects the second"
            );
        } else if pce_dual || ws_dual {
            report["dual_chip"] = json!(
                "each chip of a dual-chip file renders as an independent instance; bit 7 of the register byte selects the second"
            );
        } else if ay_dual || gb_dual {
            report["dual_chip"] = json!(
                "each chip of a dual-chip AY8910 or GameBoy DMG file renders as an independent instance; bit 7 of the register byte selects the second"
            );
        }
        if chips.iter().any(|c| c.model.family() == "nes_apu") {
            report["expansion_audio"] = json!({"fds_declared":nes_fds,
                "rendered":false,"note":"expansion audio (FDS, VRC6, VRC7, MMC5, Namco 163, Sunsoft 5B) is not modelled; FDS register writes are reported as skipped"});
        }
        if let Some(gain) = gain {
            let (header, _) = volume_modifier_gain(&vgm);
            report["volume_modifier"] = json!({"applied":true,"gain":gain,"header":header,
                "rule":"each rendered chip's native samples are multiplied by the gain, rounded half away from zero and clamped to PCM16 before any resampling or mixing"});
        }
        if let Some((seconds, _)) = tail {
            report["timing"]["tail"] = json!({"requested_seconds":seconds,
                "rule":"rendered after the end of the stream with no further writes; cut at the first moment after which every rendered chip's output stays constant (a held level counts as silence) when that lasts at least 0.1 s or the rest of the tail; a constant run that begins before the stream end counts, so the tail can be 0 s"});
        }
        Ok(Plan {
            chips,
            report,
            tail: tail.map(|(seconds, _)| (seconds, range.end_sample)),
            gain,
        })
    }

    fn explicit_plan(input: &RenderPsgInput) -> Result<Plan, ToolResponse> {
        if input.range.is_some()
            || input.loop_passes.is_some()
            || input.families.is_some()
            || input.apply_volume_modifier.is_some()
        {
            return Err(refused(
                "/range",
                "range, loop_passes, families and apply_volume_modifier apply to VGM sources only",
            ));
        }
        let chip = input.chip.ok_or_else(|| {
            refused(
                "/chip",
                "declare chip.variant (sega, sn76489, ay8910, ym2149, dmg, nes_apu, nes_apu_pal, huc6280 or wonderswan) and chip.clock_hz for explicit writes or notes",
            )
        })?;
        let model = chip.variant.model(chip.clock_hz);
        match model {
            ChipModel::Sn(_) => check_clock(chip.clock_hz),
            ChipModel::Ay(_) => crate::ay::check_clock(chip.clock_hz),
            ChipModel::Gb(_) => crate::gb_apu::check_clock(chip.clock_hz),
            ChipModel::Nes(_) => crate::nes_apu::check_clock(chip.clock_hz),
            ChipModel::Pce(_) => crate::huc6280::check_clock(chip.clock_hz),
            ChipModel::Ws(_) => crate::wonderswan::check_clock(chip.clock_hz),
        }
        .map_err(|m| refused("/chip/clock_hz", m))?;
        let ws = matches!(model, ChipModel::Ws(_));
        let wavetables = ws || matches!(model, ChipModel::Pce(_));
        if !wavetables
            && let Some(i) = input
                .notes
                .iter()
                .flatten()
                .position(|n| n.wavetable.is_some())
        {
            return Err(refused(
                &format!("/notes/{i}/wavetable"),
                "wavetable belongs to HuC6280 and WonderSwan notes; Game Boy wave notes use wave",
            ));
        }
        if !ws
            && let Some(i) = input
                .notes
                .iter()
                .flatten()
                .position(|n| n.noise.is_some_and(|x| x.taps.is_some()))
        {
            return Err(refused(
                &format!("/notes/{i}/noise/taps"),
                "noise.taps belongs to WonderSwan noise notes",
            ));
        }
        if !ws
            && let Some(i) = input
                .writes
                .iter()
                .flatten()
                .position(|w| w.address.is_some())
        {
            return Err(refused(
                &format!("/writes/{i}/address"),
                "address writes internal RAM of the WonderSwan; this variant takes register writes",
            ));
        }
        let nes = matches!(model, ChipModel::Nes(_));
        if input.dmc_memory.is_some() && !(nes && input.writes.is_some()) {
            return Err(refused(
                "/dmc_memory",
                "dmc_memory applies to explicit NES writes; DMC notes carry their samples in dpcm",
            ));
        }
        if !nes && let Some(i) = input.notes.iter().flatten().position(|n| n.dpcm.is_some()) {
            return Err(refused(
                &format!("/notes/{i}/dpcm"),
                "dpcm belongs to NES DMC notes (variant nes_apu or nes_apu_pal, channel 4)",
            ));
        }
        let mut memory: Vec<MemoryBlock> = Vec::new();
        let tail = input.tail_seconds.unwrap_or(DEFAULT_TAIL_SECONDS);
        if !(tail.is_finite() && (0.0..=MAX_TAIL_SECONDS).contains(&tail)) {
            return Err(refused(
                "/tail_seconds",
                format!("tail_seconds must be 0..{MAX_TAIL_SECONDS}"),
            ));
        }
        let (tick_divider, per) = (model.tick_divider(), u64::from(model.ticks_per_frame()));
        let (writes, source, notes, setup) = match (&input.writes, &input.notes) {
            (Some(writes), None) => {
                if writes.is_empty() || writes.len() > MAX_EVENTS {
                    return Err(refused("/writes", format!("give 1..{MAX_EVENTS} writes")));
                }
                let timed = match model {
                    ChipModel::Sn(config) => {
                        if let Some(i) = writes
                            .iter()
                            .position(|w| w.port == PsgPort::Stereo && !config.stereo_port)
                        {
                            return Err(refused(
                                &format!("/writes/{i}/port"),
                                "this variant has no Game Gear stereo port; use variant sega for Game Gear",
                            ));
                        }
                        explicit_writes(chip.clock_hz, writes)
                            .map_err(|(i, m)| refused(&format!("/writes/{i}"), m))?
                            .into_iter()
                            .map(|w| ChipWrite {
                                tick: w.native_sample,
                                target: if w.port == PsgPort::Stereo {
                                    Target::Stereo
                                } else {
                                    Target::Psg
                                },
                                data: w.data,
                            })
                            .collect()
                    }
                    _ => register_writes(chip.clock_hz, model, writes)?,
                };
                let mut timed = timed;
                if let Some(blocks) = &input.dmc_memory {
                    for (i, block) in blocks.iter().enumerate() {
                        let data = crate::nes_apu::parse_hex(&block.data_hex)
                            .map_err(|m| refused(&format!("/dmc_memory/{i}/data_hex"), m))?;
                        if data.is_empty() || usize::from(block.address) + data.len() > 0x10000 {
                            return Err(refused(
                                &format!("/dmc_memory/{i}"),
                                "give 1 or more bytes that end at or before $FFFF",
                            ));
                        }
                        memory.push(MemoryBlock {
                            start: block.address,
                            data,
                        });
                    }
                    let first: Vec<ChipWrite> = (0..memory.len())
                        .map(|i| ChipWrite {
                            tick: 0,
                            target: Target::MemoryBlock(i as u16),
                            data: 0,
                        })
                        .collect();
                    timed.splice(0..0, first);
                }
                (timed, "explicit_writes", None, None)
            }
            (None, Some(notes)) => {
                if notes.is_empty() || notes.len() > MAX_EVENTS / 4 {
                    return Err(refused(
                        "/notes",
                        format!("give 1..{} notes", MAX_EVENTS / 4),
                    ));
                }
                let located = |(i, m): (usize, String)| refused(&format!("/notes/{i}"), m);
                let (timed, reports, setup) = match model {
                    ChipModel::Sn(_) => {
                        let (timed, reports) =
                            note_writes(chip.clock_hz, notes).map_err(located)?;
                        let timed = timed
                            .into_iter()
                            .map(|w| ChipWrite {
                                tick: w.native_sample,
                                target: Target::Psg,
                                data: w.data,
                            })
                            .collect();
                        (timed, reports, None)
                    }
                    ChipModel::Ay(_) => {
                        let (timed, reports) =
                            crate::ay::note_writes(chip.clock_hz, tick_divider, notes)
                                .map_err(located)?;
                        (timed, reports, None)
                    }
                    ChipModel::Nes(config) => {
                        let (timed, reports, blocks) =
                            crate::nes_apu::note_writes(chip.clock_hz, config, notes)
                                .map_err(located)?;
                        let mut setup: Vec<Value> = crate::nes_apu::NOTE_SETUP
                            .iter()
                            .map(|(o, d)| json!({"register":crate::nes_apu::register_name(*o),"offset":o,"data":d,"tick":0}))
                            .collect();
                        for block in &blocks {
                            setup.push(json!({"dmc_memory":{"start":format!("${:04X}", block.start),"bytes":block.data.len()},"tick":0}));
                        }
                        memory = blocks;
                        (timed, reports, Some(setup))
                    }
                    ChipModel::Ws(_) => {
                        let (timed, reports) = crate::wonderswan::note_writes(chip.clock_hz, notes)
                            .map_err(located)?;
                        let setup: Vec<Value> = crate::wonderswan::NOTE_SETUP
                            .iter()
                            .map(|(o, d)| json!({"port":format!("${:02X}", 0x80 + u16::from(*o)),"name":crate::wonderswan::port_name(*o),"data":d,"tick":0}))
                            .collect();
                        (timed, reports, Some(setup))
                    }
                    ChipModel::Pce(_) => {
                        let (timed, reports) =
                            crate::huc6280::note_writes(chip.clock_hz, notes).map_err(located)?;
                        let setup: Vec<Value> = crate::huc6280::NOTE_SETUP
                            .iter()
                            .map(|(o, d)| json!({"register":crate::huc6280::register_name(*o),"offset":o,"data":d,"tick":0}))
                            .collect();
                        (timed, reports, Some(setup))
                    }
                    ChipModel::Gb(_) => {
                        let (timed, reports) =
                            crate::gb_apu::note_writes(chip.clock_hz, notes).map_err(located)?;
                        let setup: Vec<Value> = crate::gb_apu::NOTE_SETUP
                            .iter()
                            .map(|(o, d)| json!({"register":crate::gb_apu::register_name(*o),"offset":o,"data":d,"tick":0}))
                            .collect();
                        (timed, reports, Some(setup))
                    }
                };
                (timed, "notes", Some(reports), setup)
            }
            _ => {
                return Err(refused("/vgm", "give exactly one of vgm, writes or notes"));
            }
        };
        let frame_rate = f64::from(chip.clock_hz) / f64::from(model.frame_divider());
        let last = writes.last().map_or(0, |w| w.tick);
        let end = (last + 1).div_ceil(per) + (tail * frame_rate).round() as u64;
        if end as f64 / frame_rate > MAX_RENDER_SECONDS as f64 {
            return Err(refused(
                "/tail_seconds",
                format!("the render would exceed {MAX_RENDER_SECONDS} s"),
            ));
        }
        let entries = writes
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let mut entry = json!({"index":i,"native_sample":w.tick / per,"data":format!("0x{:02x}",w.data)});
                match w.target {
                    Target::Psg => entry["port"] = json!(port_name(PsgPort::Psg)),
                    Target::Stereo => entry["port"] = json!(port_name(PsgPort::Stereo)),
                    Target::Register(r) => entry["register"] = json!(r),
                    Target::StereoMask => entry["port"] = json!("stereo_mask"),
                    Target::Memory(address) => {
                        entry["port"] = json!("memory");
                        entry["address"] = json!(address);
                    }
                    Target::MemoryBlock(b) => {
                        entry["port"] = json!("dmc_memory");
                        let block = &memory[usize::from(b)];
                        entry["memory_block"] = json!({"start":format!("${:04X}", block.start),"bytes":block.data.len()});
                        if let Some(object) = entry.as_object_mut() {
                            object.remove("data");
                        }
                    }
                }
                if per > 1 {
                    entry["tick"] = json!(w.tick);
                }
                entry
            })
            .collect();
        let mut timing = json!({"start_native":0,"end_native":end,"tail_seconds":tail,
            "tail_authority":if input.tail_seconds.is_some() {"declared"} else {"default 0.5"}});
        match model {
            ChipModel::Sn(_) => timing["seconds_to_native"] = json!("round(seconds * clock / 16)"),
            _ => {
                timing["seconds_to_tick"] =
                    json!(format!("round(seconds * clock / {tick_divider})"))
            }
        }
        let mut report = json!({"source":{"kind":source},
            "chip":{"variant":chip.variant,"clock_hz":chip.clock_hz,"config":model.config_json(),"authority":"declared"},
            "timing":timing});
        if let Some(notes) = notes {
            report["notes"] = json!(notes);
        }
        if let Some(setup) = setup {
            report["setup_writes"] = json!(setup);
        }
        let id = model.family();
        Ok(Plan {
            chips: vec![ChipPlan {
                id,
                model,
                clock: chip.clock_hz,
                writes,
                entries,
                start: 0,
                end,
                memory,
                memory_sources: Vec::new(),
                chip: Value::Null,
            }],
            report,
            tail: None,
            gain: None,
        })
    }

    pub(super) fn render_psg(&mut self, input: RenderPsgInput) -> Handled {
        if let Some(rate) = input.output_sample_rate
            && !(8_000..=192_000).contains(&rate)
        {
            return Err(refused(
                "/output_sample_rate",
                "output_sample_rate must be 8000..192000",
            ));
        }
        let plan = match &input.vgm {
            Some(id) if input.writes.is_none() && input.notes.is_none() => {
                self.vgm_plan(id, &input)?
            }
            Some(_) => {
                return Err(refused("/vgm", "give exactly one of vgm, writes or notes"));
            }
            None => Self::explicit_plan(&input)?,
        };
        if plan.chips.len() > 1 && input.output_sample_rate.is_none() {
            let ids: Vec<&str> = plan.chips.iter().map(|c| c.id).collect();
            return Err(refused(
                "/output_sample_rate",
                format!(
                    "this VGM renders {} chips ({}) at different native rates; give output_sample_rate for their mix (include_native_wav adds each chip's native WAV), or select one with families",
                    ids.len(),
                    ids.join(", ")
                ),
            ));
        }
        let masks = channel_masks(&plan.chips, input.channels.as_deref())?;
        let mut report = plan.report;
        let mut muted_summary = Vec::new();
        if input.channels.is_some() {
            report["channel_selection"] =
                channel_selection_report(&plan.chips, &masks, &mut muted_summary);
        }
        let single = plan.chips.len() == 1;
        let sn_single = single && plan.chips[0].model.family() == "sn76489";
        let mut files = Vec::new();
        let mut names = json!({"report":"render.json"});
        let native_wanted =
            input.output_sample_rate.is_none() || input.include_native_wav == Some(true);
        let mut all_writes: Vec<(u64, u64, Value)> = Vec::new();
        let mut summaries = Vec::new();
        let mut mixed_sources = Vec::new();
        let mut chip_reports = Vec::new();
        let mut seconds = 0f64;
        let mut renders = Vec::with_capacity(plan.chips.len());
        for (chip, mask) in plan.chips.iter().zip(&masks) {
            renders.push(
                chip.model
                    .render_muted(&chip.writes, &chip.memory, chip.start, chip.end, *mask)
                    .map_err(|m| refused("/writes", m))?,
            );
        }
        let mut chips = plan.chips;
        if let Some((requested, range_end)) = plan.tail {
            // The latest moment after which every chip's output stays constant.
            let mut settled_at = 0u64;
            for (chip, rendered) in chips.iter().zip(&renders) {
                let divider = u64::from(chip.model.frame_divider());
                let frames = &rendered.frames;
                let settled = (1..frames.len())
                    .rev()
                    .find(|&i| frames[i] != frames[i - 1])
                    .map_or(0, |i| i as u64);
                let native = chip.start + settled;
                let at = (u128::from(native) * u128::from(divider) * u128::from(VGM_RATE))
                    .div_ceil(u128::from(chip.clock)) as u64;
                settled_at = settled_at.max(at);
            }
            let cut = range_end.max(settled_at);
            let limit = range_end + (requested * f64::from(VGM_RATE)).round() as u64;
            // Silence must last TAIL_SETTLE_SAMPLES (or the rest of the tail)
            // before the end; a shorter constant stretch is a held waveform.
            let cut = cut.min(limit);
            let cut = if limit - cut < TAIL_SETTLE_SAMPLES.min(limit - range_end) {
                limit
            } else {
                cut
            };
            for (chip, rendered) in chips.iter_mut().zip(&mut renders) {
                let end = vgm_to(cut, chip.clock, chip.model.frame_divider()).max(chip.start);
                rendered
                    .frames
                    .truncate(end.saturating_sub(chip.start) as usize);
                rendered.peak = rendered
                    .frames
                    .iter()
                    .map(|f| i32::from(f[0]).abs().max(i32::from(f[1]).abs()))
                    .max()
                    .unwrap_or(0);
                chip.end = end;
            }
            let tail = &mut report["timing"]["tail"];
            tail["rendered_seconds"] = json!((cut - range_end) as f64 / f64::from(VGM_RATE));
            tail["end_sample"] = json!(cut);
            tail["stopped_early"] = json!(cut < limit);
            // Seconds from the start of the rendered range, as in the summary.
            let start = report["timing"]["range"]["start_sample"]
                .as_u64()
                .unwrap_or(0);
            let seconds_at =
                |sample: u64| sample.saturating_sub(start) as f64 / f64::from(VGM_RATE);
            let reason = if cut == limit {
                format!(
                    "the whole tail was rendered: the output did not stay constant for {:.1} s before the tail ended",
                    TAIL_SETTLE_SAMPLES as f64 / f64::from(VGM_RATE)
                )
            } else if settled_at <= range_end {
                format!(
                    "no tail was needed: every rendered chip's output was already constant from {:.3} s, before the stream end at {:.3} s (a constant run that starts before the stream end counts)",
                    seconds_at(settled_at),
                    seconds_at(range_end)
                )
            } else {
                format!(
                    "every rendered chip's output stays constant from {:.3} s, {:.3} s after the stream end",
                    seconds_at(settled_at),
                    (settled_at - range_end) as f64 / f64::from(VGM_RATE)
                )
            };
            let tail = &mut report["timing"]["tail"];
            tail["output_constant_from_seconds"] = json!(seconds_at(settled_at));
            tail["stream_end_seconds"] = json!(seconds_at(range_end));
            tail["reason"] = json!(reason);
            if chips.len() == 1 {
                report["timing"]["range"]["end_native"] = json!(chips[0].end);
            }
        }
        if let Some(gain) = plan.gain {
            let mut clipped = 0u64;
            for rendered in &mut renders {
                for frame in &mut rendered.frames {
                    for v in frame.iter_mut() {
                        let scaled = (f64::from(*v) * gain).round();
                        if !(-32768.0..=32767.0).contains(&scaled) {
                            clipped += 1;
                        }
                        *v = scaled.clamp(-32768.0, 32767.0) as i16;
                    }
                }
                rendered.peak = rendered
                    .frames
                    .iter()
                    .map(|f| i32::from(f[0]).abs().max(i32::from(f[1]).abs()))
                    .max()
                    .unwrap_or(0);
            }
            report["volume_modifier"]["clipped_samples"] = json!(clipped);
        }
        for (chip, rendered) in chips.into_iter().zip(renders) {
            let (rate, header_rate) = chip.native_rate();
            seconds = seconds.max(rendered.frames.len() as f64 / rate);
            let applied = chip.entries.len();
            for (mut entry, effect) in chip.entries.into_iter().zip(rendered.effects) {
                entry["effect"] = effect;
                let order = (
                    entry["pass"].as_u64().unwrap_or(0),
                    entry["command_index"]
                        .as_u64()
                        .or(entry["index"].as_u64())
                        .unwrap_or(0),
                );
                all_writes.push((order.0, order.1, entry));
            }
            let divider = chip.model.frame_divider();
            let mut timing = json!({"native_rate_exact":format!("{}/{divider}", chip.clock),"native_rate_hz":rate,"wav_header_rate_hz":header_rate});
            let output = json!({"frames":rendered.frames.len(),"channels":2,"peak":rendered.peak,"clipped_samples":0,
                "final_state":rendered.final_state});
            summaries.push(format!("{} {} write(s)", applied, chip.model.label()));
            if single {
                for (key, value) in timing.as_object().expect("object") {
                    report["timing"][key] = value.clone();
                }
                report["writes_applied"] = json!(applied);
                report["output"] = output;
                report["model"] = chip.model.model_scope();
                if native_wanted {
                    files.push((
                        "render.wav".to_owned(),
                        crate::opn::wav16(header_rate, &rendered.frames),
                    ));
                    names["native"] = json!("render.wav");
                }
                if let Some(target) = input.output_sample_rate {
                    let (frames, mut resampled) =
                        resample_divided(&rendered.frames, chip.clock, divider, target);
                    let name = format!("render-{target}.wav");
                    resampled["file"] = json!(name);
                    files.push((name.clone(), crate::opn::wav16(target, &frames)));
                    names["resampled"] = json!(name);
                    report["resampled"] = resampled;
                }
            } else {
                let target = input.output_sample_rate.expect("checked");
                timing["start_native"] = json!(chip.start);
                timing["end_native"] = json!(chip.end);
                timing["vgm_to_native"] =
                    json!(format!("floor(vgm_sample * clock / ({divider} * 44100))"));
                if chip.model.ticks_per_frame() > 1 {
                    timing["vgm_to_tick"] = json!(format!(
                        "floor(vgm_sample * clock / ({} * 44100))",
                        chip.model.tick_divider()
                    ));
                }
                let (exact, resampled) =
                    resample_exact(&rendered.frames, chip.clock, divider, target);
                mixed_sources.push(exact);
                let mut entry = chip.chip;
                entry["id"] = json!(chip.id);
                entry["timing"] = timing;
                entry["writes_applied"] = json!(applied);
                entry["output"] = output;
                entry["resampled"] = resampled;
                if native_wanted {
                    let name = format!("render-{}.wav", chip.id);
                    files.push((
                        name.clone(),
                        crate::opn::wav16(header_rate, &rendered.frames),
                    ));
                    entry["native_file"] = json!(name);
                }
                chip_reports.push((chip.model.family(), chip.model.model_scope(), entry));
            }
        }
        if !single {
            let target = input.output_sample_rate.expect("checked");
            let (frames, mix_report) = mix(&mixed_sources);
            let name = format!("render-{target}.wav");
            files.push((name.clone(), crate::opn::wav16(target, &frames)));
            names["resampled"] = json!(name);
            report["mix"] = mix_report;
            report["resampled"] = json!({"file":name,"target_rate":target,"target_samples":frames.len(),
                "alignment":"each chip's output sample m sits at its own range start plus m/rate; range starts differ by less than one native sample"});
            let mut models = serde_json::Map::new();
            for (family, scope, _) in &chip_reports {
                models.insert((*family).to_owned(), scope.clone());
            }
            report["models"] = json!(models);
            report["chips"] = json!(
                chip_reports
                    .into_iter()
                    .map(|(_, _, e)| e)
                    .collect::<Vec<_>>()
            );
            report["writes_applied"] = json!(all_writes.len());
        }
        all_writes.sort_by_key(|(pass, index, _)| (*pass, *index));
        let register_writes: Vec<Value> = all_writes.into_iter().map(|(_, _, e)| e).collect();
        report["files"] = names;
        let mut full = report.clone();
        full["register_writes"] = json!(register_writes);
        files.push((
            "render.json".to_owned(),
            serde_json::to_vec_pretty(&full).expect("serializes"),
        ));
        let mut response = self.export_sound_files(&input.job_id, &input.destination, files)?;
        response.summary = if sn_single {
            format!(
                "rendered {} PSG write(s) over {:.3} s with the Tunemend SN76489-family model at {} Hz",
                register_writes.len(),
                seconds,
                report["timing"]["native_rate_hz"]
            )
        } else {
            format!(
                "rendered {} over {:.3} s with Tunemend's built-in models",
                summaries.join(" and "),
                seconds
            )
        };
        if !muted_summary.is_empty() {
            response.summary = format!(
                "{}; left out of the output mix: {}",
                response.summary,
                muted_summary.join("; ")
            );
        }
        if let Some(tail) = report["timing"]
            .get("tail")
            .filter(|t| t.get("reason").is_some())
        {
            response.summary = format!(
                "{}; tail {:.3} of {:.3} s rendered: {}",
                response.summary,
                tail["rendered_seconds"].as_f64().unwrap_or(0.0),
                tail["requested_seconds"].as_f64().unwrap_or(0.0),
                tail["reason"].as_str().unwrap_or("")
            );
        }
        if let Some(object) = report.as_object_mut() {
            object.remove("skipped_commands");
        }
        report["register_writes"] =
            json!(&register_writes[..register_writes.len().min(RESPONSE_WRITES)]);
        report["register_writes_note"] = json!(format!(
            "first {} of {} applied writes; render.json lists every write and every skipped command",
            register_writes.len().min(RESPONSE_WRITES),
            register_writes.len()
        ));
        report["readback_checked"] = json!(true);
        report["hardware_audio"] = json!(false);
        response.result = report;
        done(response)
    }
}

/// Output mute masks per planned chip (bit c = channel c), from the caller's
/// channel selection; all zero without one.
fn channel_masks(
    chips: &[ChipPlan],
    selection: Option<&[ChannelSelection]>,
) -> Result<Vec<u8>, ToolResponse> {
    let mut masks = vec![0u8; chips.len()];
    let Some(selection) = selection else {
        return Ok(masks);
    };
    let ids = || chips.iter().map(|c| c.id).collect::<Vec<_>>().join(", ");
    if selection.is_empty() {
        return Err(refused(
            "/channels",
            "channels needs at least one chip entry; omit it to hear every channel",
        ));
    }
    let mut seen = vec![false; chips.len()];
    for (i, entry) in selection.iter().enumerate() {
        let at = |field: &str| format!("/channels/{i}{field}");
        let index = match &entry.chip {
            Some(id) => chips.iter().position(|c| c.id == id).ok_or_else(|| {
                refused(
                    &at("/chip"),
                    format!("`{id}` is not a rendered chip; this render has: {}", ids()),
                )
            })?,
            None if chips.len() == 1 => 0,
            None => {
                return Err(refused(
                    &at("/chip"),
                    format!(
                        "several chips render ({}); name the chip of each channel selection",
                        ids()
                    ),
                ));
            }
        };
        if std::mem::replace(&mut seen[index], true) {
            return Err(refused(
                &at("/chip"),
                format!(
                    "`{}` is selected twice; give one entry per chip",
                    chips[index].id
                ),
            ));
        }
        let names = chips[index].model.channel_names();
        let (list, solo) = match (&entry.solo, &entry.mute) {
            (Some(list), None) => (list, true),
            (None, Some(list)) => (list, false),
            _ => {
                return Err(refused(
                    &at(""),
                    "give exactly one of solo (channels to hear) or mute (channels to silence)",
                ));
            }
        };
        if list.is_empty() {
            return Err(refused(
                &at(if solo { "/solo" } else { "/mute" }),
                "list at least one channel",
            ));
        }
        let mut chosen = 0u8;
        for (k, &channel) in list.iter().enumerate() {
            if usize::from(channel) >= names.len() {
                return Err(refused(
                    &at(&format!("/{}/{k}", if solo { "solo" } else { "mute" })),
                    format!(
                        "{} has channels 0..{} ({})",
                        chips[index].id,
                        names.len() - 1,
                        names
                            .iter()
                            .enumerate()
                            .map(|(c, n)| format!("{c} {n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
            chosen |= 1 << channel;
        }
        let all = ((1u16 << names.len()) - 1) as u8;
        masks[index] = if solo { all & !chosen } else { chosen };
    }
    Ok(masks)
}

/// The report of a channel selection, and summary phrases for muted chips.
fn channel_selection_report(chips: &[ChipPlan], masks: &[u8], summary: &mut Vec<String>) -> Value {
    let entries: Vec<Value> = chips
        .iter()
        .zip(masks)
        .map(|(chip, &mask)| {
            let names = chip.model.channel_names();
            let list = |muted: bool| -> Vec<Value> {
                (0..names.len())
                    .filter(|&c| (mask & (1 << c) != 0) == muted)
                    .map(|c| json!({"channel":c,"name":names[c]}))
                    .collect()
            };
            if mask != 0 {
                let muted: Vec<&str> = (0..names.len())
                    .filter(|&c| mask & (1 << c) != 0)
                    .map(|c| names[c])
                    .collect();
                summary.push(format!("{} {}", chip.id, muted.join(", ")));
            }
            json!({"chip":chip.id,"heard":list(false),"muted":list(true)})
        })
        .collect();
    json!({"chips":entries,
        "policy":"muted channels are left out of each model's output mix; every write is applied and registers, counters, envelopes and phases run as in an unselected render, so final_state and the applied writes are unchanged; a multi-chip mix keeps its 1/N gain"})
}

/// The header volume modifier (0x7C) as the VGM specification defines it:
/// gain 2^(v/32) for v in -63..192 (0xC1..0xFF negative, -63 read as -64),
/// supported from version 1.50 when the header reaches 0x7C.
fn volume_modifier_gain(vgm: &Vgm) -> (Value, f64) {
    let present = vgm.version >= 0x150 && vgm.data_start > 0x7c;
    let raw = if present { vgm.bytes[0x7c] } else { 0 };
    let v = if raw <= 192 {
        i32::from(raw)
    } else {
        i32::from(raw) - 256
    };
    let v = if v == -63 { -64 } else { v };
    let gain = (f64::from(v) / 32.0).exp2();
    (
        json!({"raw":format!("0x{raw:02x}"),"present":present,"exponent":v,
            "authority":if present {"header byte 0x7C (players support it from 1.50)"} else {"absent before 1.50 or outside the header: gain 1"}}),
        gain,
    )
}

/// Explicit AY, Game Boy or NES register writes with their ticks.
fn register_writes(
    clock_hz: u32,
    model: ChipModel,
    writes: &[PsgWriteInput],
) -> Result<Vec<ChipWrite>, ToolResponse> {
    let (divider, per) = (model.tick_divider(), u64::from(model.ticks_per_frame()));
    let mut out: Vec<ChipWrite> = Vec::with_capacity(writes.len());
    for (index, write) in writes.iter().enumerate() {
        let at = |m: &str| refused(&format!("/writes/{index}"), m.to_owned());
        if write.port != PsgPort::Psg {
            return Err(refused(
                &format!("/writes/{index}/port"),
                "AY, Game Boy, NES, HuC6280 and WonderSwan writes take register (or a WonderSwan address) and data; the stereo port belongs to the Game Gear",
            ));
        }
        let target = match (model, write.register, write.address) {
            (ChipModel::Ws(_), None, Some(address)) => Target::Memory(address),
            (ChipModel::Ws(_), Some(_), Some(_)) => {
                return Err(refused(
                    &format!("/writes/{index}"),
                    "give register (a port) or address (internal RAM), not both",
                ));
            }
            (_, Some(register), _) => Target::Register(register),
            _ => {
                return Err(refused(
                    &format!("/writes/{index}/register"),
                    "give the register for AY, Game Boy, NES, HuC6280 and WonderSwan writes (or a WonderSwan RAM address)",
                ));
            }
        };
        let register = match target {
            Target::Register(register) => register,
            _ => 0,
        };
        let valid = match model {
            ChipModel::Ay(_) => register <= 15,
            ChipModel::Nes(_) => register <= 0x1f,
            ChipModel::Pce(_) => register <= 0x7f,
            ChipModel::Ws(_) => register <= 0x7f,
            _ => register <= 0x2f,
        };
        if !valid {
            return Err(refused(
                &format!("/writes/{index}/register"),
                match model {
                    ChipModel::Ay(_) => "AY registers are 0..15",
                    ChipModel::Ws(_) => {
                        "WonderSwan registers are 0x00..0x7F as in VGM 0xBC: ports $80..$9F at 0x00..0x1F; read-only and undocumented ports and offsets 0x20..0x7F are applied and reported as ignored"
                    }
                    ChipModel::Pce(_) => {
                        "HuC6280 registers are 0x00..0x7F as in VGM 0xB9: R0..R9 at 0..9; offsets 0x0A..0x7F are applied and reported as ignored"
                    }
                    ChipModel::Nes(_) => {
                        "NES registers are offsets 0x00..0x1F from $4000 (APU registers 0x00..0x13, 0x15 status, 0x17 frame counter; other addresses are applied and reported as ignored)"
                    }
                    _ => {
                        "Game Boy registers are offsets 0x00..0x2F from 0xFF10 (sound registers 0x00..0x16, wave RAM 0x20..0x2F; unused addresses are applied and reported as ignored)"
                    }
                },
            ));
        }
        let tick = match (write.at_seconds, write.at_native_sample, write.at_tick) {
            (Some(seconds), None, None) => {
                if !(seconds.is_finite() && (0.0..=MAX_RENDER_SECONDS as f64).contains(&seconds)) {
                    return Err(at(&format!(
                        "times must be 0..{MAX_RENDER_SECONDS} seconds"
                    )));
                }
                (seconds * f64::from(clock_hz) / f64::from(divider)).round() as u64
            }
            (None, Some(sample), None) => sample * per,
            (None, None, Some(tick)) => tick,
            _ => {
                return Err(at(
                    "give exactly one of at_seconds, at_native_sample or at_tick",
                ));
            }
        };
        if out.last().is_some_and(|w| w.tick > tick) {
            return Err(at("writes must be in time order"));
        }
        out.push(ChipWrite {
            tick,
            target,
            data: write.data,
        });
    }
    Ok(out)
}
