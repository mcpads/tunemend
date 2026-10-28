# Review and delivery

## Investigate feedback

Read new marks with `read_feedback`, continuing from the last cursor. Inspect the marked revision, loop pass, surrounding notes and other voices. A mark near a repeated section may concern the loop return rather than the note under the cursor.

Marks on source renders (`read_feedback` with `source`) give the source tick, seconds and the retained note-on events held there on each MIDI port receiver, by `inspect_midi` event ID, with whether the key or a pedal holds them. They name no revision or voice. If you later edit a project imported from that source, record how you related the mark to it through the source tick; that link is your interpretation. Exporting that project carries the source reviews and their WAVs, and `import_project` restores them with the source in a fresh workspace.

When the person names a time in words ("at 0:05"), convert it to a sample of the render they heard and attach it as a mark. If `record_feedback` refuses with `mark_outside_render`, the time lies beyond that render: never move, clamp or shorten the mark to make it fit. Record their words verbatim without the mark, tell them the render's length (`length_samples` / `sample_rate`, and where the music ends before the tail), and ask them to mark the spot again on the listening screen or name a time within it. A spoken range that starts inside the render but ends after it is the same case: keep the words, report the length, and ask whether they meant the passage up to the end. You may say which events lie near the end as candidates, labelled as your reading, not as their mark.

Store conversational remarks verbatim with `record_feedback`. Attach the feedback ID and your hypothesis to the edit. Preserve the original scope and protections. Compare revisions to check what changed, and render the same layout before and after for an audio comparison.

Present previous and revised candidates together when that helps the person judge the fix. Marks remain observations until the person confirms the result. After reconnecting, retry the original `present_review` request with the same job ID and arguments to recover a live URL. In CLI mode, a null URL means the review server is not running: start `tunemend review` in the same workspace, keep it alive, then retry that request for the listening link.

## Measure the question you are asking

Project/source previews and batch renders report `levels`; a `warnings` entry (`integer_clipping`, or `samples_over_full_scale` for float) means samples exceed full scale. Rendering never automatically rescales: add headroom with a fader, master volume or master limiter in a new revision, not by changing MIDI. Check candidate clipping, peak headroom, intended silence and tails with `measure_audio`. Choose loudness for the use case; avoid making an alternative seem better merely by making it louder. A null measurement can mean silence or too little audio for its window, not a failed render.

Use spectra and isolated voices to investigate balance. Reference waveform cycles are zero-mean, but finite renders can have a nonzero mean from partial cycles, envelopes or processing. Investigate persistent DC across longer ranges and isolated voices; also check chip instruments, audio clips and older renders. Unipolar chip models can carry DC; consult `spectrum.dc_removed` when present rather than treating that energy as musical bass. The [chip reference](psg.md#limits) describes those models.

For pitch, render a single line or chip channel before measuring. A chord's common period can be reported as a note nobody plays. Treat low-confidence estimates and short transition runs as uncertain; verify them in a longer isolated passage.

Use `compare_audio` with identical layouts and zero alignment offset unless the starts actually differ. To check one recorded channel against a mono export, set zero-based `channel` on that audio input; omit it to keep all channels. `identical` checks length and samples; `overlap_identical` only covers shared samples. A low gain-matched residual with a nonzero gain difference indicates a change mainly in level.

For a loop seam, render three passes. From the returned timing, let I be intro seconds and L loop seconds. Compare pass 2 (`I + L` to `I + 2L`) with pass 3 (`I + 2L` to `I + 3L`) in the same render. Both include the previous pass's tail. Report remaining differences and where they occur; equality establishes repeatability, not musical quality. Comparing passes 1 and 2 also reveals the intro-to-loop transition.

Measurements describe the rendered model. Say whether you actually listened, and keep target playback claims tied to runtime evidence.

## Stems and regions

Use `render_batch` for sections, stems and delivery formats. Read `sum_check`: coverage, rounding and approximated processing determine whether stems reconstruct the mix. Pre-master stems sum before master processing, so they need not match the delivered master.

Prefer float stems when they will be summed or processed. Integer files round independently; inspect encoded rounding and clipping results. Skip silent stems in the handoff unless they serve an explicit layout requirement.

Authored-note excerpts retrigger notes at their start; they are not exact crops of a full mix. Source-backed MIDI uses preroll. For continuity checks, measure ranges within one full render.

Section renders normally include decay tails. Choose `tail: "cut"` for files that must end exactly at a boundary, and disclose any abrupt cutoff. Use named copies for a person opening the folder; retain the report's mapping to render IDs.

## Export and resume

`export_project` delivers MIDI, an editable bundle, a report and selected audio. Inspect its MIDI/bundle readback results and `not_in_midi` information. Report the profile actually checked separately: export does not run constraint checks.

Choose the audio and record policy together:

- For a self-contained selection, use `render_audio: "listed"`, the required `render_ids` and `render_records: "delivered"`. Include every render referenced by a presentation or feedback mark.
- For complete review history, keep all render records. Omitting their WAVs then requires an explicit missing-audio policy on import.
- Keep referenced source data and objects unless the receiver already has them. A bundle depends on those bytes for later edits.

For missing WAVs, `missing_renders: "regenerate"` restores all and checks recorded hashes. Alternatively use `missing_renders: "without_audio"` with selected `regenerate_renders`; presented renders must be included. Later `render_batch` regeneration restores a recorded render without changing its settings. A hash mismatch is a failed restoration, not a reason to silently replace history.

A project imported from original MIDI keeps those bytes in its bundle. After `import_project`, its result's `midi_source.source` is the same source ID as when it was first loaded, now loaded in this workspace: `export_midi_source` returns the exact original, and `inspect_midi` and source `render_preview` work without calling `load_midi` again.

Inspect `regenerated_checks`: older records without a hash can only receive a length check. If you instead render anew from the composition, describe it as a new render.

Give the person the usable files and a short account of changes and remaining limits. Keep diagnostic renders out of the delivery unless needed for continued review.
