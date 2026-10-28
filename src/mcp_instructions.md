# Tunemend

Make and revise music for the person; they listen and point out problems.

## Start

Call get_capabilities, then inspect_project view=projects. Resume matching work. Request capability sections as needed. Specialist work uses midi, project, audio, feedback, sample, sound, target or observation. Call operation=describe for its list, then operation=describe with select=<operation> for one schema; execute with operation=<operation> and arguments={...}. Operation names below refer to these groups. Work persists in the launch workspace (or --workspace).

## Work

- Create a brief separating requirements from assumptions. Edit through edit_project with the intended scope and protections. Every edit, including a branch, becomes the head; use set_head to return to a chosen revision.
- Check the intended constraints, render, and measure. Use listening when the host supports it; never claim to have heard audio from measurements alone.
- present_review returns the listening URL. Read feedback against the original render, investigate, revise, compare, and present again. Marks are observations, not approval or permission to release protections.
- Deliver projects with export_project. For stems or regions, use render_batch first. To include only chosen renders with default import, use render_audio=listed and render_records=delivered; retain any renders referenced by review history. The bundle carries what MIDI cannot.

## Original data

Load before interpreting: load_midi for MIDI; load_instrument_source or the bank loaders for native data. Inspect, prepare or edit, verify, then export. Accept only reported changes compatible with the request. Unsupported projection or rendering does not invalidate exact source retention. Chip renders model sound, not game-driver or hardware playback.

## Calls and recovery

- Use a fresh job_id for each change (request.job_id for edit_project). Retry the same call with the same ID; corrected input needs a new ID.
- Read status and diagnostics. For schema_mismatch, consult the tool schema; otherwise follow the reported location and recovery. Never weaken requirements to bypass a refusal.
- Pass next_cursor as offset, except inspect_project uses cursor and read_feedback uses after_sequence.
- Continue deliveries with import_project. If WAVs were omitted, choose missing_renders explicitly; regenerate checks stored hashes, without_audio retains records.
- Keep the MCP connection open while the person reviews. After reconnecting, retry present_review with its original job_id and arguments to recover the same presentation's live URL.
