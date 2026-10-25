# Arrangement

Use live schemas for operation arguments. This reference covers decisions that affect preservation and the delivered result.

## Arranging an existing MIDI

Load the original with `load_midi`, inspect its summary and events, then use `import_midi` only when an editable projection is needed. A projection refusal does not mean the original failed to load. Containers retain their complete bytes and identify child MIDI sources; choose a child before inspecting events.

Use protections on every edit that must preserve a part. A voice protection covers notes, controllers, clips, mixer state and instruments. A range narrows timed items, but the voice's strip, sends, effects and instrument remain protected as a whole.

Source-backed edits retain track/channel identities, original markers, end/loop boundaries and RPN ranges. Single-port declarations and channel prefixes are preserved; multiple receivers remain unsupported. Same-track tempo events at one tick retain their order, with the final value shown in the editor. External SMPTE offsets and terminal controls stay in the source rather than moving the local editable timeline. Unsupported audible events can block a preview without blocking original MIDI delivery. Do not discard those events to make a render succeed. Initial GM resets, channel pressure and CC20–31 SoundFont modulation can play in the reference model; custom game loop meanings are not inferred from those controllers.

With no musical edit, verify `compare_midi` reports `byte_identical: true`. With edits, inspect intentional differences and the export's source-preservation/readback results. Retain the original in the bundle.

## Phrases and performance

Choose performance edits for a musical reason: dynamics for a phrase, strumming for a chord, timing variation for a particular feel. Record a seed when using `humanize_notes`. Timing changes can introduce overlaps in a single-line part; inspect advisories and re-check its intended polyphony.

No note may cross `loop_start`; tempo and controller state must return to the loop's starting values or have an explicit change at the start. Keep automation points before playback end.

A timebase cannot change after project creation. Effect positions use integer microseconds; a 60 Hz frame is fractional in that unit. Choose representable timing and inspect export diagnostics.

## Choose what carries the sound

For MIDI delivery, use MIDI volume, pan and expression lanes for changes that must survive another player. Project mixer settings, effects, chip instruments and audio clips live in the bundle and rendered audio, not in MIDI.

For the project's mix, inspect routing before changing levels. Use buses for shared processing, and clear diagnostic mute/solo settings before presenting a candidate. Effects and instruments apply to an entire voice, even when a note edit is range-scoped.

Use effects to solve a named problem rather than to fill a chain. A sidechain cannot react to a part omitted from the render. Frequency automation may need an exponential curve for an even octave sweep; decibel automation is already logarithmic. Clear dependent automation when replacing or removing its processor.

Audio clips retain their source WAVs in the bundle. A clip-only voice still counts toward a voice-count profile, even though its MIDI track has no notes.

## Chip instruments

Audition a retained BRR sample or FM patch alone before binding it with `set_instrument`. Declare pitch mapping, envelope or FM context, velocity behavior and level; program numbers do not infer a chip instrument. See [instrument banks](instrument-banks.md) and [FM banks](fm-banks.md) for source and audition procedures.

Choose one path for volume/expression/pan: apply MIDI lanes through `channel_controls`, or reproduce them in the mixer. Applying both doubles the change. Read the render's applied controls and `not_applied` report; chip instruments do not reproduce pitch bend or driver programs.

Each instrument voice has its own chip instance. A successful render therefore does not prove the entire arrangement fits one physical chip. Check the total budget against the consumer's profile; `snes-dsp-voices` counts release tails and can reserve voices for effects. The abstract `test-four-channel` profile is only a test fixture.

Describe these auditions as Tunemend's model with declared settings. Hardware, driver envelopes, pitch tables and runtime behavior require separate evidence.
