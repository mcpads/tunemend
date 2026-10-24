# External runtime observations

Use this when an emulator or another external tool has produced evidence about a
source, bank, sample plan, score plan or revision, and you want that evidence
kept with the work. Tunemend retains and associates the evidence. It does not
launch or control emulators, and attaching an observation never proves that a
candidate sounds the same in the game.

## Attach

`attach_observation` takes a published folder relative to the workspace. Every
regular file in it is retained exactly. Links, special files, more than 256 files
or more than 256 MiB are refused.

- For an emucap `record_audio` folder (`manifest.json` and `audio.wav`), use
  `producer_format: "emucap_audio"`. The receipt is derived from the manifest.
  The WAV header, size and hashes must agree with it.
- For anything else, pass `receipt` with `producer` (`name`, `system`, optional
  `backend`, `build`), `content` identity keys, optional `capability.revision`,
  optional `clock` (`domain`, `start`, `end`), `completion.status` (`complete`,
  `interrupted`, `truncated` or `failed`, optional `dropped`), and `members`
  listing each described file's `path`, `role` and `sha256`. Copy identities
  from the producer's own outputs; never invent them. Extra fields are kept.

Pass the identities you expect in `expected`: the `system`, `content` keys, the
current `producer_build` and `capability_revision`. Content keys must be the
receipt's own keys and values compare exactly, including letter case. An emucap
audio receipt carries emucap's `get_rom_info` content: `sha1` as uppercase hex,
plus `name` and `path`; so pass `{"sha1": "<UPPERCASE SHA-1>"}`. An expected key
the receipt lacks is not compared and leaves `content_unverified`; the refusal
message lists the receipt's keys. A
different system or content value is refused. Name `subjects` for the Tunemend
items you compared or diagnosed, with a short `relation`.

The first call usually returns the limitation kinds it found: for example
`incomplete_capture`, `capability_unrecorded`, `stale_producer_build`,
`content_unverified`, `clock_unrecorded` or `undescribed_members`. Read them. If
the evidence is still useful within that scope, retry with a new `job_id` and
exactly those kinds in `accepted_limitations`. Otherwise obtain a better
observation. An emucap audio manifest carries no capability revision; record the
live capability revision you used in your report and accept
`capability_unrecorded` only if that is true.

## Inspect and deliver

`inspect_observation` with `view: "observations"` lists saved records, and
`subject` filters by an associated ID. `summary` shows the receipt, checks,
limitations and subjects. `members` and `bytes` page the exact retained files.
Records verify after restart without the original folder. `export_observation`
writes `observation.json` and every file under `members/` to a new folder.

## What it does and does not establish

A completed SNES DSP capture is the emulator's mixed output for that window and
initial state. It is not a sample bank, a register-write trace or the person's
listening judgment. A memory snapshot is state at one point, not a full playback
history. Report the observation scope and its accepted limitations alongside any
claim you make from it. Original loading never waits for an observation.
