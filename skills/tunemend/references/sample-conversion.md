# Cross-family sample conversion

Use `load_instrument_source` first for an unfamiliar file: exact original retention
never depends on knowing its sample format. For an already loaded native bank,
inspect its sample slots. Do not infer sample rate or musical tuning from the
console name.

`prepare_sample` takes either `source` plus `offset`/optional `length`, or `bank`
plus `slot`. Source slices declare `encoding`: `brr`, `u8`, `s8`, `s16le`,
`s16be`, `psx_adpcm` (PlayStation SPU ADPCM blocks), `vag` for a complete VAG
file, `md_dac4` (Mega Drive DAC values packed two per byte), or `wav` for a
complete RIFF/WAVE file inside the slice; bank inputs
require `brr` and use the stored loop address. Supply an explicit `sample_rate`
and a `target` encoding (`brr`, `u8`, `s8`, `s16le`, `s16be`, `psx_adpcm`,
`vag` or `md_dac4`). The codec is built in. You may pass `codec` with a trusted external script that speaks the
same JSON protocol (run with `python`, default `python3`); it is executable code,
so use only one you trust. It supports only raw encodings with
`loop_policy: "finite"`, without `wav`, `target_sample_rate`, `root`,
`psx_adpcm`, `md_dac` or PSX ADPCM/VAG/`md_dac4` sides.

The codec accepts at most 8388608 decoded mono samples, including expanded
loop traversals (about 190 seconds at 44.1 kHz). Raw slices are limited to 16 MiB;
WAV containers to 64 MiB. Target block padding and termination must also fit.
Long loops retain complete PCM, seam and level checks; pitch/period analysis is
omitted above 131072 loop samples and reported as not run. BRR requires complete
nine-byte blocks ending exactly at the final end flag. A looping source requires
an aligned `loop_offset` for raw slices. Choose how a loop reaches the target:

- `loop_policy: "finite"` (default): `loop_repeats` adds 0..16 traversals,
  carrying predictor history; the target is a one-shot. Loop baking always
  appears as `finite_loop`, even when every decoded sample is exact.
- `loop_policy: "preserve"`: the target keeps the loop and the report gives its
  `target_loop_offset`. Supply `loop_offset` for a raw slice (bytes, on a sample
  boundary for PCM); a bank slot uses the bank's stored loop, so no
  `loop_offset` is needed. A BRR target needs a 16-sample-aligned loop start and
  length; otherwise the call is refused rather than shifting the loop, with
  recoveries ([loop fit](#loop-fit-and-loop-diagnostics)). A BRR
  target restarts the loop block with filter 0, and the comparison covers the
  first pass plus one loop traversal, so any change at loop re-entry is measured.

Initial BRR history is zero. No arbitrary compressed-format detection is provided.

A preserved BRR loop can be inexact even when the target is PCM16. BRR filters
1-3 predict from the two previous decoded samples. The first pass enters the
loop block after the attack; every later traversal re-enters it after the
loop's own end, so the second and later traversals can differ from the loop
region of the first pass. A PCM target stores one copy of the loop and repeats
the first-pass values, so it cannot reproduce the second traversal. `sample_values`
then reports `first_changed_sample`, `first_pass_changed_samples`,
`loop_traversal_changed_samples` and `first_pass_samples`. Choose one: accept
the measured change, pick another sample or slot whose loop is exact (for
example a loop block that uses filter 0), or use `loop_policy: "finite"` to
bake the traversals into a one-shot.

For a native BRR bank sample, `sample_rate` is a declared playback assumption,
not a measured tuning. The S-DSP outputs 32000 Hz, and pitch register 0x1000
plays a sample at 32000 Hz, so declaring 32000 describes the sample as played at
pitch 0x1000. The driver's actual pitch tables and note offsets determine the
sounding pitch; use the bank's tuning data or a stated reference when it matters.

## WAV sources

`encoding: "wav"` reads integer PCM 8/16/24/32-bit and IEEE float 32/64-bit,
plain or as `WAVE_FORMAT_EXTENSIBLE`, with 1..32 channels, at most 8388608
frames and at most 64 MiB.
Compressed WAV formats such as ADPCM, RIFX and RF64 are refused. Before
preparing, `inspect_instrument_source` with the source shows under
`discovery.formats[0].wav` what `prepare_sample` will read: format, channels,
`sample_rate` (pass it as `sample_rate`), bit depth, frames, duration and the
`smpl` loops (start, inclusive end, length, whether `wav.smpl_loop` can use
each and whether it fits BRR or PSX ADPCM blocks without resampling) and root
(whether `wav.smpl_root` can use it). The report's
`wav` section gives the header facts (`header_sample_rate`, bits, channels,
`extensible`, frames), every chunk with its offset and size, the
`uninterpreted_chunks`, and any `smpl` chunk. Unknown chunks are not
interpreted; they stay byte-exact in the retained original and `input.bin`.
Truncated or inconsistent files are refused with the byte offset involved.

Values are scaled to PCM16 units (8-bit unsigned and float 1.0 = 32768 are
exact powers of two) and rounded half away from zero. Choose the channel
explicitly in `wav`:

- Mono files need no `wav` object.
- `wav.channel` (0-based) uses one channel unchanged.
- `wav.downmix: "mean"` averages all channels per frame. It always appears as
  `channel_downmix` with the maximum and RMS deviation of each channel from the
  mix.

`bit_depth_reduction` appears when the 16-bit value differs from the exact
selected or mixed value: 24/32-bit and float sources with finer detail, a mean
between PCM16 steps, or float beyond full scale (counted in `clipped_samples`).

`sample_rate` must equal the header rate. To play the frames at another rate,
which changes pitch and duration, set `wav.reinterpret_rate: true`; it appears
as `rate_reinterpreted` with the pitch change in cents. To keep pitch and
change the rate, use `target_sample_rate` instead.

`smpl` loop points, unity note and pitch fraction are always reported and used
only on request. `wav.smpl_loop: <index>` keeps that loop with
`loop_policy: "preserve"`; it must be a forward (type 0), infinitely repeating,
whole-sample loop whose inclusive end is the last frame. `wav.loop_start_frame`
selects a loop start explicitly. Raw `loop_offset` and `loop_repeats` are not
used for WAV. `wav.smpl_root: true` takes the root note from `smpl`.

## PlayStation SPU ADPCM and VAG

PSX ADPCM is its own variant: 16-byte blocks of 28 samples with filters 0-4,
shifts 0-12 and loop flags in every block. It is not BRR, CD-XA or another
"ADPCM". Use it as a source or target like the formats above; BRR to PSX ADPCM
and back goes through the decoded PCM16 stream and is measured.

A `vag` source is a whole file whose magic is `VAGp`. `load_instrument_source`
discovery shows its declared `sample_rate`, data size, name and any trailing
bytes; pass that rate as `sample_rate` (or set `psx_adpcm.reinterpret_rate:
true` to play the blocks at another rate, reported as `rate_reinterpreted`).
Interleaved `VAGi` and byte-reversed `pGAV` files are refused. If the data
size does not fit, read the blocks as `encoding: "psx_adpcm"` with
`offset: 48` and an explicit `length`.

The stream ends at the first block with the loop-end flag; the slice must end
there. The report's `psx_adpcm.ending` says how:

- `end_mute`: a one-shot.
- `silent_loop_block`: the documented one-shot terminator (a silent block that
  loops on itself). It is a one-shot; `loop_policy: "preserve"` is refused.
- `loop`: loops to the last block with the loop-start flag. When no block has
  that flag, the loop address is not in the data and the call is refused;
  pass `loop_offset` (a 16-byte block boundary in the slice, counted from the
  start of the slice, so a VAG's first block is at 48) only if you know where
  the loop starts. A loop address outside the slice is refused.

Refusals name the block and byte. Filters 5-15 are undocumented and always
refused. Shifts 13-15 are reserved: they are refused unless you set
`psx_adpcm.reserved_shift_as_9: true`, which decodes them as shift 9 (the
documented rule) and appears as `reserved_shift_interpreted`. Flag bits 3-7 are
kept in the original but not reproduced (`flag_bits_not_carried`).

Targets:

- One-shot (`loop_policy: "finite"`): by default the encoder appends the
  documented silent terminator block, counted in `padding`;
  `psx_adpcm.one_shot_end: "end_mute"` instead ends on the last data block.
- Preserved loop: the loop start and length must be multiples of 28 samples,
  otherwise the call is refused with recoveries for your source
  ([loop fit](#loop-fit-and-loop-diagnostics)). The
  loop block restarts with filter 0 and carries the loop-start flag; the last
  block carries loop end and repeat. Filter 0 there and on the first block
  means no prediction from history, so the largest errors usually sit at the
  attack and the loop re-entry (see `sample_values` `first_changed_sample` and
  the first-pass and loop-traversal counts).
- `vag` writes a 48-byte header. From a VAG source every header byte is kept
  except data size, sample rate and an explicit `psx_adpcm.name` (printable
  ASCII, at most 16 bytes); `psx_adpcm_target.vag.changed_header_offsets`
  lists the changed bytes. `target_loop_offset` includes the header. A VAG
  header has no root note: a `smpl` root or declared `root` stays in the
  plan's `tuning` but not in the file, so when you convert that VAG onward
  (to BRR, for example) declare `root` again.

The tuning report adds `spu_pitch`: the pitch register value
`round(rate * 4096 / 44100)` (0x1000 plays at 44100 Hz, 0x4000 is the fastest)
and, with a root, the in-tune value and note range. Like `snes_pitch`, these are
computed, not observed.

Decoding follows the documented equation, including its +32 rounding and
clamped history. Other decoders (FFmpeg's `adpcm_psx`, for example) round
predictions differently, so their output can differ by a few to tens of units
on filters 1-4 even for the same bytes. Report Tunemend's decoded values as the
documented reading, not as what a console plays.

## Mega Drive packed 4-bit DAC samples

`md_dac4` is headerless sample data for the YM2612 DAC: each byte holds two
DAC values, each nibble being the upper four bits of the 8-bit value (lower
bits zero). The bytes carry no length, rate, loop or end marker, so the
slice is the whole sample and three things are always yours to declare:

- `md_dac.nibble_order`: `high_first` (bits 4-7 play first) or `low_first`.
  It is required whenever the encoding or target is `md_dac4` and refused
  otherwise. It is a property of the driver that plays the data, not of the
  bytes; take it from the driver context you were given. It was observed as
  `high_first` for one driver family; nothing in the data proves the order,
  and a wrong order still decodes, so never pick it by trying both.
- `offset` and `length`: where the sample starts and ends inside a bank,
  ROM or extracted file. Use the driver's sample table or another verified
  source for both. The bytes cannot be recognized: for a file named
  `.packed4`, and only when nothing else matches, discovery lists
  `md_dac4_candidate` with `basis` "file name only" and what still has to be
  declared. It is a pointer to this section, not a format identification;
  other headerless files (banks, ROMs) get no hint.
- `sample_rate`: the driver's DAC write rate, set by its timing loop. It is
  not in the data. Take it from the driver context when you have it;
  otherwise measure it from a reference capture
  ([choosing the rate](#choosing-an-md_dac4-sample-rate)). Either way it is a
  playback assumption, like a BRR rate: state its source.

The decoding is linear: PCM16 = (nibble - 8) * 4096. A `u8` target gives the
8-bit DAC values exactly (`sample_values_exact` with no changes); `s16le`
gives PCM16 exactly; `md_dac4` to `md_dac4` with the same order is
byte-identical. The report's `md_dac` section gives the declared layout, the
coordinates of sample k (slice byte k / 2 and which nibble), byte and sample
counts, the nibble histogram and range, the counts of nibbles 0 and 15
(values at the extremes) and the first bytes with their DAC values.

Encoding PCM to `md_dac4` rounds each sample to the nearest of 16 levels
(`floor((pcm16 + 2048) / 4096) + 8`, clamped to 0..15). This loses 12 bits;
expect `sample_values` with errors up to half a level (2048 PCM16 units) on
real material, and values above +30719 clamp to the top level
(`md_dac_target.clamped_samples`). An odd sample count gets one midpoint
nibble (8), reported as `padding`. Accept those kinds at export only when the
request allows a 4-bit result. Resample with `target_sample_rate` to the
declared driver rate; the target plays at whatever rate the driver uses.

The format holds no loop. `loop_policy: "finite"` refuses a `loop_offset` on
an `md_dac4` source (there is nothing to bake). `loop_policy: "preserve"`
needs `loop_offset` (a slice byte) on such a source; a target loop must start
and end on whole bytes (an even sample start and length), otherwise the call
refuses with recoveries ([loop fit](#loop-fit-and-loop-diagnostics)). The
target's `target_loop_offset` is reported in `report.json` only; the driver
must be told about it some other way.

Refusals name the field (`/md_dac`, `/md_dac/nibble_order`) or the slice
byte where the 8388608-sample limit (4194304 bytes) is reached. An empty slice
is refused. The external codec is not available for `md_dac4`.

`md_dac4` is a sample representation, not a driver bank: replacing a sample
in a game also needs the driver's table entry (pointer, length, timing) and
bank placement, which consumer integration owns.

### Choosing an md_dac4 sample rate

Without driver documentation, the rate can be measured from a recording of the
driver playing the same sample (an emulator or hardware capture). Tunemend does
not estimate it for you; you compare trial rates:

1. Bracket it: `measure_audio` on the capture gives `silence.leading_seconds`
   and `trailing_seconds`; samples / active seconds is a rough rate. It depends
   on the silence threshold (a few percent), so treat it only as a range.
2. Make a mono copy of the capture: `prepare_sample` with `encoding: "wav"`,
   `wav.channel: 0` and target `s16le` at the capture's rate (`compare_audio`
   needs the same channel count and rate; WAV sources hold at most 8388608
   frames, so a longer capture cannot be compared this way).
3. Coarse sweep: for trial rates across the range in about 50 Hz steps,
   prepare the payload as `md_dac4` with that `sample_rate`,
   `target_sample_rate` set to the capture's rate and target `s16le`, export
   it, and `compare_audio` the mono capture (reference) with its `target.wav`
   (candidate), `max_offset_seconds: 0.05` and `candidate_range` of the first
   0.1 s. Keep the rate with the highest `difference.alignment.correlation`.
4. Fine sweep around it in 1-2 Hz steps over the full length. The true rate
   gives a narrow correlation peak with a steady `offset_samples` (on three real
   captures: 0.86-0.95 at the peak, a few hundredths lower 1 Hz off, about
   0.5 at 5 Hz off). A flat curve or a peak below about 0.7 means the order,
   extent or capture does not match: do not pick the best of bad fits.
5. Report the result as an assumption: the capture's identity, the rate and
   step, the peak correlation, and that it describes that capture's playback,
   not a documented driver rate. A reference recording is usually filtered and
   DC-blocked, so correlation stays below 1 even at the right rate.

## CRI ADX streams

ADX is a stream format (Saturn, Dreamcast and later CD titles), not a sample
plan: streams are minutes long and often stereo, so they have their own tools.
Retain the file with `load_instrument_source`; discovery reports `format:
"adx"` with version, channels, rate, samples, highpass and loop state, or
`unusable` with the byte that stops interpretation. Retention and
`export_instrument_source` never depend on that.

- `inspect_adx` `summary` gives every interpreted value, `unknown_header_bytes`
  (offset, length, nonzero count), the loop in samples and absolute byte
  offsets with `start_byte_consistent`/`end_byte_consistent`, and the `end`
  word (`0x8001`, padding length, nonzero padding, trailing bytes). `header`
  lists each field's offset and raw bytes; `blocks` pages block rows;
  `samples` pages decoded frames with the byte, nibble, scale and prediction
  behind every value. Follow `next_cursor` as `offset`.
- `render_adx` writes `decoded.wav` (PCM16 at the header rate), the exact
  `original.adx` and `render.json`, and retains the WAV as `decoded_source`:
  pass it to `encode_adx` (the response's `encode_adx.arguments` already holds
  the source, highpass and loop) or, for one channel of at most 8388608 frames,
  to `prepare_sample` without loading it again. `measure_with.audio` goes to
  `measure_audio` and `compare_audio`. The `stream` span keeps the loop as an
  `smpl` loop (end inclusive = loop end - 1). `loop` with `passes` unrolls the
  loop with carried decoder history and reports `reentry_check`: the history at
  the first entry and at the jump, and how the second traversal differs from
  the first (`differing_frames`, `max_abs_difference`, `identical_from_frame`,
  `seam_step`). A difference there is how the stream itself loops, not a
  decoding error. `frames` decodes a range from the stream start, so its values
  equal the stream render; a mono range of at most 8388608 frames (choose a
  channel with `wav.channel` in `prepare_sample`) is how ADX audio becomes a
  sample plan source.
- Refusals name the byte: encoding types other than 3 (2 preset coefficients,
  4 exponential scale, 0x10/0x11 AHX), flags 0x08 (encrypted) and any other
  nonzero flags, scale words above 0x1FFF, inconsistent loop fields and
  truncated data. There is no key handling; an encrypted stream stays retained
  but is not decoded. Report the refusal instead of guessing.

The default arithmetic `fixed_12` rounds the derived coefficients to 4.12 fixed
point and keeps clamped history; `arithmetic: "float_reference"` follows the
floating-point description (truncated prediction, unclamped history) and can
differ by tens of units. Report decoded values as Tunemend's declared reading,
not as what a console plays.

`encode_adx` writes a new retained ADX (version 3, type 3, 18-byte blocks) from
a retained PCM16 WAV. Declare `highpass_frequency` (observed CRI streams use
500); `loop_frames` takes a start on a 32-frame boundary and an exclusive end
(the refusal names the nearest starts). For a start off the boundary,
`loop_start_padding: true` prepends silent frames to reach it, writes their
count to the header's loop alignment field as CRI streams do, and reports
`lead_in_padding` (every frame moves later by that count); `sector_bytes` (such as 2048) aligns the
loop-start byte and file length as CD streams do. ADPCM is lossy: the first
call refuses with `adpcm_quantization` and per-channel error, and a WAV whose
`smpl` loop, root or other chunks are not carried adds
`wav_metadata_not_carried`. Accept only changes compatible with the request in
`accept_changes`, with a new `job_id`. The result's `readback` and
`loop_reentry` come from parsing the written bytes again; compare
`decoded.wav` with the source WAV using `compare_audio`.

Expect a click at the loop jump when the loop's last two frames differ from the
two frames before its start: the decoder keeps its history across the jump,
and the encoder codes the loop-start block for the history of the first pass.
A whole-file loop (start 0) enters with silence the first time, so unless the
file ends near zero the second traversal starts with a transient: on test tones
up to about 11600 units for a loud loop end, about 750 for a zero-crossing
start. Check `loop_reentry` (`max_abs_difference`, `identical_from_frame`) and
listen to a `loop` render with 2 passes. To reduce it, start the loop after a
lead-in: a lead-in that is a whole number of periods of steady material
re-enters identically, and copying the loop's last 32 frames before its start
cut the measured transient by 4 to 12 times. Otherwise let `rank_adx_loops` choose candidates: give the WAV, the highpass
you will encode with, a start range and an end range (exclusive ends; at most
1,000,000 pairs). It ranks 32-frame-aligned starts with ends by the predicted
re-entry difference and returns each candidate's `loop_frames`, predicted
difference and settle frames. It applies nothing: pass the chosen
`loop_frames` to `encode_adx`, then check `loop_reentry` and listen to a
`loop` render, because quantization is not in the prediction (on a corpus
jingle the predictions were within about 10% of the measurements; an abrupt
attack just before the loop start can measure well above the prediction). Corpus loops start after a lead-in and still differ for tens to
hundreds of frames; that is how those streams loop.

## Nintendo DS sound data

DS sound files are SDAT archives and their members: SWAR wave archives (each
wave a PCM8, PCM16 or IMA-ADPCM SWAV block, looped or one-shot), standalone
SWAV, SBNK instrument banks and STRM streams. Load the file with
`load_instrument_source`; discovery names it (`nds_swar`, `nds_sdat`, ...).
Then use `inspect_nds_sound`:

- `summary` first: header, counts, encodings, storage layout and refusals.
  For an SDAT it counts members by kind; `view: "members"` pages the FAT
  files with their SYMB names and INFO references (a bank's four wave
  archives, a sequence's bank, a stream's volume). Pass `member` (a FAT
  index) to interpret one member in place; offsets are then member-relative
  and `member_offset` locates them in the SDAT.
- `waves` and `wave` (`index`) for SWAR/SWAV. `wave` decodes the wave and
  gives its readback hash, loop start sample, IMA-ADPCM header and loop
  state, and how to reach `prepare_sample`.
- `instruments` for an SBNK: type (empty, single, drum set, key split or
  undocumented), each note definition's SWAV index, wave-archive slot, base
  note, attack, decay, sustain, release and pan. Only as an SDAT member does
  the slot resolve to a FAT member and say whether the SWAV index exists
  (`swav_in_range`).
- `blocks` for an IMA-ADPCM STRM: each block's own header per channel.

Refusals name the byte (`/source/bytes/N` or `/member/M/bytes/N`) and field;
the original stays retained. A refused wave or instrument does not hide the
others: the summary lists them. Do not repair a file to get past a refusal.

`export_nds_sample` writes one wave (`wave`) or one stream channel
(`channel`) to a new folder: `original.bin` (the exact bytes),
`decoded.wav` (mono PCM16 at the header's rate, with a `smpl` loop when the
source loops), `report.json`, and with `audition_loops` an `audition.wav`
with that many loop traversals baked in. The rate is the header's
`sample_rate`; the report also gives the timer's rate and the difference in
cents. `root_note` sets the WAV unity note; the SWAV itself has none, and an
SBNK definition's `base_note` is that bank's choice for that note range. The
decoded WAV is retained: pass the report's `prepare_sample.arguments` (add
`job_id` and `target`) to convert it, with `loop_policy: "preserve"` and
`wav.smpl_loop: 0` for a loop. PCM8/PCM16 waves are also direct raw slices
(`s8`/`s16le`); `wave` view gives those arguments. Streams longer than 8388608
samples exceed `prepare_sample`; use the WAV directly or an excerpt.

`replace_nds_wave` writes a new SWAR with one wave replaced, from a retained
WAV (`channel` for multi-channel files) at that WAV's rate; resample first with
`prepare_sample` `target_sample_rate` and use its `target.wav`. Choose
`encoding` (`pcm8`, `pcm16`, `ima_adpcm`). A loop comes from
`loop_start_frame` or `smpl_loop` and must start and repeat on whole 32-bit
words: multiples of 4 (PCM8), 2 (PCM16) or 8 (IMA-ADPCM) samples; the refusal
lists fitting starts. One-shots are padded with zeros to a word. Accept only
the reported changes you intend: `sample_values` (lossy encoding),
`bit_depth_reduction`, `padding`. The report lists every moved offset and
changed header field and confirms every other wave is byte-identical. For an
SDAT member the result is a standalone SWAR: the SDAT, its FAT and any SBNK
are not rebuilt, and inserting it is the consumer's job.

The decoded values follow the documented DS arithmetic, which clamps at
-7FFFh; another decoder that clamps at -8000h differs by one unit after a
negative saturation. The audition is the decoded samples only: no DS mixer,
interpolation, SBNK envelope or sequence playback. SSEQ and SSAR are
retained with their header reported; they are not played.

## Root pitch and the SNES pitch register

`root: {"note": 0..127, "cents": -100..100}` records the note the sample sounds
when played at its declared rate. The report's `tuning` section records the
rate, the root and its source (`explicit` or `smpl`). For a BRR target it also
gives `snes_pitch`: `original_pitch` is the 14-bit S-DSP pitch register value
`round(rate * 4096 / 32000)` that plays the sample at its original pitch on the
32000 Hz DSP output, with the rounding error in cents. With a root,
`root_in_tune` corrects the root's cents and `in_tune_note_range` gives the MIDI
notes whose in-tune register fits 1..0x3FFF. A value outside that range reports
`in_range: false`. These are computed values, not observed playback; envelopes,
interpolation and driver pitch tables are separate.

## Level changes

`gain_db` (−96..24 dB, non-zero) scales the decoded PCM16 stream at the
source rate, before any resampling and before the target is encoded: each
value is multiplied by 10^(gain_db / 20), rounded half away from zero and
clamped to 16 bits. A negative gain therefore also gives the resampler
headroom; any overshoot the resampler still produces is counted separately
in `resampled.clipped_samples`. It is reported as the `gain` change with
`factor`, `clipped_samples` (and
`loop_traversal_clipped_samples` for a preserved loop), `peak_before`,
`peak_after` and `measured_level_change_db`; the plan records `gain_db`.
`sample_values` then compares the target with the scaled (and resampled)
stream, so it measures only the target encoding. Accept `gain` at export like any other
change. A positive gain that clips is still reported as requested; lower it
or accept the clipping deliberately. Omitting `gain_db` (or passing 0) keeps
the level and the plan of the same request without it. Use it, for example,
for a quieter copy of a sample for a bank replacement, instead of a project
render.

## Resampling

`target_sample_rate` resamples the decoded stream before encoding. It always
appears as `resampled` with source/target rates and sample counts. The method is
a deterministic Kaiser-windowed sinc filter (beta 9, 32 zero crossings per side,
cutoff at 0.9 of the lower Nyquist rate, unit DC gain). It is flat within 0.001
dB to 0.8 of the lower Nyquist rate, -0.23 dB at 0.85 and -6 dB at 0.9; measured
rejection of content from 0.99 of the output Nyquist rate on is at least 98 dB. Content
between about 0.85 and 0.99 is attenuated rather than preserved. The result is
rounded to PCM16 (`clipped_samples` counts overshoot), so a 16-bit source gains
roughly 0.3-0.4 LSB RMS of rounding error. `sample_values` then compares the
target with the resampled stream.

With `loop_policy: "preserve"`, the loop start and length must scale to whole
target samples, and a BRR target still needs 16-sample alignment at the new
rate; otherwise the call is refused rather than shifting the loop, with
recoveries in source frames ([loop fit](#loop-fit-and-loop-diagnostics)). The loop
region is resampled as its repeating steady state, so every target traversal is
identical; the attack sees silence before it and the loop after it. The report
gives `target_loop_start_sample`. A one-shot stream is silent outside its
samples and yields `ceil(samples * target / source)` samples.

## Loop fit and loop diagnostics

A preserved loop must start and end on whole target blocks after resampling:
16 samples for BRR, 28 for PSX ADPCM and VAG, 1 for PCM. A loop that does not
fit is refused, and the refusal lists `Recoveries` in source units (WAV frames
or raw samples), because those are what you choose:

- the nearest loop starts below and above that fit at this rate (as
  `wav.loop_start_frame` or `loop_offset` bytes); with resampling they are the
  multiples of block × source rate / gcd(block × source rate, target rate),
  such as 49 frames for BRR from 44100 to 28800 Hz;
- the nearest fitting total lengths (the loop runs to the last frame, so that
  needs a WAV or slice of that length);
- the loop's length in periods of its fundamental, which is the same at every
  rate, and fitting starts at which the loop spans a whole number of periods;
- target rates near the requested one, each with a start that fits there and
  makes the loop a whole number of periods;
- up to two `target_sample_rate` values at which the loop fits as it is;
- the one-shot alternative (`loop_policy: "finite"`; for a WAV, drop the loop
  choice).

A plan that preserves a loop gets `report_guide.loop_diagnostics`, measured on
the decoded target (`target.wav`: the first pass, then one traversal with
decoder history carried) at the target rate:

- `fundamental_hz`, `period_samples` and `periods_in_loop`; `phase_jump_periods`
  is how far the loop is from a whole number of periods, the phase the waveform
  jumps at the seam on every repeat;
- `seam`: the sample `step` at the seam against the `largest_step_inside_loop`;
  `waveform_error`, the RMS difference between one period after the seam and
  the period before it, relative to the loop RMS, against the same measure in
  the middle of the loop (`waveform_error_inside_loop`); and `level_change_db`,
  the RMS at the loop start against the loop end;
- `sustain_pitch`: the YIN pitch of the traversal repeated, with
  `cents_from_root` when a root is declared and the spread of its windows;
- `flags`: the measures beyond their thresholds (stated in `method`), and an
  empty list when none is;
- `whole_period_starts` when the phase jump exceeds 0.05 period and the loop is
  chosen in frames or samples: fitting source starts at this rate and, under
  `other_rates`, at nearby target rates, with the periods each spans.

A loop of a fractional number of periods (52.33, say) clicks or buzzes at the
seam even when it is aligned: pick a whole-period start from the refusal or the
diagnostics, prepare it, and read the diagnostics again; they do not check the
waveform at a start you have not prepared. Diagnostics measure the decoded
samples; they are not a listening judgment.

## Inspect and export

Inspect the returned plan with `inspect_sample`. By default it returns a
summary: `accepted_changes` (the change kinds `export_sample` needs), the
conversion's measured `changes`, loop, tuning and WAV facts, `brr_target` block
facts and `loop_diagnostics`. `detail: "full"` returns the stored report and the
whole `report_guide`. The report distinguishes
`sample_values` (changed count, maximum absolute error and RMS in signed PCM16
units), `padding` (BRR or PSX ADPCM block alignment and terminator, or the `md_dac4` byte fill), `finite_loop`, `resampled`,
`channel_downmix`, `bit_depth_reduction`, `rate_reinterpreted`,
`reserved_shift_interpreted`, `flag_bits_not_carried` and `gain`; `md_dac4` targets
report `sample_values` and `padding` as above. BRR to PCM16
can preserve the finite decoded stream exactly. Eight-bit PCM quantization and
BRR encoding can change samples. BRR encoding uses greedy block search; it is
not an optimal compressor. Retaining the original does not make these changes
lossless. When exact output is required, choose a representation with no changes
or report the constraint; do not accept quantization on the person's behalf.

`prepare_sample` and `inspect_sample` also return a `report_guide`, derived from
the report and not stored in the plan. Its `histograms` repeat the `filters` and
`shifts` arrays (index = filter or shift number, value = block count) as objects
keyed by that number; `brr_target` gives a BRR target's block statistics like
`psx_adpcm_target` (blocks, filter and shift counts, end flags, loop block,
filter 0 on the first and loop blocks); `value_domains` says which values are
the source's (`conversion.sample_rate`, `conversion.loop.loop_start_sample`
before resampling) and which the target's (`target_sample_rate`,
`target_loop_start_sample`, `target_loop_offset`, `tuning.sample_rate`); and
`null_fields` explains top-level nulls such as `loop_offset`, which is always
null for WAV sources (their loops are frames in `wav`).

Call `export_sample` with the plan, a new relative `destination`, and exactly the
inspected `accepted_changes` keys. Its result gives the `destination` and
`files`, the written path of every file by name. This is the agent's explicit mapping choice
within the task, not a mandatory human approval gate. An empty list permits only
an unchanged finite decoded stream. Delivery includes `original.bin`, `input.bin`,
`sample.bin`, `source.wav`, `target.wav` and a hash/provenance report.
`source.wav` holds the decoded mono PCM16 source at its declared rate and
`target.wav` the decoded target at the target rate. For a preserved loop both
hold the compared stream (first pass plus one loop traversal) and carry a `smpl`
chunk whose forward loop repeats that traversal, so samplers play the loop; its
unity note is the root when given, otherwise a placeholder 60. A BRR
`sample.bin` carries its loop in the end/loop flags at `target_loop_offset`, a
PSX ADPCM or VAG one in its loop-start and loop-end flags; raw
PCM `sample.bin` has no loop field, so its loop is recorded only in
`report.json`. Finite previews have no `smpl` chunk. Bank input's original is the
exact extracted BRR; the complete native bank remains retained under its bank
ID. Saved plans resume without input or codec paths. Stored identities and
exported bytes are checked. Codec reports describe decoded samples, not hardware
playback.

`u8` and `md_dac4` are suitable sample data for a YM2612 DAC pipeline, not an
FM patch, a driver sample table or a complete MD song. BRR output is sample data, not a complete SFC bank; PSX ADPCM
or VAG output is sample data, not a VAB bank or SPU memory image. No
route establishes native insertion, looped instrument replacement, envelopes,
interpolation, DAC timing, mixing or original sound equivalence. Reference WAVs
represent decoded samples at the declared rate. Use consumer integration and
runtime/listening checks for those additional claims.

A retry with the same `job_id` replays the stored result when the arguments mean
the same request, including omitted versus explicitly written defaults.
