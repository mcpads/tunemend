# Arrangement

Use live schemas for operation arguments. This reference covers decisions that affect preservation and the delivered result.

## Arranging an existing MIDI

Load the original with `load_midi`, inspect its summary and events, then use `import_midi` only when an editable projection is needed. A projection refusal does not mean the original failed to load. Containers retain their complete bytes and identify child MIDI sources; choose a child before inspecting events.

If `load_midi` reports `timed_content: false`, every event sits at tick 0: there is nothing to play, edit in time or map, and `render_preview` refuses with "source has no timed content". Tell the person the file holds no playable timeline, say what it does hold (for example only port and end-of-track events, from `inspect_midi`), and keep and export it unchanged; never add notes or timing to make it playable.

To hear the original without importing, call `render_preview` with `source`, a new `destination` folder and a full or excerpt `layout`. It exports the original and an unclamped float WAV. Multiple MIDI ports each use their own receiver. SMPTE timing and other events the editable projection refuses can still play. For SMF type 2, choose one `sequence`; the load summary's preview lists the playable sequences and any refused event locations. Read the playback report and measure the WAV. When the response lists `headroom`, samples exceed full scale and the listening screen or a player clips them: render again with a new job ID and `gain_db` at or below `suggested_gain_db` before presenting it, and tell the person the original was lowered for listening only. If the person finds the start abrupt because the music begins at the first sample, render again with `lead_in_ms` (for example 500) instead of changing the MIDI; marks still map to the source. `present_review` warns about any presented render that clips. This is reference synthesis, not the original game's sound. To let the person listen and mark the original, add `"keep_for_review": true`; the result names a `source-render-N` that `present_review` and `record_feedback` accept with `source` instead of `project`. It is not a project render or revision.

Use protections on every edit that must preserve a part. A voice protection covers notes, controllers, clips, mixer state and instruments. A range narrows timed items, but the voice's strip, sends, effects and instrument remain protected as a whole.

Imported notes pair per track, port, channel and key, first in, first out: each release ends the earliest still-sounding note of that key, so stacked same-key onsets are separate notes. Their `duration` in `inspect_project` and the `sounding` lists of `read_feedback` follow that pairing, and the reference receiver releases the same way, ending one instance per Note Off. A long note can still fade before its release when its instrument decays. When two tracks play the same key on one channel, the receiver pairs their releases in merged order, which can differ from each track's own durations; the import's `retained` list names those notes ("also sounds from track"), so measure before stating that one sounds.

When several tracks share one port and MIDI channel, each is its own voice, but the channel's state is shared. A plain project render plays all of them on one receiver, as the source does. When the project renders each voice on its own synthesizer (stems, or a project with mixer settings), every voice's synthesizer still receives all program, controller and pitch-bend events of its port and channel from every track, in source order. A voice without its own program or CC7 lane therefore sounds with the program and volume another track set for the channel. Only notes stay per voice, so a release in one track does not end another track's same-key note there. To check the sounding program, read the channel's `program_change` events with `inspect_midi` (`midi_channel` names the channel) before the voice's notes, and compare the voice's stem with a source render when timbre matters.

Messages and voice IDs number MIDI channels 1–16 (`track0-ch2` is MIDI channel 2). The `channel` fields of `inspect_midi` events and export `assignments` hold the 0–15 status nibble; `inspect_midi` events also give `midi_channel` 1–16. Releases without a sounding note, notes never released and zero-length pairs are listed in `retained` and stay in the source without an editable note; do not invent durations for them. Only when the person asks to edit those notes, import with an explicit `declared_ends` rule (`next_onset`, `track_end` or `fixed` ticks) and limit it with `apply_to` to the patterns the person means, for example `["never_released"]`. Report the declared ends and `not_completed` notes from the import result, and each `declared_end_writes` entry an edit returns. `next_onset` ends a note at the next start of the same key in its track and channel, or at the track end when no later start exists. An end at the content end sounds the same as no declared end, because every render releases held notes there; expect an audible change only for ends before it. Previews stop declared notes at their declared ends; the delivered MIDI keeps the original bytes for untouched notes unless you export with `write_declared_ends`, which writes every declared end and reports it. Retained never-released notes appear in `read_feedback` `sounding` by `source_event`, and every render releases them at the content end. Edits that would create a new same-key stack or change the pairing are refused. When tracks share a port and channel, a lane or same-key note change must include every voice on that channel in the scope, and protecting one of those voices protects it from the others' channel changes.

Source-backed edits retain track/port/channel identities, original markers, end/loop boundaries and RPN ranges. In a multi-port source, `track1-port2-ch3` identifies track 1, port 2, channel 3; port 0 and single-receiver files keep the older `track1-ch3` form. Port events stay unchanged. An edit that moves a note or control onto another receiver is refused; do not remove port events to bypass it. Same-track tempo events at one tick retain their order, with the final value shown in the editor. External SMPTE offsets and terminal controls/tempos stay in the source rather than moving the local editable timeline. Unsupported audible events can block a preview without blocking original MIDI delivery. Do not discard those events to make a render succeed. Initial GM resets, channel pressure and CC20–31 SoundFont modulation can play in the reference model; custom game loop meanings are not inferred from those controllers.

With no musical edit, verify `compare_midi` reports `byte_identical: true`. With edits, inspect intentional differences and the export's source-preservation/readback results: each inserted, removed, changed or moved event is one difference; follow `next_cursor` as `offset` until every difference has been read. Retain the original in the bundle.

## Phrases and performance

Choose performance edits for a musical reason: dynamics for a phrase, strumming for a chord, timing variation for a particular feel. Record a seed when using `humanize_notes`. Timing changes can introduce overlaps in a single-line part; inspect advisories and re-check its intended polyphony.

No note may cross `loop_start`; tempo and controller state must return to the loop's starting values or have an explicit change at the start. Keep automation points before playback end.

A timebase cannot change after project creation. Effect positions use integer microseconds; a 60 Hz frame is fractional in that unit. Choose representable timing and inspect export diagnostics.

## Choose what carries the sound

For MIDI delivery, use MIDI volume, pan and expression lanes for changes that must survive another player. Project mixer settings, effects, chip instruments and audio clips live in the bundle and rendered audio, not in MIDI.

In the reference synthesizer, note velocity, volume (CC7) and expression (CC11) each scale amplitude by (value/127)², which is 40·log10(value/127) dB; the three factors multiply. To change a part's level by g dB, multiply its CC7 values (or CC11 values or velocities) by 10^(g/40) and round. Some examples:

- "20% quieter" as amplitude ×0.8 (−1.94 dB): multiply by √0.8 ≈ 0.894; 100 becomes 89 (−2.0 dB) and 70 becomes 63 (−1.8 dB).
- −6 dB: multiply by 0.708; 100 becomes 71.
- Multiplying CC7 by 0.8 gives −3.9 dB, not −1.9.

Integer values limit resolution: one step near 100 is about 0.17 dB, near 40 about 0.44 dB. Velocity also darkens the tone slightly through the default velocity-to-filter modulator, so for level alone change CC7 or CC11. Other players can use other curves; measure when exact loudness matters, and remember that perceived loudness also depends on the rest of the mix.

For the project's mix, inspect routing before changing levels. Use buses for shared processing, and clear diagnostic mute/solo settings before presenting a candidate. Effects and instruments apply to an entire voice, even when a note edit is range-scoped.

Use effects to solve a named problem rather than to fill a chain. A sidechain cannot react to a part omitted from the render. Frequency automation may need an exponential curve for an even octave sweep; decibel automation is already logarithmic. Clear dependent automation when replacing or removing its processor.

Audio clips retain their source WAVs in the bundle. For neutral float32 delivery, set `normalization: "wav_amplitude"` on the clip. Omission preserves historical PCM16 code scaling; the render reports `source_normalization`. PCM16 delivery still quantizes to −32767..32767. A clip-only voice counts toward a voice-count profile even without MIDI notes.

## Chip instruments

Audition a retained BRR sample or FM patch alone before binding it with `set_instrument`. Declare pitch mapping, envelope or FM context, velocity behavior and level; program numbers do not infer a chip instrument. See [instrument banks](instrument-banks.md) and [FM banks](fm-banks.md) for source and audition procedures.

Choose one path for volume/expression/pan: apply MIDI lanes through `channel_controls`, or reproduce them in the mixer. Applying both doubles the change. Read the render's applied controls and `not_applied` report; chip instruments do not reproduce pitch bend or driver programs.

Each instrument voice has its own chip instance. A successful render therefore does not prove the entire arrangement fits one physical chip. Check the total budget against the consumer's profile; `snes-dsp-voices` counts release tails and can reserve voices for effects, but only for voices with a chip instrument: without one, each note counts only until it ends, and the report's `unchecked` says so, so releases overlapping the next attacks (for example notes ending exactly at the loop return) are not checked. Read `peak_simultaneous_notes` (count, first position and notes) even when the check passes, to see how close the arrangement is to the budget. The abstract `test-four-channel` profile is only a test fixture.

Describe these auditions as Tunemend's model with declared settings. Hardware, driver envelopes, pitch tables and runtime behavior require separate evidence.

NRPN selection and data-entry events remain exact through imported note edits;
they do not become editable parameter lanes. Their preservation does not establish
the original device’s sound or convert native SEQ loops into project loops.
