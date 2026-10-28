# Inspecting and auditioning PSG-family music

Use this workflow for VGM files with Master System, Game Gear or Mega Drive PSG
data (SN76489 family), AY-3-8910/YM2149 PSG data (MSX, ZX Spectrum 128,
Amstrad CPC, Atari ST), Game Boy sound (DMG APU), NES/Famicom sound (2A03 or
2A07 APU), PC Engine/TurboGrafx-16 sound (HuC6280 PSG) or WonderSwan sound, or
to hear register writes and notes for those chips. Tunemend interprets VGM and renders these chips with
its own models; no emulator or player is needed.

## Retain and inspect a VGM

1. Call `load_instrument_source` with the file path. Every byte is retained,
   whatever the file contains; `discovery.formats` names `vgm` when the file
   starts with the VGM identifier. A `.vgz` file is gzip-compressed: retain it
   as it is, and load a decompressed `.vgm` copy for interpretation.
2. Call `inspect_vgm` with the `source` ID:
   - `view: "summary"`: version, chip clocks, the PSG settings the header
     declares (`psg.config`, with authority for each), counted and declared
     totals, the loop, the byte layout, command kinds and GD3. Check
     `total_consistent` and `loop.consistent`; a mismatch means the header
     disagrees with the stream.
   - `view: "kinds"`: every command kind present, its count, opcodes, first
     offset and what `render_psg` does with it. `ay8910_write` (0xA0),
     `gameboy_dmg_write` (0xB3), `nes_apu_write` (0xB4), `huc6280_write`
     (0xB9), `wonderswan_write` (0xBC) and `wonderswan_memory_write` (0xC6)
     are rendered when the header declares that chip; other chips (such as the YM2612 in a Mega
     Drive file or the SCC in an MSX file) appear as `other_chip_write` and
     are not rendered.
   - The summary's `ay8910`, `gameboy_dmg` and `nes_apu` entries show the
     declared clock, dual-chip bit and, for the AY, the chip type, variant,
     pin-26 clock halving and pan flag, or why the chip cannot be rendered
     (`render_refusal`); for the NES, the region (PAL tables when the clock is
     within 1% of 1662607 Hz) and the FDS bit. `huc6280` and `wonderswan`
     entries appear when the header declares those clocks.
   - The summary's `usage` answers which parts a file uses without paging
     commands: per chip, each channel's `writes`, `key_ons`, `active_seconds`
     and first/last write sample, `unused_channels`, channels
     `written_but_never_keyed`, named `ports` with counts and `unused_ports`.
     Keyed time comes from register state in one pass (no loop repeats, no
     model run); read each chip's `active_rule`, which says where it is an
     upper bound. `other_chip_commands` counts commands of chips Tunemend does
     not render, `data_blocks` lists blocks by type and size, and `hints` flags
     data that looks implausible (such as an SN76489 period that seems to have
     wrapped past 10 bits) without changing it; mention a hint to the person
     rather than repairing the data. The HuC6280 entry also counts
     `waveform_writes` and `dda_writes` per channel and names
     `noise_enabled_channels` and whether the LFO was enabled; the WonderSwan
     entry counts `internal_ram` writes among its ports and says whether voice,
     sweep and noise modes were used.
   - `view: "commands"`: paged commands in `items`, each with `offset`,
     `length`, raw `bytes`, `at_sample` (44100 Hz VGM time) and decoded
     `meaning`; `total` counts the matching commands. PSG writes show the
     register they reach through the latch protocol, so a data byte names its
     latched register. AY writes show the register name, the combined tone or
     envelope period, mixer enables and envelope shape; Game Boy writes show
     the register (NR10–NR52 or `wave_ram`) and its fields, and say when a
     write is ignored because the APU is off. NES writes show the `$40xx`
     register name and its fields (duty, envelope, sweep target and mute,
     period, length index and whether it loaded, DMC rate, address and length,
     `$4015` enables), and a `data_block` of type 0xC2 shows the DMC memory it
     fills. HuC6280 writes name the register (R0–R9) and the channel R0
     selected: the divider and its frequency, the R4 mode (write data, reset
     counter, mixing, direct D/A), per-side attenuation in dB or `silent`,
     waveform words with their address, DDA levels, noise rates and LFO
     settings. WonderSwan port writes name the port ($80–$9F) and its fields
     (divisor and frequency, volumes, voice levels, sweep, tap mode with its
     sequence length, channel modes), and RAM writes show which channel's
     wavetable samples they hold. Filter with `kind` (for example `psg_write`, `gameboy_dmg_write` or
     `nes_apu_write`) and follow `next_cursor` as `offset`. The input `offset`
     is a position in the (filtered) command list, not a byte offset; each
     item's `offset` is its byte offset in the file and `index` its command
     index. An input `offset` past the end refuses at `/offset` and, when the
     value is the byte offset of a command, names the position to pass.
   - `view: "header"`: every header field with raw bytes and whether this
     version defines it. `view: "gd3"`: the decoded tag.
3. A refusal names the byte offset (`/source/bytes/<offset>`): a truncated
   command, an undefined opcode, a missing end command or a broken header. The
   retained original is unchanged; report the offset instead of guessing a
   repair.

## Render the VGM stream

Call `render_psg` with `vgm` set to the source ID and a new `destination`.

- The header supplies each chip's clock and variant (for the SN76489: LFSR
  width and taps, period-0 behaviour, stereo and negate flags; for the AY: the
  chip type, pin-26 halving and pan flag). Do not pass `chip`.
- Every modelled chip the header declares renders. When there is more than
  one (for example an AY with a Game Boy, or two AY chips with the dual-chip
  bit), give `output_sample_rate`: the chips are resampled, scaled by 1/N and
  summed into `render-<rate>.wav`, and `mix` reports the policy and clipping.
  `include_native_wav: true` adds each chip's native WAV (`render-<id>.wav`).
  To hear one chip alone, pass `families` (such as `["ay8910"]`); the others are
  then reported as skipped.
- To hear or measure single channels, pass `channels`: one entry per chip you
  change, with `chip` (the rendered id from `render.json`: `sn76489`,
  `ay8910`, `ay8910_2`, `gb_dmg`, `nes_apu`, `huc6280`, `wonderswan` and their
  `_2` second chips; required when several chips render) and either `solo` or
  `mute` with 0-based channel numbers: SN76489 tones 0–2 and noise 3; AY A, B,
  C as 0–2; Game Boy pulse 1, pulse 2, wave, noise as 0–3; NES pulse 1, pulse 2,
  triangle, noise, DMC as 0–4; HuC6280 channels 1–6 as 0–5; WonderSwan channels
  1–4 as 0–3. For example `"channels": [{"chip": "nes_apu", "solo": [2]}]`
  renders the triangle alone. The channels are left out of the output only:
  every write still applies and the chip state, phases and envelopes run as
  in the full render, so the channel sounds exactly as it does inside the mix
  and you can measure its pitch with `measure_audio` `pitch: {}` without
  making muted copies with `transform_vgm`. `channel_selection` in the report
  lists the heard and muted channels; chips you do not name play whole and
  the 1/N mix gain stays. Explicit writes and notes take the same `channels`
  (the `chip` may be omitted).
- `loop_passes` (1–16) plays the file once and then repeats from the header
  loop; use 2 for looping music so the loop return is audible.
- `range` (`start_sample`, `end_sample` in 44100 Hz VGM samples of the unrolled
  timeline) renders an excerpt. Earlier writes are still applied, so the excerpt
  matches the same span of a full render exactly. One call covers at most
  120 s; hear a longer song in consecutive ranges (0..5292000,
  5292000..10584000, …). With `output_sample_rate` the resampled output sits
  on the timeline's grid: at 44100 Hz a range has exactly `end_sample −
  start_sample` frames, so consecutive ranges join without a gap or an extra
  frame (only the samples nearest a join differ from one longer render, since
  each call's resampler sees silence outside its own range).
- `output_sample_rate` (for example 44100) writes `render-<rate>.wav`. Without
  it, a single chip's `render.wav` is written at its native rate (SN76489
  clock / 16, about 223.7 kHz; AY clock / 8; Game Boy clock / 32, 131072 Hz;
  NES clock / 16, about 111.9 kHz; HuC6280 clock / 32, about 111.9 kHz;
  WonderSwan clock / 128, 24000 Hz); add `include_native_wav: true` to get both.
- The render stops at the end of the stream, which can cut a note that is still
  sounding. `tail_seconds` (up to 30) lets the chips run on without further
  writes and stops once every chip's output has stayed constant for 0.1 s;
  `timing.tail` reports the requested and rendered seconds,
  `output_constant_from_seconds`, `stream_end_seconds` and a `reason`, which
  the summary repeats. A constant run that began before the stream end counts:
  a stream that mutes every channel before its end gets a 0 s tail
  (`stopped_early: true`), which means nothing rings on, not that the tail
  failed. It needs the range to reach the end of the stream.
- `apply_volume_modifier: true` applies the header volume modifier (0x7C,
  gain 2^(v/32)) to every chip and reports `volume_modifier`; by default it is
  listed in `not_applied`.

Read the response or `render.json`: `writes_applied`, `register_writes` (each
with command index, file offset, pass, VGM and native sample and decoded
effect), `skipped` (kinds not rendered, with counts and reasons),
`writes_after_range`, `header_consistency` and `model` (`chips`, `models` and
`mix` when several chips render; each write then names its `chip`). Tell the
person what was not rendered: in a Mega Drive file the FM part and DAC samples
are missing from this render, in an MSX file the SCC or FM part, in an NES
file any FDS expansion part (`expansion_audio`).

Files declaring a T6W28, XNOR noise, a /2 clock divider, an AY8930 or an
undefined AY chip type refuse unless `families` leaves that chip out; the
second SN76489 of a dual-chip file is reported as not rendered.

## Derive a changed VGM

`transform_vgm` writes a new retained VGM from a retained one; the original
stays exact. Give a new `job_id`, the `source`, a new `destination`
(optionally `vgm_name`, a plain file name such as `jingle-msx.vgm`, for the
written file and the new source; default `transformed.vgm`) and exactly one
operation:

- `copy` with `family` (`sn76489`, `ay8910`, `gb_dmg` or `nes_apu`, declared
  by the header) and at least one change: `transpose_semitones` (-48..48,
  optionally `transpose_channels`; tone channels only), `mute_channels`,
  `clock_hz` (with `keep_pitch`, default true, which rewrites periods so pitch
  stays), or `variant` (`sega`/`sn76489`, `ay8910`/`ym2149`, `nes_apu`/
  `nes_apu_pal` for the NTSC or PAL clock and tables). Commands of other chips
  are copied byte for byte.
- `map_sn76489_to_ay` with `variant` (`ay8910` or `ym2149`), `clock_hz`, and
  optionally `tone_channels` (AY channel or null per SN76489 tone),
  `noise_channel` (default 2, C) or `drop_noise`.

The first call may refuse at `/accept_losses`: each diagnostic names a loss
code (for example `period_out_of_range`, `volume_rounded`,
`noise_sequence_differs`, `period_needs_retrigger`, `loop_entry_state_differs`)
with its count and first command, and nothing is written. Do not accept a loss
just to proceed. Tell the person what each loss changes in the music and decide
together, or change the request (a smaller transposition, another noise
channel, a channel that is free). Then repeat with a new `job_id` and the codes
in `accept_losses`. A request that changes no byte refuses with `empty_edit`.

Read the result: `source` (the new ID) and `from`, `commands` (unchanged,
replaced and replacement counts), `changes` (each changed group with original
and new bytes and reasons; `transform.json` has all), `header_changes` (EoF,
GD3 and loop offsets are bookkeeping), `losses`, `approximations` (rounding in
cents, level error in dB) and `readback` (totals, loop time, untouched commands,
replayed register state). Check the new source with `inspect_vgm`, render both
with `render_psg`, and compare them with `compare_audio` before presenting the
result. An SN76489 mapped to the AY plays through the AY model (unipolar, with
DC). The mapping and copies are model-level register rewrites; a game driver may
still play the result differently.

## Render explicit writes or notes

Without a VGM, declare the chip. For the SN76489 family:
`chip: {"variant": "sega", "clock_hz": 3579545}` for Master System, Game Gear
and Mega Drive (3546893 for PAL), or `sn76489` for the discrete part (SG-1000,
SC-3000H, BBC Micro, ColecoVision). The AY and Game Boy variants follow below.
Nothing is inferred from a game or console name.

- `notes`: tone channels 0–2 take `pitch` (`{"kind":"period","period":254}` or
  `{"kind":"midi_note","note":69,"a4_hz":440}`) and `attenuation` 0–14 (2 dB
  steps); channel 3 takes `noise` (`mode` white or periodic, `rate` 0–3, where 3
  follows tone 2's period) and `attenuation`. Notes on one channel must not
  overlap. MIDI pitches report their period rounding in cents.
- `writes`: exact bytes for the PSG port, or for the Game Gear `stereo` port
  (sega variant only), at `at_native_sample` or `at_seconds`, in time order.
  Use this to reproduce driver output exactly.

`tail_seconds` (default 0.5) extends the render after the last write.

### AY-3-8910 and YM2149

Declare `chip: {"variant": "ay8910", "clock_hz": 1789773}` (MSX; 1773400 for
the ZX Spectrum 128, 1000000 for the Amstrad CPC) or `ym2149` (Atari ST,
2000000). The YM2149 has a 32-step envelope and finer DAC steps; give the clock
that reaches the sound core.

- `writes`: `register` 0–15 and `data`, at `at_native_sample` (clock / 8 ticks),
  `at_tick` or `at_seconds`. Register 7 is the mixer (a bit set disables a tone
  or noise), 8–10 the amplitudes (bit 4 selects the envelope), 11–13 the
  envelope period and shape; 14 and 15 are I/O ports and make no sound.
- `notes`: channels 0–2 with `pitch` (period 1–4095 or a MIDI note) and/or
  `noise: {"period": 1..31}`, and `volume` 1–15. The noise period is shared, so
  overlapping noise notes must use the same period. Use `writes` for envelopes.

### Game Boy

Declare `chip: {"variant": "dmg", "clock_hz": 4194304}`. CGB differences are
not modelled.

- `writes`: `register` as the offset from 0xFF10 (0x00 NR10 … 0x16 NR52,
  0x20–0x2F wave RAM) and `data`, at `at_tick` (clock / 2, exact),
  `at_native_sample` (clock / 32) or `at_seconds`. The APU starts powered on
  with every register 0, so write NR50 and NR51 before expecting sound.
- `notes`: setup writes NR52, NR50 0x77 and NR51 0xFF at time 0. Channels 0 and
  1 (pulse) take `pitch`, `volume` 0–15, `duty` 0–3 and optional
  `envelope: {"direction": "down", "pace": 2}`; channel 2 (wave) takes `pitch`
  and `wave: {"samples": [32 values 0..15], "output_level": 1..3}`; channel 3
  (noise) takes `noise: {"mode": "white" or "periodic", "divisor": 0..7,
  "shift": 0..13}`, `volume` and an optional envelope.

### NES APU

Declare `chip: {"variant": "nes_apu", "clock_hz": 1789773}` (NTSC 2A03 tables)
or `nes_apu_pal` with 1662607 (PAL 2A07 noise, DMC and frame counter tables).
Expansion audio (FDS, VRC6, VRC7, MMC5, Namco 163, Sunsoft 5B) is not modelled.

- `writes`: `register` as the offset from $4000 (0x00–0x13 channels, 0x15
  status, 0x17 frame counter) and `data`, at `at_tick` (CPU cycles, exact),
  `at_native_sample` (16 cycles) or `at_seconds`. Every channel starts
  disabled, so write `$4015` first; a length load ($4003, $4007, $400B, $400F)
  on a disabled channel is ignored. `dmc_memory: [{"address": 49152,
  "data_hex": "..."}]` places DPCM bytes before the first write; the DMC reads
  $C000 + 64·($4012) for 16·($4013) + 1 bytes.
- `notes`: setup writes $4015 = $0F and $4017 = $40. Channels 0 and 1 (pulse)
  take `pitch`, `volume` 0–15 and `duty` 0–3; channel 2 (triangle) takes
  `pitch` only and starts and stops at the next quarter frame (about 4 ms);
  channel 3 (noise) takes `noise: {"mode": "white" or "periodic", "period":
  0..15}` and `volume`; channel 4 (DMC) takes `dpcm: {"data_hex": "...",
  "rate": 0..15, "looped": false, "start_level": 64}` with 16n + 1 bytes. Use
  `writes` for hardware envelopes and sweeps.

### HuC6280 (PC Engine)

Declare `chip: {"variant": "huc6280", "clock_hz": 3579545}` (the 7.16 MHz
master clock / 2, as VGM files give it).

- `writes`: `register` R0–R9 (0–9) and `data`, at `at_tick` (clock cycles,
  exact), `at_native_sample` (32 ticks) or `at_seconds`. R0 selects the
  channel (0–5) that R2–R7 act on; R1 is the main level (left/right nibbles),
  R4 sets chON, DDA and the 5-bit level, R5 the channel's left/right levels,
  R6 writes the waveform (chON and DDA clear) or a direct D/A level (DDA set),
  R7 enables noise on channels 4 and 5, R8 and R9 drive the LFO. Every
  channel starts off with R1 = 0: write R1 before expecting sound. To load a
  waveform from word 0, write R4 $40, then $00, then 32 words to R6.
- `notes`: setup writes R1 = $FF. Channels 0–5 take `pitch` (divider
  1–4095; f = clock / (32 divider)), `volume` (0–31, 1.5 dB steps) and
  `wavetable` (32 words 0–31, loaded at each note start); channels 4 and 5 can
  instead take `noise: {"period": 0..31}` (the R7 field; higher is faster) and
  `volume`. Use `writes` for DDA samples, stereo balance and the LFO.
- Note reports give both numberings: `channel` is your 0-based input channel
  (0–5) and `hardware_channel` is the same channel numbered 1–6 as in the
  HuC6280 manual. Use `hardware_channel` when you quote the manual or a driver
  document, and `channel` in requests.

### WonderSwan

Declare `chip: {"variant": "wonderswan", "clock_hz": 3072000}`. Hyper Voice
(WonderSwan Color) and the internal speaker path are not modelled; the render
is the headphone mix.

- `writes`: `register` as the port minus 0x80 (0x00–0x1F for $80–$9F) or
  `address` for an internal RAM byte, with `data`, at `at_tick` (CPU cycles,
  exact), `at_native_sample` (128 cycles) or `at_seconds`. $8F (0x0F) places
  the wavetables at RAM `$8F << 6`, 16 bytes per channel with the earlier
  sample in each low nibble; $90 (0x10) enables channels 1–4 (bits 0–3), channel
  2 voice (bit 5, playing $89 at the $94 levels), channel 3 sweep (bit 6, with
  $8C and $8D) and channel 4 noise (bit 7, with $8E). Everything starts off and
  RAM is 0.
- `notes`: setup writes $8F = 0 and $91 = $08. Channels 0–3 take `pitch`
  (divisor 0–2047; f = clock / (32 (2048 − divisor))), `volume` 0–15 and
  `wavetable` (32 samples 0–15, written to RAM at the note start); channel 3
  can instead take a `pitch` period with `noise: {"taps": 0..7}` and `volume`.
  Use `writes` for voice, sweep and separate left/right volumes.

## Limits

The render is Tunemend's model of each chip. The SN76489 output is bipolar and
unfiltered: the real chip's DC drift, analog filtering and the Master System
2's clipped top levels are not modelled. The AY output is unipolar with its DC
kept and documented 3 dB (1.5 dB on the YM2149) level steps, not measured DAC
curves. The Game Boy output is the DAC level before the console's high-pass
filter, so it carries DC, and quirks such as wave RAM corruption are not
modelled. The NES output is the documented nonlinear mixer level before the
console's filters, also with DC; DMC DMA timing effects are not modelled. The
HuC6280 output is unipolar with DC, its 1.5 dB and 3 dB level steps and 45 dB
cut-off follow Hudson's manual, and its noise bit sequence is a stand-in (only
its rate follows the documents). The WonderSwan output is the unipolar
headphone mix sampled at 24000 Hz; no independent reference was compared.
Mixed chips use the reported 1/N policy, not a machine's analog
mixing.

When you measure these renders with `measure_audio`, expect DC: AY, Game Boy,
NES, HuC6280 and WonderSwan renders are unipolar, so `dc_offset` is well above 0 (for example 0.2
on an NES render) and a muted channel holds a constant level rather than 0
(a channel left out with `render_psg` `channels` contributes nothing). A
pitch track over a held level can show `dc_dominated` windows and notes
marked `low_confidence`; treat those as the offset, not as played notes.
`silence` counts a held constant level as silence only when it is near 0, so
judge the start and tail of a unipolar render from the render report
(`timing.tail`, the last write) instead of expecting `dc_offset` near 0. The
SN76489 render is bipolar and follows the general rule. Present it as a model audition, not original hardware,
driver or game audio, and do not claim a song is reproduced because its PSG
part sounds right. Other chips, DAC streams and data blocks in a VGM are not
rendered, except NES APU RAM-write blocks (type 0xC2), which fill the DMC's
memory.
