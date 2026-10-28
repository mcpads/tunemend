# PlayStation VAB banks

A VAB bank is a PlayStation sound bank: a VH header (programs, tones and a VAG
size table) and a VB body of SPU ADPCM sample regions, either as two files or
as one combined VAB. Use these tools to inspect a bank, reuse or replace one
of its samples, deliver it exactly and audition a tone. A SEQ or SEP sequence
says which programs and keys a song plays; [Sequences](#sequences) covers
reading one, exporting it as MIDI and auditioning it against its bank.

## Load

1. Retain each file with `load_instrument_source`. A bank inside a larger
   container (a `.DAT` or archive) is retained as the whole container. Its
   `discovery.formats` reports a VH or combined VAB as `vab` (header length,
   expected body bytes), headers inside a container as `vab_headers_inside`
   (offsets), and a `.vb` file only as a `vab_body_candidate`, since a VB has
   no header.
2. Call `load_vab_bank` with byte ranges of those sources:
   - separate files: `{"header": {"source": "<VH>"}, "body": {"source": "<VB>"}}`;
   - inside a container: add `offset` and `length` to each part. The VH length
     is `0x820 + 0x200 × (programs with tones) + 0x200`; the VB length is
     the sum of the VAG size table × 8, and its start is not stored in the VH
     (in observed containers it is the next 2048-byte boundary after the VH);
   - a combined VAB: `header` alone, covering header and body.

A refusal names `/header` or `/body`, the byte offset within that part and the
source offset. Counts that disagree with the tables, a VH whose length is not
its declared layout, and regions past the end of the body are refused; fix the
range rather than the bytes. The retained sources stay exact either way.

## Inspect

`inspect_vab_bank` views, paged with `next_cursor` as `offset` (at most 128
items per page; a larger `limit` returns 128 and the response's `limit` says
`clamped: true`):

- `summary`: header fields with offsets, `version_documented`, the tone block
  order (`block_programs`) and `checks`: total size match, trailing body
  bytes, dangling tone references, parent mismatches, the endings count and
  undecodable samples. Read `checks` before relying on the bank.
- `programs`: all 128 entries, used or not, with every field.
- `tones` (optionally `program`): all 16 records of each tone block. Used
  tones give center note, fine tune, key range, ADSR1/ADSR2 (decoded), the VAG
  and its region. Unused records keep whatever bytes the file holds.
- `samples`: each VAG region's body offset, size, SPU ADPCM stream report
  (ending `end_mute`, `loop` or `silent_loop_block`, loop-start block),
  `after_end` bytes that the voice never plays, the tones referencing it and a
  ready `prepare_sample` slice. Stream offsets (`end_offset`,
  `loop_start_offset`) count from the start of the VB; `offsets` gives the end
  and loop start as region, VB and source offsets.

A VAB stores no sample rate. A tone plays its region at
`pitch = 4096 × 2^((key − center + fine_tune/128) / 12)`, 0x1000 being 44100
Hz. This is Tunemend's declared reading (the fine-tune sign is inferred from
real banks, not documented); state it when you report a pitch.

The SPU steps no faster than pitch 0x3FFF (about 176 kHz, two octaves above
0x1000). A key or bend that asks for more plays at that ceiling, so it
sounds flat by the difference. `render_vab_tone` reports this as
`voice.pitch.clamped: true`, and `render_psx_sequence` counts such note-ons
and bend updates in `pitch_clamped`. A nonzero count means some notes play
lower than written: say so, and name the tones (key far above the center
note) if the person asks about pitch.

## Reuse a sample elsewhere

Pass the `samples` row's `prepare_sample` object (drop its `note`, add
`job_id`, `target` and `loop_policy`) to `prepare_sample`. `sample_rate` 44100
is the rate at pitch 0x1000; to keep the pitch a tone gives at a key, use the
rate that key's pitch register implies. Continue with the
[sample conversion](sample-conversion.md) workflow.

## Replace a sample

`replace_vab_sample` writes a new bank; the original bank is unchanged.

- From a new sample: prepare a plan with target `psx_adpcm` (or `vag`) and a
  `root`. Use `psx_adpcm.one_shot_end: "end_mute"` for a one-shot replacing an
  `end_mute` region, and `loop_policy: "preserve"` for a loop. With the default
  `framing: match_original` the new region gets the old region's leading zero
  block and its unplayed after-end bytes.
- From another bank: `donor_bank` and `donor_vag` copy that region exactly.

`layout: fixed` (default) keeps every region offset and refuses a size
change; the refusal states both sizes. A smaller replacement fits the slot
when you add `"accepted_changes": ["silent_padding"]`: unplayed zero blocks
fill it after the loop-end block, reported under `changes`. A larger one needs
a shorter sample or `layout: resize`. Use `layout: resize` only when the
consumer will load the whole new VB and VH together: later regions move and
the report lists them (`shifted_regions`), so anything that addresses SPU RAM
by offset must be updated by the consumer.

The response lists every changed header byte, tone change and the readback.
`changes.region_bytes` counts the region bytes that differ and gives the old
and new endings and loop starts; `changes.block_flags` names each changed
flag byte (block, body offset, old and new bits), its origin (the plan's own
encoding, framing, padding or the donor region) and whether playback can
notice it. Flags are never copied from the replaced region: a re-encoded
one-shot can drop an unused loop-start flag, which is reported as having no
effect under the documented SPU. When the replaced sample loops, prepare the
plan with `loop_policy: "preserve"` and compare the loop starts.
`transfer.total_size.updated` is true only when the value changed.
Every tone in `tones_playing_it` now plays the new sample; if that is wider
than the request, stop and ask. When the new sample's rate or root differs,
`suggested_tone_tuning` gives the center note and fine tune that would play it
in tune; apply them only through `tone_updates`, and only for the tones the
request covers.

## Deliver and audition

`export_vab_bank` writes `bank.vh` and `bank.vb` (or `bank.vab`), `bank.json`
and decoded first-pass WAVs at 44100 Hz. Insertion into a disc or archive is the
consumer's job.

`render_vab_tone` auditions one used tone (`program`, `tone`) at a `note` (or an
explicit `pitch_register`) through a documented SPU voice model with the tone's
ADSR, for `key_on_samples` then `release_samples` at 44100 Hz. Pass
`measure_with` to `measure_audio`. Volumes and pan are reported, not applied,
and the game's sound driver, vibrato and reverb are not reproduced; say that a
render is a reference audition, not what the console plays.

## Sequences

A SEQ holds one MIDI-like sequence (`pQES`, version 1); a SEP holds several.
Neither names its VAB, and file position does not tell you which bank it
uses (in one observed container all sequences sit together after the last
bank). Choose the bank with the binding view: a VH/VB with the same stem is a
good first candidate; inside a container, `binding_candidates` ranks every VAB
header there. Prefer a bank with `notes_without_tone` 0, and report the
pairing and its evidence as an assumption.

1. Retain the file (or the container) with `load_instrument_source`; its
   `discovery.formats` names `psx_sequence` (a whole file) or
   `psx_sequences_inside` (members with `offset` and `bytes`; `count` and
   `truncated` when more than 64). Then call `load_psx_sequence` with
   `{"sequence": {"source": "<id>"}}`, adding `offset` and `length` for a
   member. Without `length` the response's `sequences[].end` is the member
   length; reload with it for an exact member (`trailing_bytes` 0).
2. `inspect_psx_sequence`: `summary` (header fields with offsets, event kinds,
   programs per channel, loops, tempo events, undescribed controls), `events`
   (every event with offset, tick, running status and meaning; page with
   `next_cursor` as `offset`; `index` selects a SEP sequence), `binding`
   with `bank` (programs and keys against the bank's tone key ranges; read
   `notes_without_tone` before auditioning) and `binding_candidates` with
   `bank_source` (a retained container: every VAB header in it ranked by
   notes without a tone, programs without tones and unused bank programs,
   each with its `header` range). Ties are common; break them with other
   evidence (sibling files, names, listening) or ask, then load the bank with
   `load_vab_bank` and confirm with `binding`.
3. `export_psx_sequence` writes the exact `sequence.seq`/`.sep`,
   `sequence-NN.mid` per sequence and `sequence.json`. Each MIDI file keeps
   every event (header tempo and time signature added at tick 0, optional
   `loopStart`/`loopEnd` markers); `not_in_midi` lists what MIDI does not
   hold. Program numbers in it are VAB programs, not General MIDI. Pass the
   file to `load_midi` for source-only `render_preview` (reference
   synthesizer, not the VAB), or use `import_midi` to edit notes. NRPN data
   entry stays opaque and exact through those edits; native loops and tone
   settings do not become editable project controls.
4. `render_psx_sequence` with `sequence`, `bank` and optionally `index`,
   `loop_repeats` (jumps back at the NRPN loop end, default 1), `max_seconds`
   (default 180, at most 600), `tail_seconds`, `gain_db` and `destination`
   returns a stereo float WAV, `levels` and `measure_with`. Read `warnings`:
   samples beyond full scale stay in the float file but clip in playback and
   integer conversion. For a listening copy render again (new `job_id`) with
   `gain_db` at or below the warning's `suggested_gain_db`, and say that you
   did; nothing is rescaled without it.

To put a sequence render on the listening screen, `present_review` needs a
project render: render with a `destination`, retain `render.wav` with
`load_instrument_source`, create a project with one voice whose mixer holds
that WAV as an audio clip spanning its length (see `get_capabilities`
section `audio_clips`), `render_preview` the project and present that render.
Say that the project only carries the sequence render.

Loops are NRPN controllers: `CC99` 20 starts one (count from the next `CC06`,
127 endless), `CC99` 30 ends it. A render takes the loop `loop_repeats` times;
the MIDI export only marks it.

Refusals name the event and its source byte. A refused load leaves the source
exact; check the offset in `inspect_instrument_source` bytes rather than
patching the file. `render_psx_sequence` refuses when a note needs a 25th SPU
voice: real sequences often do, because released voices keep sounding. Choose
`voice_overflow: "steal_releasing"` (or `"drop"`) and report the
`voice_overflow` totals (`events`, `stolen`, `dropped`, `by_channel`, first and
last tick) with the result; `listed` holds only the first 64, with `truncated`
when there are more. It also refuses tone attribute NRPNs,
NRPN uses without a public description (`known: false` in the events view),
bank select, other undescribed controllers and aftertouch; there is no option
to skip them. Reverb settings and marks are reported, not applied.

The render uses Tunemend's declared volume, pan, bend and timing readings
(listed in the report), not the game's sound library. Say that it is a
reference audition, not what the console plays, and compare by listening
before treating differences from another tool's render as errors.

