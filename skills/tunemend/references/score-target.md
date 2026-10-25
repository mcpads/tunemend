# Preparing a driver score

This optional backend targets the Compile score-module API. It requires Python 3
(standard library only), a compatible existing Python encoder, and the target's
score template and matching sample-bank module. None of these driver assets or
external encoder implementations are bundled. Their paths and identities are
inputs, not inferred from a game name, song index or directory layout.

Use `tunemend schema prepare_target` for the full mapping schema. The encoder
module must expose `encode`, `Track.stream`, `track_notes` and the Compile memory
layout constants. Its `mlk_song` and `midi_notes` functions are replaced inside an
isolated process by inspected source data; archive extraction and the encoder's
MIDI parser are not used. This is a specific backend contract, not an arbitrary
Python encoder plugin API. Only use a trusted local encoder.

## Mapping inputs

Supply either a loaded `source` ID or a `project` and optional `revision`.
Source input does not require an editable composition. New projects can map
without loading an external file. Source support here is synchronous PPQ SMF
0/1, including RMID, with unambiguous note pairing. Single-port declarations and channel prefixes are retained in the source and
accounted as metadata not encoded by the driver. Type 2, SMPTE, multiple output ports,
unpaired notes and overlapping same-key notes need a different mapping backend;
these limitations never affect exact source loading.

`target` identifies `encoder`, `template`, `bank`, optional `tuned_srcns` and the
Python executable (`python3` by default). The template supplies percussion and
tuning tables; the matching bank reserves sample memory. Sample numbers, ADSR,
volume/envelope indices, vibrato and pan are actual driver choices, not GM
instrument names. Obtain them from the consumer's verified profile.

Alternatively supply `instrument_bank`, an immutable ID from the
[instrument-bank workflow](instrument-banks.md), instead of `template`/`bank`.
The bank must match this backend's layout; its runtime and sample modules are
frozen with the score plan and included in delivery.

`mapping` controls `ppqn` (12 by default, must divide source PPQ),
`offset_ticks` (0), `whole_bars` (true), `loop` (true), `intro_once` (false),
and one to eight `tracks`. `intro_once` plays initial silence only before the
first loop pass. The selected encoder must implement the chosen policy; a
mismatch refuses the plan. First-pass duration and loop return are checked separately.
Each track supplies a unique `name` and `volume`, then either source `channels`
or a `copy_of` another non-copy track. Optional controls include:

- `voice`: highest-first chord rank at a shared source onset.
- `srcn`, `adsr`, `octave_shift`, `transpose`: sample/envelope and pitch mapping.
- `drums`: MIDI keys to driver percussion entries 0–29.
- `pan`: -128 through 128; positive is left for this backend.
- `quantize: "round"`: allow rounded onsets; otherwise off-grid starts refuse.
- `fill_gaps`: target ticks to join to the next onset.
- `volumes`: explicit source-tick/driver-volume pairs.
- `delay`: source ticks before a copied track enters; copies share the score.
- `same_tick: "last"`: explicitly select the last event sorted by target tick,
  duration and encoded status when target onsets coincide. Otherwise refuse.

Later onsets cut earlier notes in a physical track. Long notes may release after
one duration byte; percussion holds until the next hit. The report accounts for
actual encoded duration and removed notes. Coincident events are selected before
serialization to prevent a legacy writer from appending the same rest twice.

The backend uses the first tempo (or 120 BPM if none), represented by the driver's
integer rate, and the selected loop/end. It reports every unused controller,
bend, program, pressure, SysEx and metadata event. Velocity is replaced by mapped
track volume. These are explicit changes to inspect, never lossless audio claims.

## Inspect, revise and deliver

1. Call `prepare_target`. It snapshots source, mappings, encoder and asset bytes,
   builds a candidate in an isolated temporary directory, and independently
   decodes the generated score. Invalid pointers, ARAM overlap, unexpected
   commands, changed header/table fields or mismatched events refuse the plan.
2. Read `changes`, `encoding` and `verification`. Page `inspect_target` with
   `view: "events"` to see each original event's targets and changes; use
   `view: "tracks"` for settings and event counts, then `view: "score_events"`
   with `track` for paged decoded score events. Copies are listed
   separately with their delays. Follow `next_cursor`.
3. If a reported omission or timing change violates the request, revise the
   mapping with a new job ID. Do not select a loss just because it is listed.
4. Call `export_target` with the exact immutable `plan`, the chosen
   `accepted_changes` (every reported change kind), and a new destination.
   Selecting these changes is the agent's mapping decision within the existing
   request; it does not inherently require a separate human approval.
5. Delivery contains `score.bin`, exact `source.mid`, `template.bin`, `bank.bin`
   and `report.json`. Every stored identity and written file is rechecked.
   Export works after restart without the external encoder or original paths.

Original source preservation, mapped score readback, driver audio, ROM insertion
and in-game playback are separate results. The latter three are not established
by this workflow. Python and encoder snapshots are retained privately in the
workspace store for provenance; export does not redistribute external source.

## Frame-converter handoff

For `convert_for_target`, use the consumer's driver limits, patches and capability profile. Map every voice explicitly and check those constraints first. Never invent patches for an existing game.

The frame adapter refuses source-backed MIDI and reported information loss before invoking the converter. When conversion succeeds, report its capability status and verification scope. Encoding and playback in the game remain consumer work; a generated file alone does not establish that it plays there.
