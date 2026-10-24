---
name: tunemend
description: Compose, arrange and revise retro game music (looping BGM) and short sound effects as MIDI without a DAW, using the tunemend MCP tools. Use when someone asks for game music or a sound effect, wants a MIDI arranged for fewer channels or a target's limits, wants to reuse an instrument or sample from another native song, or has listened to a result and marked places that sound wrong. 고전게임 BGM·효과음 MIDI 작곡·편곡·수정 및 다른 곡의 악기·샘플 이전 요청에 사용한다.
---

# Tunemend

You make the music. The person only says what the scene needs, listens, and marks places that sound wrong. Never ask them to edit notes, name chords or instruments, or operate a DAW.

## Before starting

1. Check that the `tunemend` MCP tools are available (`get_capabilities`). Its default answer is a short summary: version, timebases, limits and one line per section. Ask for the sections your task needs in full, such as `{"sections": ["edit_operations", "mixer"]}` before mixing or `{"sections": ["nes_apu"]}` before a chip render; `{"detail": "full"}` returns everything (about 64 KB). If the tools are not available, install them: see [installation](references/install.md). Installing is your job, not the person's. Without MCP, the same tools run from the command line: `tunemend --workspace <dir> call <tool> --input <file.json>` (or JSON on stdin) prints the same response, `tunemend schema <tool>` prints its input schema, and the exit status is 0 when it succeeded, 2 when it was refused and 1 on an error.
2. Call `inspect_project` with `view: "projects"`. Resume an existing project instead of starting over when the request continues earlier work. If the person hands you a delivered folder, open its `.project.json` with `import_project` and continue from its head revision; if its render WAVs are missing, see Delivering.

## Making something new

1. Write the brief with `create_project` (or `update_brief`): the purpose, the person's requirements in their words, and every assumption you chose yourself (length, meter, tempo, voices, loop). Requirements and assumptions stay separate.
2. Choose the timebase: `music` (ticks, PPQ 480 is a good default, tempo map) or `effect` (microseconds, no beat grid). It cannot change later.
3. Build content with `edit_project`. Write whole phrases per request: `add_voice`, `add_notes`, `set_control_points` (program, volume, pan, expression, modulation, reverb, chorus, pitch bend with an explicit `pitch_bend_range` lane), `set_tempo_map`, `set_time_signatures`, `set_sections` (named regions such as `intro` or `theme-a`), `set_markers` (named positions, such as a cue point where drums enter), `set_playback`; sections, markers and the timeline need `scope.timeline`. Every changing call needs a fresh `job_id`; reuse the same `job_id` only to retry the exact same call.
4. For a loop, set `playback.loop_start` and make the loop seamless: no note may sound across `loop_start`, and tempo and controller values at the end must match those at the loop start (or place a change exactly at `loop_start`). The checks tell you where it breaks.
5. Check limits with `check_project` and an inline profile describing the person's target. With no named target, write an inline profile of what you intend and report it as a sanity check, not a device claim. For an SNES target, the built-in `snes-dsp-voices` checks the S-DSP voice budget (at most 8 notes at once across all voices); pass `reserved_for_effects` (for example 2) when the game keeps voices for sound effects. It is a voice count, not a driver model, so report it as that. Notes of a voice with a chip instrument count until their release ends (the S-DSP release ramp of about 8 ms, an FM patch's release rate, or `chip_release_ms` when the driver's release is known), and a voice's next note re-keys its chip voice as renders do. The built-in `test-four-channel` is an abstract test profile (four voices, 60 Hz grid); a piece with more voices fails it by design, so do not run it as a stand-in or report its result. For example, a six-voice arrangement with a single-line melody and bass:

   ```json
   {"project": "…", "profile": {"name": "intent-6-voices", "description": "Sanity limits chosen for this arrangement; not a device",
     "max_voices": 6, "max_simultaneous_notes": 10,
     "voice_rules": [{"voice": "melody", "monophonic": true}, {"voice": "bass", "monophonic": true, "pitch_max": 64}]}}
   ```

   A rule without `voice` applies to every voice; add `time_grid_per_second` or `loop_support` only when the target needs them. Fix violations by re-voicing, moving or re-rhythming notes yourself; never just delete notes to pass.
6. Render with `render_preview`: the whole piece with `loop_passes: 2` for BGM so the loop return is audible, excerpts or single voices when investigating. For the loop seam, render `loop_passes: 3` and compare pass 2 with pass 3 of the same render ([Measuring audio](#measuring-audio)).
7. Measure the render with `measure_audio` before presenting it (see [Measuring audio](#measuring-audio)).
8. Show it with `present_review` and give the person the URL plus the `guidance_for_person` text. They can mark several points and ranges and deliver them together. Say that the sound is a reference preview, not the target console's sound.

Keep the MCP connection open during review. After reconnecting, retry the original `present_review` call with the same job ID and arguments to recover its live URL without creating another presentation.

## Loading original music

Exact loading is a mandatory guarantee when a file is supplied; new composition needs no source file. Call `load_midi` on supplied files before choosing an arrangement or target mapping. It retains SMF 0/1/2, PPQ/SMPTE, RMID and unmodeled events without requiring an editable composition. A container returns all discovered MIDI child IDs and byte ranges while preserving the complete parent; select a child to inspect its events. No filename or game-specific index format is assumed.

Use `inspect_midi` with `view: "sources"` to resume, `summary` for headers and optional projection support, `events` for original ordered events, and `bytes` for complete raw payloads. Follow `next_cursor`; event previews can reference longer payloads by byte range. `export_midi_source` writes exact original bytes and checks readback. A projection diagnostic is not a failed load. Keep the original source even if no mapping is requested or the editor cannot yet represent it.

## Arranging an existing MIDI

1. Load and inspect the original first. Only when editing is needed, call `import_midi` with the loaded `source` ID. Read its editable projection limits separately from source availability. `analyze_midi` is also available for read-only file inspection; pin `expected_sha256` when importing a file path.
2. Read `inspect_project` summary and events. Decide what must stay (often the melody) and put it in `protections` of every edit request; limit `scope` to what the request allows. A protection covers everything of the voice: its notes, control lanes and audio clips, and its mixer data (strip, sends, effect chain, automation and chip instrument). With a `range`, timed items (notes, control points, clips, automation points) are protected where they overlap it, but the strip, sends, effect chain and instrument stay protected whatever the range, so a `set_instrument` or `set_mix` on a protected voice is refused; protect only what must stay unchanged.
3. Edit notes within existing voices, supported control points, tempo or meter through `edit_project`. Source-backed projects currently retain track/channel identities, markers, end/loop boundaries and RPN ranges. A blocked edit creates no revision; do not remove source events or relax the person's requirements to bypass it.
4. A control lane shows the final value at a tick. Original same-tick events, order and opaque data remain in the source. Rendering uses the preserved sequence; unsupported audible events block preview separately from MIDI delivery. Do not call a reference preview an original-device reconstruction.
5. Export with the original assignments. With no musical edit, use `compare_midi` against the source and require `byte_identical: true`. With edits, inspect the export's preservation report and differences: intentional changed events should differ, and source-edit readback must pass. The original bytes travel in the project bundle for later edits.

If the required operation is outside the reported scope, state the exact missing capability. A new game-specific script or discarded event is not a successful lossless workflow. Use another capable reusable tool or extend the owning implementation when authorized, preserving original data and verification requirements. Report that this MCP does not yet provide the operation; do not ask the person to repair MIDI by hand.

## Shaping performance

These `edit_project` operations work on selected note IDs, in new and imported projects alike, and obey scope and protections like any other edit:

- `humanize_notes`: small timing and velocity variation. Pick and record a `seed`; the same seed gives the same result, so a revision stays reproducible.
- `scale_velocities` (`percent`, `offset`) and `ramp_velocities` (`from`, `to` across the notes in time order) for dynamics and crescendos.
- `legato_notes` joins each note to the next start in its voice, minus `gap`. `strum_notes` spreads chord tones by `step`, `up` or `down`, keeping their ends.
- `ramp_control_points` writes a controller swell or fade (`from`, `to`, a point every `step`, `to` exactly at `end`) and keeps the lane's other points.
- `snap_to_scale` with `root` (pitch class 0–11) and a `scale` name or `intervals`; `stretch_notes` scales timing around an `anchor` by `numerator`/`denominator`.

Results clamp velocities to 1–127; operations that would leave a note with no length or outside the pitch/time range are refused as a whole. Timing changes such as humanize or strum can make notes of a single-line voice overlap; the edit allows that (it is valid polyphony) and its response lists a `voice_polyphony_increased` advisory. If the voice should stay a single line, fix it with `legato_notes` or smaller offsets and re-run `check_project`.

## Balancing the mix

The mixer sets how voices sit together in every `render_preview`, without touching notes. It is saved with each revision, so try a balance, render, and keep or branch like any edit.

Choose where a level, pan or space change lives by where the person will use the result. If they will take the MIDI into another player or engine, change the `volume`/`pan`/`expression`/`reverb` control lanes, because MIDI carries those and not the mixer. Use the mixer (and its effects) for the project's own previews, routing and anything MIDI cannot express, and say in the delivery that the mixer is not in the MIDI.

1. Read the current mixer in `inspect_project` summary (`mix`); `events` lists `mix_automation` points (and `effect_automation` points when effects are automated) in a range.
2. Use `set_mix` on a voice for `volume_db` (0.1 dB steps, -120.0 to 12.0), `pan` (-100.0 left to 100.0 right), `mute`, `solo` or `output` (a bus id or `master`). Prefer small level moves (1–6 dB) and say why in `rationale`.
3. Group or share processing with `add_bus`, route voices with `output`, and add `set_send` (`gain_db`, `pre_fader`). `set_mix` on a bus or `master` (volume only) sets their levels. Bus and master changes need `scope.buses: true`; voice strips, sends and voice automation follow `scope.voices`, `range` and protections.
4. Use `set_mix_automation` for fades and moves: `points` with `at`, `value` and `shape` (`step` holds, `linear` ramps to the next point). With a loop, put a point exactly at `loop_start` when the end value differs. Every point must lie before the playback end (`event_beyond_end` otherwise): to ramp back at the loop end, put the last point at `end - 1`.
5. Mute and solo are for investigating; clear them before presenting a candidate unless the person asked to drop a part, and mention any that remain.

The mixer is not written to MIDI. The export report lists it, and the bundle keeps it. When the delivered MIDI itself must be quieter or panned, change the `volume` or `pan` control lanes instead. Imported MIDI accepts mixer changes as render state; its exported bytes do not change.

### Effects on voices, buses and the master

Effect chains run offline in every `render_preview`, in list order, before a strip's fader and pan; the master chain runs after the master volume. Use them when a balance problem is about tone, dynamics or space rather than level:

- `eq` bands for masking or harshness: a `high_pass` around 80–150 Hz on parts that need no low end, a `peaking` cut (`gain_db` -2 to -6, `q` 1–4) where two parts collide, gentle shelves for overall tone. Prefer cuts and small boosts (up to about 4 dB).
- `compressor` to even a part's level (`threshold_db`, `ratio` 2–4, `attack_ms`, `release_ms`, optional `knee_db` and `makeup_db`). Set `sidechain` to another voice or a bus to duck one part under another; a voice chain can be keyed only by another voice. A key that is not heard in a render (muted, not soloed, or not among the rendered voices) keys nothing.
- `limiter` last on the master to keep peaks under `ceiling_db` (sample peaks; use about -1.0 dBFS for headroom).
- `delay` (`time` in `ms` or as a note fraction such as 3/16 at the tempo), `reverb` (`room_size_percent`, `damping_percent`, `pre_delay_ms`, `mix_percent`), or `snes_echo` (FIR bytes, EDL `delay` 0–15, `feedback`, `volume_left`/`volume_right`) for space. For a shared space, put the effect on a bus with `mix_percent: 100` (or `dry: false` for `snes_echo`) and feed it with `set_send`. `snes_echo` lets the person audition a native-style echo; it runs at the render rate and is not the console's sound.

Operations: `set_effects` replaces a whole chain, `insert_effect` adds one (each effect has an `id`), `move_effect` reorders, `set_effect` replaces a processor's settings or toggles `bypass`, and `remove_effect` deletes. Voice chains follow `scope.voices` and protections and cannot be limited by `scope.range`; bus and master chains need `scope.buses: true`. Render the same layout before and after a change and give the reason in `rationale`.

To move an effect setting over time (open a filter through a phrase, raise the echo return into the loop, tighten a limiter at the climax), use `set_effect_automation` with the chain's `target`, the `effect` id, the `parameter` and `points` like `set_mix_automation`. Automatable: `eq` `gain_db` and `frequency_hz` (give the 0-based `band`; `gain_db` only on peaking and shelf bands), `compressor` `threshold_db`, `ratio`, `makeup_db`, `limiter` `ceiling_db`, `delay` `feedback_percent`, `mix_percent`, `reverb` `mix_percent`, `room_size_percent`, `damping_percent`, and `snes_echo` `feedback`, `volume_left`, `volume_right` (whole numbers). Times, `q`, filter kinds, EDL and FIR are not automatable. Point shapes are `step` (default), `linear` and `exponential`. `"shape": "linear"` ramps in the parameter's own unit (hertz for `frequency_hz`), which crowds a filter sweep into its top octave; use `"shape": "exponential"` on `frequency_hz` (and compressor `ratio`) for an even sweep in octaves. Exponential is refused on decibel, percent and register parameters and on volume and pan lanes (decibels are already logarithmic); use linear there. A low-pass or high-pass sweep is only audible once its cutoff passes the part's main frequencies: start below the fundamental (check the voice's bands with `measure_audio`). Before the first point the effect's own setting applies; with a loop, put a point at `loop_start` when the end value differs. Values outside the processor's range are refused at the point. To remove an automated effect, or change its settings so a band or its gain disappears, or reuse its id for another kind, clear or reset the lane (`points: []`) in the same request; `move_effect` keeps lanes. Lanes follow the same scope as the chain: voice lanes follow `scope.voices`, `range` and protections, bus and master lanes need `scope.buses: true`. `inspect_project` lists them under `mix.effect_automation` (summary) and `effect_automation` (events).

Check numbers, not impressions: read peaks in the rendered WAV or measure it with `measure_audio` (see [Measuring audio](#measuring-audio)). Do not tell the person that a mix sounds better; present both versions and let them listen and mark. Effects are not written to MIDI; the export report lists them under `mix` and `not_in_midi`.

## Placing audio clips

Use a clip when the piece needs recorded audio rather than notes, such as a supplied jingle, a voice sample, an ambience or a sound effect file to place under the music. A clip plays a range of a WAV on a voice's timeline, through that voice's mixer strip, effects, sends and output.

1. Retain the WAV with `load_instrument_source` and keep the returned `source` SHA-256. Do not convert it first: integer PCM 8–32-bit and float WAVs, 1–32 channels, up to 64 MiB, are read directly.
2. Give the audio its own voice (`add_voice`, with a `role` such as `ambience`) unless it belongs with a part, so its level, pan, effects and mute stay separate.
3. `edit_project` with `add_clip`: `id`, `voice`, `source`, `start` and `duration` in the timebase unit, and `channels`, which you must choose: `{"mode":"stereo"}` for a two-channel file, `{"mode":"channel","channel":0}` for mono (or one channel of a multichannel file), or `{"mode":"downmix"}` for the mean. Optional: `source_start`/`source_end` in source frames to trim, `gain_db`, and linear `fade_in_ms`/`fade_out_ms`. The result's `clips` entry gives `audio_seconds`, `span_seconds` and `duration_for_audio`, the duration that plays the whole range at the current tempo; use it unless you mean to cut the audio short. Shorter spans cut the audio; longer ones leave silence. `source_exhausted` is set only when that silence lasts at least one tick; `duration_for_audio` rounds up to whole ticks, so it leaves a few samples of silence (render placements report them as `silent_samples`) without setting it.
4. A clip may not cross `loop_start` or end after the playback end. For a loop, place clips entirely in the intro or entirely in the loop region. Clips in the loop region replay in every pass. Split a long sound into two clips at the loop start if it must continue.
5. Clip edits follow `scope.voices`, `scope.range` (the clip's span) and protections like notes. Unknown sources, non-WAV files and trims or channel modes the file cannot satisfy refuse the edit. Change a clip with `update_clip` (only the given fields) and remove it with `remove_clip`.
6. Render and read `clips.played`: each clip's `render_sample`, `samples_written`, channel mapping and `rate` (resampled to the render rate, with the ratio, when the file's rate differs). Measure the render as usual. Clips of muted or unsoloed voices are listed under `clips.not_heard`.

A voice that holds only clips is still a voice: `check_project` counts it against `max_voices`, and MIDI export assigns it a track and channel with no notes. Say so when you report a check against a device profile. The first clip (like any mixer data) switches the render to one synthesizer per voice, so the other voices can differ from the earlier render by a few least-significant bits; `compare_audio` then shows tiny differences outside the clips that are not musical changes.

MIDI cannot carry audio: the export report lists clips under `not_in_midi`, and the bundle and its `objects/` folder carry the clip records and source files. On imported MIDI, clips go on existing voices and never change the exported MIDI bytes.

## When the person has marked something

1. `read_feedback` with your last `next_cursor` as `after_sequence`. Each mark gives the revision, the loop pass, the musical position, and the notes sounding there. Marks have names such as `B` (`mark`); the person's text may refer to them by name.
2. A mark is an observation. Investigate around it yourself: the notes before and after, other voices, the loop return, controller values, the render tail. Use `inspect_project` events with a `range`, excerpts and single-voice renders. An event's `position` counts from the beat before it (`bar 2 beat 4 +466 ticks`); `nearest` names the following beat when that is nearer (`bar 3 beat 1 -14 ticks`), which is how to read a note that slightly anticipates a beat. If the mark is in loop pass 2, consider the loop boundary first.
3. Make one fix per hypothesis with `edit_project`, putting the feedback id in `request.feedback` and your hypothesis in `request.rationale`. Stay inside the original request's scope and protections; a mark never unlocks protected parts.
4. Re-check, render the same layout again, and run `compare_audio` between the earlier and new render to confirm the change is where you intended and nowhere else. `compare_revisions` lists the data that changed; its `voices` and `range` filters narrow notes, controls and clips (and, for `voices`, voice mixer items), never added or removed voices or the timeline, as its `filter_applies_to` says. Present the fix together with the earlier version (two items in `present_review`, for example `Revised` and `Previous version`). Do not mark feedback as resolved: only the person can say that.
5. If the person's remark came through conversation instead of the screen, store it verbatim with `record_feedback` before acting on it.

## Measuring audio

You cannot hear the renders, so measure them. `measure_audio` takes `audio`: either a stored render (`{"project": …, "render": …}`) or a workspace WAV (`{"path": …}`), optionally a `range` (`start_seconds`, `end_seconds`) and `per_channel`. `compare_audio` takes `reference` and `candidate` in the same form; `range` applies to both, and `reference_range` and `candidate_range` give each its own. A `render_instrument` audition is measured by the `wav.path` it returns (its `measure_with` is the input).

- Every candidate: check `clipped_samples` is 0 and `true_peak_dbtp` stays below about -1 dBTP, `silence` matches the intended start and tail, and `dc_offset` is near 0 (not for unipolar AY, Game Boy, NES, HuC6280 and WonderSwan chip renders, which carry DC; see the [PSG reference](references/psg.md#limits)).
- Loudness of candidates and alternatives: compare `integrated_lufs` (and `short_term_max_lufs` for short cues). Keep alternatives you present together within about 1 LU unless loudness is the point, so the person judges content rather than level. Sound effects shorter than 3 s have no short-term or range value; use `momentary_max_lufs`.
- Balance: the third-octave `spectrum` shows where energy sits; the band list is `spectrum.levels` (`spectrum.bands` only names the layout). Use each band's `share` to spot a voice that dominates low or high bands, or measure single-voice renders and compare their levels. Unipolar chip renders carry DC that inflates `below_first_band_share` and lowers `centroid_hz`; read `spectrum.dc_removed` (present when DC is at least 0.1 % of the power) for their balance. A band 50 dB or more below a much louder band an octave or more lower can be spread from notes that start or stop abruptly, not a separate sound.
- Pitch: add `"pitch": {}` to `measure_audio` to track the fundamental of a single line: per window `hz`, nearest MIDI `note` and `cents`, `notes` segments (consecutive windows on one note, with name and median cents) and a `summary`. Use it to confirm a transposition, a tuning (`a4_hz`) or that each written note sounds: render the voice alone (`render_preview` with `voices`) first, because on chords and mixes it reports the common period, which can be a note nobody plays. With `compare_audio`, `"pitch": {}` gives `median_difference_cents` (+300 for three semitones up), `different_note_windows` and the `largest_differences` with their times. Lower `min_hz` (default 50) for bass below G1; `windows: false` keeps only the summary and segments, and is needed past 2000 windows (20 s at the default 10 ms hop). A run shorter than `min_note_windows` (default 2) is not a note: single windows where the estimate passes through a neighbouring note at a transition are counted in `short_runs_omitted`. A listed note with `low_confidence` is suspect: `few_windows` (under three windows, often a transition) or `dc_dominated` (the window level is mostly a constant offset, as when a chip holds a level, so the estimate rests on a small ripple; it can also be a quiet waveform on a large offset). Do not report such a note as played without checking it in a channel solo or a longer render. For one channel of a chip render, render that channel alone with `render_psg` `channels` (`[{"chip": "sn76489", "solo": [2]}]`) and measure that WAV.
- Revisions: `compare_audio` with the previous render as `reference` reports loudness and band deltas, `largest_difference_windows` (input seconds) and residuals. `identical: true` means the same length and samples; `overlap_identical: true` means every shared sample is equal and only the length or offset differs (the summary gives the unmatched frames), as when one render has a longer tail. A deep `gain_matched_residual_db` with a non-zero `best_gain_db` means the change is mostly level. Set `max_offset_seconds` only when the two renders may start at different times; identical layouts need 0.
- Loop seam: render the whole piece with `loop_passes: 3`; `timing` gives `intro_seconds` (I) and `loop_seconds` (L). Compare the render with itself, `reference_range` pass 2 (`I + L` to `I + 2L`) against `candidate_range` pass 3 (`I + 2L` to `I + 3L`): both start with the previous pass's tail, so `identical` or `overlap_identical` means every repeat sounds the same. Differences confined to the first windows after the seam come from chip state or effects carried differently; report them. Pass 1 against pass 2 also shows how the intro's tail differs from the loop end's.

Report numbers as measurements of the reference preview, not as listening results or as the target console's loudness. `null` means silence or a signal too short for that window. A refused WAV names the byte offset and field that is malformed.

## Rendering stems, regions and formats

Use `render_batch` when the person or a later tool needs files rather than one listening render: every section, the stems of a mix, or a 24-bit or float master. One call takes `spans` (`full` with `loop_passes`, `excerpt`, `{"kind": "section", "id": …}` or `{"kind": "sections"}` for every section), `mixdown` (default true), optional `stems`, `stem_master`, `sample_rate` and `format` (`pcm16` default, `pcm24`, `float32`). Declare the regions first with `set_sections`; an unknown section id is refused with the existing ids.

- `stems: {"by": "voice"}` (optionally `voices`) gives one stem per heard voice; `{"by": "bus"}` one per unmuted bus plus `direct` for voices routed straight to the master; `{"by": "group", "groups": [{"id": "rhythm", "voices": [...]}]}` one per group, with each voice in at most one group.
- Stems follow the mixer, so they add up to the mixdown: bus and master compressors and limiters apply the whole mix's gain reduction to each stem. Read each span's `sum_check`: `sums_within_rounding: true` means the stems sum to the reference within float rounding (`max_abs_difference`), and `sums_to_mixdown` says whether that reference is the written mixdown file. `approximated_by` names a shared SNES echo the stems can only approximate; `covers_mix: false` lists `uncovered_voices` you left out. If the stems will be mastered elsewhere, or the master holds an echo, use `stem_master: "pre_master"`: stems then stop before the master volume and chain and sum to the master input, which is not written. `master_is_identity: false` (with `sums_to`) means the master volume or chain changed this render, so tell the person the stems sum to the unmastered mix, not to the delivered mixdown.
- Integer formats round and clamp each file on its own: summed 16-bit or 24-bit stem files differ from the mixdown file by a few codes even when the samples sum exactly. `sum_check.encoded` gives the measured `max_abs_difference_codes` and the `rounding_bound_codes` it stays within (about half a code per file, so 2 codes for three stems; `clamped_samples` must be 0). Prefer `float32` when someone will sum or process the stems. Check a float master with `measure_audio` for peaks above 0 dBFS before converting it elsewhere.
- A render entry with `silent: true` holds only zeros (a voice that does not play in that section); skip it when presenting or delivering.
- Section and excerpt renders keep the decay tail after their end (`timing.tail_seconds`, up to `max_tail_seconds`), so a file placed at its section's start runs into the next section by that much. When files will be butted together at the markers, pass `tail: "cut"`: every output then ends at the content end (`timing.tail: "cut"`, `tail_cut` in the record), and notes still sounding there stop abruptly. Say which you delivered.
- Every output is an ordinary render record (`region`, `part`, `format` say what it is). Present them with `present_review`, measure them, and deliver exactly them with `export_project` `render_audio: "listed"` and their `render_ids`. At most 64 renders per call; split larger sets.
- Delivered WAVs are named `renders/<render id>.wav` (import finds them by id). Pass `named_copies: true` to `export_project` when a person will open the folder: it also writes byte-identical copies as `mix/<span>.wav` and `stems/<span>/<part>.wav` (span: the section id, `full`, `full-<n>-passes` or `excerpt-<start>-<end>`; part: `voice-<id>`, `bus-<id>`, `group-<id>` or `direct`; `-only-<voices>` for a voice-filtered render, `-pre-master` for a pre-master stem, `-<render id>` when two would share a name). The result's `named_copies` and the report's `files` (`copy_of`) map each copy to its render. Otherwise the report's `files` entry for each WAV names its `region`, `part`, `format`, layout and revision; give the person that mapping (for example "render-6 is theme-a, melody stem") rather than bare file names.

## Delivering

`export_project` writes the MIDI, the re-editable project bundle, a report and the renders into a new folder. The report holds the bundle and MIDI readback checks, source preservation for imported projects, and what MIDI does not carry; it does not run constraint profiles, so report the `check_project` result of the profile you actually checked (the person's target, or your inline sanity profile, named as such). Tell the person where it is and what the report says MIDI cannot hold (loop markers, reference sound, project notes, the mixer, its effects, chip instruments and audio clips). Markers are written as MIDI meta events of their kind. Sections stay in the bundle; pass `sections_as_markers: true` when a DAW or driver tool should see each section's name as a marker at its start (not available for imported MIDI, whose markers are original bytes).

Diagnostic renders make a delivery large. `render_audio: "presented"` keeps only the WAVs shown to the person, `"listed"` with `render_ids` keeps exactly the renders you name (useful without a review screen), and `"none"` keeps none; `render_ids` with `presented` or `none` adds renders to that choice. By default every render's record and hash stay in the bundle. The delivery also carries everything the revisions refer to (clip sources, instrument and FM banks, sample plans) in the bundle's `references` and `objects/`, so another workspace can continue it. `referenced_data: "omit"` leaves those bytes out only when the receiver already has them. `import_project` verifies every object and names any reference it cannot find. To give someone only certain files (for example an engineer who needs three stems), export with `render_audio: "listed"`, those `render_ids` and `render_records: "delivered"`: the bundle then holds only those renders' records and imports with the default policy. It is refused while a presentation or feedback mark refers to a render you leave out; add that render or keep the default `render_records: "all"` (needed when the receiver continues the review history). With the default, a delivery made with `listed`, `presented` or `none` carries every record without its WAV, so the receiver's default import refuses with `missing_render_audio`. When you continue such a bundle:

- `missing_renders: "regenerate"` renders every missing WAV again (slow for many renders) and refuses if one does not match its recorded hash (a different renderer made it).
- `missing_renders: "without_audio"` with `regenerate_renders: [ids]` renders only the listed ones again and keeps the rest as records without audio (`renders_without_audio`). Renders shown in a presentation must be listed. `inspect_project` `reviews` shows each render's `audio` as `present` (with its `path`) or `absent`. `present_review`, `measure_audio`, `compare_audio` and `export_project` refuse a render without audio with `missing_render_audio`; restore it later with `render_batch` `{"project": …, "regenerate_renders": [ids]}` (no other settings), which checks the hash. An export that omits those renders keeps their records.

`regenerated_checks` says for each render whether its hash was checked or, for older bundles that recorded none, only its length (such renders cannot be kept without audio). Otherwise rebuild from the head composition and say so.

## Reusing an instrument from another song

For OPN FM patches, use the [FM-bank workflow](references/fm-banks.md): retain the
complete container, inspect a verified register layout, copy static parameters
between banks and export with exact readback. Keep channel/global playback
context distinct from the static patch. A whole bank record replacement affects
every song referring to it; it is not a one-track binding change. To hear a
patch, audition it with `render_fm_patch` and explicitly declared chip, clock,
pan, LFO and notes, as the reference describes. Present the result as
Tunemend's model, not original hardware audio.

Use the [instrument-bank workflow](references/instrument-banks.md) to retain a
sound source, discover modules and native tracks, and transplant exact BRR data
with its referenced pitch/envelope programs. Select unused destination slots and
inspect reference coverage before allocating missing tables. Preserve the
receiving arrangement unless the request changes it. Export native modules
without a MIDI prerequisite, or hand a compatible bank to score preparation.
Treat initial-header edits, later score changes, runtime activation and listening
as separate operations. Do not automatically normalize loudness.
Audition a slot or BRR sample plan with `render_instrument` (one S-DSP voice,
explicit pitch and envelope) as described in that reference; it is not the
driver's playback of a song.

### Playing a voice on a chip instrument

When the person should hear the arrangement with a retained chip sound rather
than the reference waveforms, and you have a saved BRR bank slot, a BRR sample
plan or an FM patch with known playback context:

1. Audition the sound alone first (`render_instrument` or `render_fm_patch`) to
   choose the root register or `a4_hz`, envelope or context, and level.
2. `edit_project` with `set_instrument` on the voice. `snes_brr` takes `bank`
   and `slot` or `plan`, a `pitch` mapping (`root`, `plan_root` or `fixed`),
   an `envelope`, `volume` and a `velocity` policy (`ignore` or
   `scale_volume`). `fm` takes `bank` and `index` or `registers`, the same
   `context` as `render_fm_patch`, `a4_hz`, and a `velocity` policy (`fixed`
   or `scale_carrier_tl`). Both take an optional `channel_controls`:
   `ignore` (default), `level` (the voice's `volume` and `expression` lanes
   become gain) or `level_and_pan` (also the `pan` lane). Nothing is inferred
   from program numbers. The edit
   follows `scope.voices` and protections and cannot be limited by
   `scope.range`; unknown banks or plans refuse it.
3. Render as usual. Read `instruments.voices` in the result: `chip_voices`
   (peak, `notes_per_chip_voice` and the allocation of the first notes), `conversion` (native rate and resampling) and
   `not_applied`. Pitch bend, program changes and controllers other than
   volume, expression and pan are never played on a chip instrument. For the
   voice's `volume`, `expression` and `pan` lanes, choose one way: set
   `channel_controls` to `level` or `level_and_pan` so the lanes shape the
   chip with the reference synthesizer's curves (`channel_controls` in the
   voice's report shows what was applied), or keep `ignore` and recreate the
   shape with the mixer (`set_mix` pan/volume and `set_mix_automation`).
   Never both, or the level drops twice. With `level`, a voice without a
   `volume` lane plays at the default CC7 100 (about -4 dB), and with
   `level_and_pan` the centre is -3 dB per side, as on the reference
   synthesizer. The MIDI keeps its lanes either way. If the part depends on
   bend, tell the person.
4. A chord with more notes than the chip's voices (8 S-DSP voices, 6 FM
   channels, 3 on YM2203, or a lower `max_voices`) refuses the render with
   `chip_voices_exceeded`. Thin the chord, split the part into voices, or
   raise `max_voices`. Each instrument voice has its own chip, so check the
   total against the target with `check_project` (`snes-dsp-voices` for the
   S-DSP budget), which counts release tails.
5. `clear_instrument` returns the voice to the reference synthesizer.

Present these renders as Tunemend's chip model with the declared settings, not
the game's driver or hardware: driver pitch tables, vibrato, volume programs
and echo writes are not reproduced. MIDI export keeps the notes and program
numbers; the report lists the instruments as not written to MIDI.

## Inspecting and auditioning PSG music

For Master System, Game Gear or Mega Drive PSG data, AY-3-8910/YM2149 PSG
data (MSX, ZX Spectrum 128, Amstrad CPC, Atari ST), Game Boy sound, NES/
Famicom APU sound, PC Engine (HuC6280) or WonderSwan sound in a VGM file, or register writes and notes for those chips,
follow the
[PSG reference](references/psg.md): retain the file, inspect its header and
every command with `inspect_vgm`, and render the modelled chips with
`render_psg` (with `channels` to hear or measure single channels). Tell the person which commands (such as Mega Drive FM, MSX
SCC or NES expansion audio) were not rendered, and present the result as
Tunemend's model, not hardware audio. To derive a changed copy (transpose,
mute, retarget the clock, variant or region) or to move SN76489 music onto an
AY-3-8910/YM2149, use `transform_vgm` as the reference describes: it never
changes the original, and you accept each listed loss explicitly.

## Mapping a source or revision to a driver score

Use `prepare_target` for a compatible Compile score encoder with consumer-supplied
sample/template assets and explicit track mappings. It accepts a loaded source
without an editable projection, or a project revision including newly created
music. The [score reference](references/score-target.md) owns required assets,
mapping choices, readback checks and the inspect/revise/export procedure.

Read the change inventory and page `inspect_target` before selecting changes for
`export_target`. Do not accept an omission that conflicts with the request.
Original-byte preservation, mapped score correctness and target playback are
separate claims. No human approval is required solely because a mapping has an
explicit selection step.

## Handing off to a sound chip target

Only when the person names a target and its consumer project supplies the driver limits and patches:

1. Check the composition against the constraint profile supplied by the consumer project first: exact voice count, one note per voice, frame grid, loop support.
2. Call `convert_for_target` with the path of the Retro Music Converter executable (`retro-music-converter`), an explicit mapping of every voice to chip, patch and pan, the consumer's capability profile, and a new folder under `out/` or `work/`. Patch data comes from the consumer project; never invent patches for a real game.
3. The current frame adapter blocks source-backed MIDI and mappings that report information loss before invoking the converter. Report this as an unsupported conversion, not a completed delivery. For a conversion that does run, report its `capability_status` and verification scope. Encoding into the game and playing it there is the consumer project's job; do not claim it works in the game until that project has played it.

## Practical notes

- Effect positions are integer microseconds. A 60 Hz frame is not a whole number of microseconds, so place effect events on millisecond values (for example multiples of 50 ms, three frames) or export with a finer `timing`; export tells you which point does not fit.
- After a blocked call, change the request and use a new `job_id`; the blocked outcome stays recorded under the old one.
- `schema_mismatch` means the arguments do not fit the tool's input schema (an unknown or misspelled field, a missing one, a wrong type); the message names it. Re-read that tool's schema rather than changing values. `invalid_range` and `value_out_of_range` are about values the schema accepts.
- A missing or non-string job ID reports `schema_mismatch` at `/job_id`, or `/request/job_id` for `edit_project`.
- Every successful edit becomes the head, including one made with `branch: true` from an older revision. After a diagnostic branch, call `set_head` with the revision you mean to continue or deliver before more edits or `export_project`; the diagnostic revisions stay in history.

## Honesty rules

- If you cannot hear audio, say so; base fixes on the structure and the checks and describe them as such. Never claim you listened.
- Passing a profile is not proof that a real console or driver plays it; say which limits were checked and which were not.
- Never directly edit `.tunemend/` or legacy `.retro-midi/` stores or bypass failed preservation checks. If a reusable external tool better serves the request, use it with explicit provenance and readback; do not claim that operation as MCP support. A blocked result identifies a capability or request conflict, not permission to weaken the person's requirements.

## Converting sample representations

For sample reuse across chip families, follow [sample conversion](references/sample-conversion.md).
Retain the source, select an explicit rate and format, prepare and inspect a plan,
then export only acceptable transformations. Do not describe retained originals
as proof that quantized target audio is lossless. PlayStation SPU ADPCM
(`psx_adpcm`) and VAG files are their own variant with loop flags in the data:
read the reported ending before choosing a loop policy, and never work around a
refusal of an undocumented or reserved header value by guessing. A VAG header
has no root note: a WAV `smpl` root or declared `root` stays in the plan report
but not in the file, so declare `root` again when converting a VAG onward. The
BRR and PSX encoders use filter 0 on the first block and on the loop-start
block, so the largest errors usually sit at the attack and the loop re-entry;
read where `sample_values` locates them. `inspect_instrument_source` shows a
WAV's rate, channels, frames and `smpl` loops before you prepare it. A loop
alignment refusal lists the nearest fitting loop starts and lengths, starts
that also make the loop a whole number of periods, and target rates that work;
pick one of those, not a guess. A preserved loop's
`report_guide.loop_diagnostics` measures its seam: a fractional
`periods_in_loop` (phase jump), a waveform break, a level change or a sustain
off the root are listed in `flags`; prepare a whole-period start it suggests
before you assign the instrument. `inspect_sample` returns a summary;
`detail: "full"` gives the stored report.

## Keeping external runtime evidence

When an emulator capture or another external observation informs your work, keep
it with [observation evidence](references/observations.md): attach the folder,
check identities, accept only limitations you can state, and associate it with
the items you compared. It is evidence, not proof of in-game sound.
