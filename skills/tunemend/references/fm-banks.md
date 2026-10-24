# Reusing a static OPN FM patch

Use this workflow for an original OPN register bank. Unlike sample reuse, the
transferred data describes four operators and their algorithm/feedback. It does
not contain a waveform. Capability discovery separates this family from BRR
banks and MIDI mappings.

## Load and inspect

1. Call `inspect_fm_bank` with `view: "layouts"` to inspect the supported packing
   and operator-coordinate conventions. Its example is synthetic, not automatic
   source recognition.
2. Establish the record offsets and layout from the source format or verified
   driver interpretation. Supply absolute file `record_offsets`, `record_size`,
   `algorithm_offset`, `operators_offset`, and `packing` (`operator_rows` or
   `register_columns`). `slot_order` maps stored positions to hardware slots;
   the default `[0,1,2,3]` means register offsets 0,4,8,12, whose named operators
   are 1,3,2,4. Do not assume named operator order equals register order.
3. Call `load_fm_bank` with a path or a saved `load_instrument_source` ID, that
   layout, and `chip`: `opn` when only the register family is known, or the
   established `ym2203`, `ym2608`, or `ym2612` model. An optional expected hash
   checks identity. The complete container is retained. Unknown layout does not
   prevent original-byte loading through `load_instrument_source`.
4. Inspect `records` with a page limit. Each record exposes its raw bytes,
   algorithm, feedback, detune, multiplier, total level, rate scale, envelope
   rates, AM flag, SSG-EG, parameter digest and unmodeled register bits.
   `banks` lists saved IDs; `summary` exposes layout and lineage.

## Copy and deliver

Call `copy_fm_patch` with a donor bank/index and receiving bank/index. This is an
explicit replacement of that record for **every reference to it**, not discovery
of an unused slot or a one-track binding edit. Declare protected entries in the
receiving layout's `reserved_records`; aliases and overlapping recipients refuse.
The original banks remain immutable.

The operation maps all 29 static registers: B0 and four slots in groups 30,40,
50,60,70,80,90. It preserves recipient reserved register bits and opaque record
fields, as well as every other bit in the complete container. Readback compares
all interpreted parameters and unselected data before saving. A YM2203 target
refuses a source AM-enable setting; the tool never drops it silently. On a
declared YM2203 source, the unused AM bit stays in the original raw data rather
than becoming an active parameter on another chip.

Inspect the returned transfer scope and identities. Then `export_fm_bank` writes
`container.bin`, `bank.json` and `patches.json` into a new relative folder and
checks exact bytes. Unedited export is byte-identical to the input. These tools
work after server restart without the original path. Reusing a job ID replays
its outcome; revise a blocked request with a new job ID.

## Audition a patch

To design a patch from scratch, pass explicit `registers` and iterate with
auditions and `measure_audio`; record the choices as assumptions. Pick an
algorithm by how many carriers you need (algorithm 0–3: one carrier with a
modulator stack, bright and focused; 4: two pairs; 5–6: three carriers;
7: four carriers, organ-like). Carrier TL sets loudness; modulator TL and
operator-1 feedback set brightness (lower TL or higher FB is brighter). A
brass-like stab usually needs a fast attack (AR 31) with a modulator whose
level falls a little faster than the carrier's; plucks and bells need decay
(D1R, SL) instead of sustain. MUL ratios that are whole numbers give harmonic
tones; detune adds slight beating. Report the patch as your design, never as a
game's instrument.

`render_fm_patch` plays one saved record (`bank` + `index`) or explicit
`registers` through Tunemend's built-in OPN FM model. Use it to hear a patch
before or after a copy, or to compare candidates. It reads banks and never
changes them.

1. Declare the context; nothing is inferred from a game or file. Required:
   `context.clock_hz` and `context.pan`. Add `context.chip` when the bank is
   family-only (`opn`) or the registers are explicit. A context chip that
   contradicts the bank's declared chip refuses: copy the patch into a bank of
   that chip instead. `get_capabilities` lists common clocks under
   `fm_banks.audition`. Use the clock established for the consumer hardware, not
   a guess.
2. Declare channel/global settings when they are known: `lfo` (enable, rate
   0–7), `ams`, `pms`, `prescaler` and `key_on_operators`. The report lists
   every default it applied. YM2203 has no LFO, sensitivities or pan.
3. Give notes with `start_seconds`, `key_on_seconds` (time to key-off) and a
   `pitch`: `{"kind":"register","fnum":…,"block":…}` for exact driver values,
   or `{"kind":"midi_note","note":69,"a4_hz":440}` with optional `block`. MIDI
   pitches report their F-number quantization in cents. Use separate `channel`
   values for overlapping notes. Velocity is never implied: use
   `carrier_tl_offset` (0.75 dB steps on every carrier) when a level difference
   is intended.
4. Choose a new `destination` folder. `tail_seconds` (default 3) bounds the
   release after the last key-off, and `ended_silent` says whether it finished.
   `audition.wav` is at the chip's native rate (header rounded; the report gives
   the exact ratio). `output_sample_rate` adds a separately reported resampled
   WAV.
5. Read `audition.json` or the response: exact patch, resolved context, every
   register write with its sample, per-note F-number/block/key code and
   effective carrier TL, peak/clipping, and the model scope.

A patch that enables SSG-EG refuses unless you declare `ssg_eg: "ignore"`.
Tell the person the audition then lacks that envelope. LFO PM is an
approximation of the documented depth. DAC quantization and the YM2612 ladder
offset are not modelled; low-level tails can sound different on hardware. The
result is Tunemend's model with the declared context, not original hardware,
driver or game audio. Present it that way, and do not call an instrument
reproduced because its audition sounds right.

## Playback context and limits

Static register equality does not prove the same audible instrument. Channel
AMS/PMS and pan, global LFO rate, clocks, special channel modes, operator key
masks, driver macros, later carrier attenuation and the note stream are separate
context. Opaque record bytes are preserved on the recipient, not interpreted as
portable settings. Inspect the relevant driver before integrating the output.

This workflow does not automatically locate banks, read reduced-register formats,
rewrite native song bindings, convert sequences, play driver macros, or certify
runtime activation/audio. `render_fm_patch` auditions only the static patch with
the context you declare. Do not label the entire instrument or song lossless
from this static-parameter check. Do not replace an unknown original with a
guessed General MIDI program, sampled approximation or a normalized volume.
