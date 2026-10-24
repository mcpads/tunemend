# Reusing instruments across native songs

Use actual sample data. MIDI program numbers contain no samples. This workflow
neither separates an instrument from a mixed recording nor normalizes loudness.

## Retain and discover the source

`load_instrument_source` retains every original file byte and returns a source
ID. Optional Compile LoROM discovery reports module catalogs even when modules
are scattered through the file. Copies with a 512-byte header, repeated pointers
and empty modules retain their original ranges. No game name or ROM hash selects
a source profile. Unknown inputs remain loadable without a candidate bank.

Use `inspect_instrument_source` with `view: "sources"` to resume saved work;
`summary` reports catalog counts, `modules` and `banks` page discovered entries,
and `bytes` reads original ranges. `export_instrument_source` writes exact
`original.bin` into a new folder independently of mapping support.

A bank candidate supplies `selection` offsets and layout `hints`. Supply its
source ID and selection as `load_instrument_bank.archive`, together with an
explicit `layout`. Alternatively, supply the separate `template`, `bank`,
`driver`, `resident` and optional `sample_modules` paths. Do not mix the two forms.
The source remains in the store, so original paths need not remain accessible.

The layout declares directory/tuning addresses and slot count, envelope table
address/count, exclusive sample-memory limit and reserved sample slots. Discovery
hints are structural candidates, not a declaration of consumer ownership or free
memory. `observed_envelope_count` is a lower bound from initial headers, not the
full table length. Reviewed Compile drivers can address 128 ADSR pairs; declaring
that count requires all corresponding bytes to be supplied. Derive memory and
reserved-slot limits from the complete target context. Never infer free memory
from zero bytes.

Several candidates often share the same sample bank, driver and resident
modules and differ only in their song template. Their samples, envelopes and
auditions are then identical; choose the template of the song whose native
tracks you will read or edit, and confirm with `tracks` that the expected track
headers appear. When the summary has no runtime interpretation, the driver is not
reviewed: semantic transfer and fixed-GAIN envelope indices are unavailable,
while exact loading and register-level audition still work. For reviewed Compile
drivers, declare `envelope_count: 128` when the supplied modules cover the whole
256-byte ADSR table; loading refuses if they do not, and you may then declare the
smaller count the supplied bytes cover. If an emulator observation of the running
sound RAM is available, the candidate in use is the one whose directory entries
and sample hashes match it.

## Inspect the instrument and recipient

`inspect_instrument_bank` provides:

- `banks`: discover immutable bank IDs without specifying a bank.
- `summary`: layout, runtime interpretation, capacity, envelopes and provenance.
- `samples`: paged sample slots, exact BRR hashes, lengths, loop offsets, tuning
  bytes and unavailable-entry reasons.
- `tracks`: native initial instrument settings, with track indices and addresses.
- `references`: used pitch/envelope program indices and native-reference coverage.

Opaque banks require identical driver and resident modules. Two reviewed Compile
standard driver implementations also support semantic transfer across different
resident tables: the tool resolves the actual pitch words and envelope programs,
then finds identical target contents. It does not equate table numbers or
approximate a missing pitch curve. Unknown drivers need a verified adapter. Reviewed semantic transfers use sample
indices below 128; higher indices have an additional pitch-carry behavior that
requires a separate interpretation. Original loading remains available.

## Copy and resolve missing tables

1. Select `source_bank` and either `source_track` from the native tracks view or
   explicit `instrument` settings. Explicit settings include `srcn`, `adsr`,
   `octave_shift`, `transpose`, `vol_env` and `vibrato`; do not guess a musical
   octave from BRR bytes. `source_track` supplies native settings directly.
2. Select `destination_bank` and an unused, unreserved `destination_slot` when
   changing one part. A slot affects every voice or percussion entry using it.
3. Call `copy_instrument`. It creates a new bank; neither input changes. Exact
   existing samples reuse storage. Default `placement: "append"` uses declared
   free memory. Explicit `placement: "replace"` requires exclusive storage large
   enough for the source. Shared storage and insufficient capacity refuse.
4. If an exact table/program is missing, inspect `references` and choose unused
   indices in `table_allocation`: `pitch`, `adsr`, `volume`, `vibrato`, or `gain`.
   Allocate only missing contents. The tool checks loaded native references,
   appends exact data and relocates the full table, preserving other entries.
   Unsupported reachable commands, occupied indices and insufficient space
   refuse. Unresolved requests and unloaded future consumers remain outside the
   reported reference coverage. Do not treat that as proof of whole-game safety.
5. Inspect `lineage.transfer`: source/destination samples, exact semantic mapping,
   selected indices, writes, readback results, binding and activation requirements.
   There is no automatic quality reduction to make a copy fit.

Active loop offsets are preserved. An inactive loop pointer retains a valid
in-sample offset, otherwise it points to the copied sample start. BRR bytes are
never decoded, resampled or re-encoded during transfer.

## Audition a slot on the S-DSP

Without a native envelope to reuse, choose explicit registers by the sound:
ADSR1 bit 7 enables ADSR; attack rate AR (0–15, 15 is immediate) and decay rate
DR (0–7) sit in ADSR1, sustain level SL (0–7, 7 is full) and sustain rate SR
(0–31, 0 holds) in ADSR2. Sustained pads want a fast attack, SL 7 and SR 0;
plucks and bells want a fast attack with a lower SL and a non-zero SR so the
note fades while held. Key-off always releases at the fixed hardware rate.
Record the envelope as your choice, not the game's.

`render_instrument` plays one bank slot (or a BRR sample plan) on the built-in
S-DSP voice model and returns a stereo WAV with a report of every register it
wrote. Use it to compare candidate instruments or check a transfer when you
cannot hear the game.

Pass `destination` to also write `audition.wav` and `audition.json` into a new
folder you can hand over; the store keeps the same WAV either way. For a plan,
`source.sample_rate` is the rate of the BRR that plays (after any resampling)
and `source.source_sample_rate` the original input's.

1. Pick the slot from `samples` and an envelope: `bank_table` with the native
   track's `settings.adsr` index (below 128 applies the table's ADSR1/ADSR2 pair;
   128 is the reviewed drivers' fixed GAIN), or explicit `registers`. Driver GAIN
   programs (indices above 128) refuse; give registers instead.
2. Give the pitch explicitly: a VxPITCH `register` value (0x1000 plays one
   decoded sample per DSP sample), a `note` with the `root_note` and the
   `root_register` that sounds it, or for a plan prepared with a `root`, a
   `plan_note` that uses that recorded root and declared rate. The slot's tuning bytes and the driver's note
   arithmetic, vibrato and volume programs are reported, not applied; do not call
   the result the game's rendition of a melody.
3. Set `key_on_samples` and `release_samples` in DSP samples (32000 per second
   nominal). Leave `counter_start` at 0 unless you are matching a capture; it only
   moves rate-limited envelope steps.
4. Read the report: first audible sample, loops, end-without-loop mute, phase
   changes, envelope at key-off and peaks. Measure the WAV with `measure_audio`;
   compare two auditions with `compare_audio`.

To compare with an emucap `record_audio` capture, render with `wav_sample_rate`
set to the capture's declared rate (for example 32040) so `compare_audio`
accepts both files; samples are never resampled. `compare_audio` searches
offsets up to `max_offset_seconds` (at most 2 s) across both full inputs, so start
the capture within 2 s of the key-on, keep one note per capture, and report the
residual it measures. Attach the
capture with `attach_observation` and associate it with the bank or plan. An
exact audition match qualifies the voice model for that setting, not the driver's
playback of a song. Echo, noise and pitch modulation are not modelled.

## Apply and deliver

For an existing native song, set `destination_track` to apply the initial binding
to one native header. By default `native_transpose: "preserve_destination"`
retains the receiving arrangement's transpose. Select `"copy_source"` only when
the donor's pitch context is intended. Noise-mode initial tracks refuse BRR
replacement. Volume, pan, timing, other headers and score streams remain exact.
Later instrument or modulation commands stay unchanged; an initial-header edit
is not an all-occurrences instrument rewrite.

`export_instrument_bank` delivers the native template, sample bank, matching
runtime/sample modules and provenance to a new folder, without MIDI or an
encoder. For consumer integration after tables were allocated, the consumer
must load the complete matching bundle and refresh cached engine pointers before
playback: the reviewed host command is
`0xf1`. Restore the matching runtime context when switching to another bank.
Export does not perform these consumer operations or patch a ROM.

For MIDI arrangement with the existing supported score layout, pass the new ID
as `prepare_target.target.instrument_bank`, omit separate template/bank paths,
and apply `lineage.transfer.binding` to the intended track. The
[score workflow](score-target.md) retains its declared layout restrictions;
native-bank interoperability does not extend an external encoder implicitly.

Data preservation and native-header readback are separate from S-DSP voice audition,
driver execution, perceived loudness, ROM insertion and listening. Choose volume
deliberately when requested and report the actual verification scope.
