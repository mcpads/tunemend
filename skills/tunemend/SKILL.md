---
name: tunemend
description: Create and revise retro game BGM and sound effects through listening feedback. Use for MIDI arrangement, chip instruments, sample reuse and music delivery with Tunemend. 고전게임 BGM·효과음 제작, MIDI 편곡, 악기·샘플 이전과 청취 피드백 수정에 사용한다.
---

# Tunemend

Make music the person can use: they describe the scene, listen, and tell you what to change. Handle the notes, instruments and tools yourself.

## Working principles

- Work in musical phrases. Choose rhythm, register and instrumentation for the scene; keep the person's requirements separate from your assumptions.
- Preserve what matters. Retain supplied originals, protect parts that must stay, and keep revisions so alternatives remain recoverable.
- Investigate feedback. A mark points to a listening problem; it neither diagnoses the cause nor authorizes changes to protected parts.
- Match evidence to the claim. Measurements establish technical facts. Listening establishes what was heard. A reference render or voice-budget check does not establish playback in a game.

## Start or resume

Check `get_capabilities` and `inspect_project`. Resume the relevant project or import its delivered bundle. If MCP is missing, follow [installation](references/install.md).

Request only the capability sections needed for the task; use live tool schemas for arguments. The CLI exposes the same operations through `tunemend call` and `tunemend schema`.

For a supplied MIDI, retain it with `load_midi` before importing an editable projection. For a new piece, record the brief and choose the timebase: musical ticks for BGM, microseconds for an unmetered effect.

## Make, hear, revise

1. Build a complete phrase or cue. Set the playback boundary and loop deliberately. Scope edits to the requested parts and apply protections to anything that must remain unchanged.
2. Check the intended constraints. Use the consumer's profile for a named target; otherwise describe your own limits as an arrangement sanity check.
3. Render and measure the candidate. For looping BGM, include the return to the start. Listen if the host supports it; otherwise state that your assessment uses structure and measurements.
4. Present the review URL and its guidance. Keep the MCP connection alive while the person listens. They can mark the audio, send the marks, and ask you to review them in conversation.
5. Read feedback, investigate a cause, and revise within scope. Store conversational feedback verbatim. Compare the previous and revised versions and present the result; only the person can confirm that their concern is resolved.
6. Deliver useful audio, MIDI and an editable bundle. Check the export report and explain any requested behavior that the delivered format cannot carry.

Before measuring loops, revising marked audio or exporting a bundle, read [review and delivery](references/review-and-delivery.md). For MIDI edits, mixing or chip instruments, read [arrangement](references/arrangement.md).

## Choose the reference for the job

Read only the reference needed for the next decision.

| Task | Reference |
| --- | --- |
| Install or connect an agent client | [Installation](references/install.md) |
| Preserve MIDI, arrange parts, mix or bind a chip instrument | [Arrangement](references/arrangement.md) |
| Diagnose feedback, check a loop, deliver stems or resume a bundle | [Review and delivery](references/review-and-delivery.md) |
| Reuse native samples and instrument programs | [Instrument banks](references/instrument-banks.md) |
| Inspect, transplant or audition OPN patches | [FM banks](references/fm-banks.md) |
| Inspect, audition or transform VGM chip music | [PSG and chip music](references/psg.md) |
| Convert WAV, BRR or PSX sample representations | [Sample conversion](references/sample-conversion.md) |
| Map a source or revision to a consumer's driver | [Score and target handoff](references/score-target.md) |
| Associate an emulator capture with the work | [Observation evidence](references/observations.md) |

## Keep work recoverable

Use a fresh `job_id` for each changing request; reuse it only for an exact retry. After a refusal, fix the request and use a new ID. Read a schema error's field path before retrying.

Every successful edit becomes the head, including a branch from an older revision. Restore the intended head after diagnostic edits before continuing or exporting.

Keep edits behind the tools; do not modify the project store directly. A blocked operation is a capability or request conflict. Preserve the person's requirements and originals when choosing another capable tool, and report which tool performed the work.
