# Review and delivery

## Investigate feedback

Read new marks with `read_feedback`, continuing from the last cursor. Inspect the marked revision, loop pass, surrounding notes and other voices. A mark near a repeated section may concern the loop return rather than the note under the cursor.

Store conversational remarks verbatim with `record_feedback`. Attach the feedback ID and your hypothesis to the edit. Preserve the original scope and protections. Compare revisions to check what changed, and render the same layout before and after for an audio comparison.

Present previous and revised candidates together when that helps the person judge the fix. Marks remain observations until the person confirms the result. After reconnecting, retry the original `present_review` request with the same job ID and arguments to recover a live URL.

## Measure the question you are asking

Check candidate clipping, peak headroom, intended silence and tails with `measure_audio`. Choose loudness for the use case; avoid making an alternative seem better merely by making it louder. A null measurement can mean silence or too little audio for its window, not a failed render.

Use spectra and isolated voices to investigate balance. Unipolar chip models can carry DC; consult `spectrum.dc_removed` when present rather than treating that energy as musical bass. The [chip reference](psg.md#limits) describes those models.

For pitch, render a single line or chip channel before measuring. A chord's common period can be reported as a note nobody plays. Treat low-confidence estimates and short transition runs as uncertain; verify them in a longer isolated passage.

Use `compare_audio` with identical layouts and zero alignment offset unless the starts actually differ. `identical` checks length and samples; `overlap_identical` only covers shared samples. A low gain-matched residual with a nonzero gain difference indicates a change mainly in level.

For a loop seam, render three passes. From the returned timing, let I be intro seconds and L loop seconds. Compare pass 2 (`I + L` to `I + 2L`) with pass 3 (`I + 2L` to `I + 3L`) in the same render. Both include the previous pass's tail. Report remaining differences and where they occur; equality establishes repeatability, not musical quality. Comparing passes 1 and 2 also reveals the intro-to-loop transition.

Measurements describe the rendered model. Say whether you actually listened, and keep target playback claims tied to runtime evidence.

## Stems and regions

Use `render_batch` for sections, stems and delivery formats. Read `sum_check`: coverage, rounding and approximated processing determine whether stems reconstruct the mix. Pre-master stems sum before master processing, so they need not match the delivered master.

Prefer float stems when they will be summed or processed. Integer files round independently; inspect encoded rounding and clipping results. Skip silent stems in the handoff unless they serve an explicit layout requirement.

Section renders normally include decay tails. Choose `tail: "cut"` for files that must end exactly at a boundary, and disclose any abrupt cutoff. Use named copies for a person opening the folder; retain the report's mapping to render IDs.

## Export and resume

`export_project` delivers MIDI, an editable bundle, a report and selected audio. Inspect its MIDI/bundle readback results and `not_in_midi` information. Report the profile actually checked separately: export does not run constraint checks.

Choose the audio and record policy together:

- For a self-contained selection, use `render_audio: "listed"`, the required `render_ids` and `render_records: "delivered"`. Include every render referenced by a presentation or feedback mark.
- For complete review history, keep all render records. Omitting their WAVs then requires an explicit missing-audio policy on import.
- Keep referenced source data and objects unless the receiver already has them. A bundle depends on those bytes for later edits.

For missing WAVs, `missing_renders: "regenerate"` restores all and checks recorded hashes. Alternatively use `missing_renders: "without_audio"` with selected `regenerate_renders`; presented renders must be included. Later `render_batch` regeneration restores a recorded render without changing its settings. A hash mismatch is a failed restoration, not a reason to silently replace history.

Inspect `regenerated_checks`: older records without a hash can only receive a length check. If you instead render anew from the composition, describe it as a new render.

Give the person the usable files and a short account of changes and remaining limits. Keep diagnostic renders out of the delivery unless needed for continued review.
