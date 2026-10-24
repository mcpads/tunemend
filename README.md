# Tunemend

Make music with an AI agent. Listen, point out what sounds wrong, and let the agent revise it.

Tunemend provides MIDI composition and arrangement tools for looping game music and short tonal sound effects. The agent handles notes, voices, timing, constraints, and exports. You describe the scene and listen in a small browser screen; no DAW or piano-roll editing is required.

## Get started with your agent

Give your agent this repository and ask:

> Install Tunemend from https://github.com/mcpads/tunemend. Follow `skills/tunemend/references/install.md`, connect it to my MCP client, and verify that it works. Then help me make a short looping theme.

Your agent handles the build, installation and connection checks. You do not need to run terminal commands or set up a DAW. This initial publication provides source, not prebuilt release binaries; the current installation checks cover macOS.

Example requests:

- “Make a quiet village loop with a clear melody and a soft bass line.”
- “Soften this MIDI’s accompaniment and keep the melody unchanged.”
- “The accompaniment jumps out at mark B. Smooth it out and let me compare.”
- “Make a short rising pickup sound with a gentler ending.”

The agent creates or imports a project, checks it, renders a preview, and presents a listening URL. You can mark moments or ranges, add an optional comment, and send several marks together. Return to the conversation and ask the agent to review your feedback when the host needs a new turn.

## What it does

- Create music with tempo and meter, or effects timed in microseconds.
- Load original MIDI independently of editing or target mapping; inspect every event and export exact bytes.
- Discover embedded MIDI streams in a container while preserving the entire original file and each stream’s byte range.
- Optionally project a loaded source into music editing, protect material, and compare revisions.
- Preserve original MIDI bytes on unchanged export, and retain unrelated events when editing. Verify delivery with `compare_midi`.
- Check explicit pitch, polyphony, timing, and resource constraints, including a built-in SNES S-DSP voice budget that counts chip-instrument release tails.
- Render full pieces, loop passes, excerpts, or individual voices with a built-in reference sound source.
- Name regions (sections) and markers, and render deliverables in one call: every section, stems per voice, bus or voice group that sum to the mixdown through the mixer (or stop before the master), as 16-bit, 24-bit or 32-bit float WAV with a reported sum check.
- Mix voices with revisioned volume, pan, mute, solo, buses, sends, master gain and volume/pan automation, applied to every preview render.
- Measure renders and WAV files natively (peak, true peak, BS.1770 loudness, loudness range, clipping, silence, band levels and the pitch of a single line) and compare revisions sample by sample.
- Process voice, bus and master chains with deterministic offline effects: parametric EQ, compressor with sidechain, sample-peak limiter, tempo-synced delay, reverb and an SNES-style echo, with their parameters automated over time.
- Keep feedback tied to the exact version and playback position you heard.
- Export MIDI, an editable project bundle with the sound sources, banks and sample plans its revisions use, renders, and reports of information MIDI cannot carry.
- Prepare optional driver scores with explicit mappings, complete event accounting and independent encoded readback.
- Discover native sound modules and reuse BRR instruments across reviewed driver variants, preserving samples and exact pitch/envelope programs.
- Audition BRR instruments from native banks or sample plans on a built-in SNES S-DSP voice model (BRR loops, Gaussian interpolation, ADSR/GAIN), with every register reported.
- Load OPN FM register banks with verified layouts and transfer static patches across storage orders, preserving all unselected container bits.
- Audition an OPN patch through a built-in YM2203/YM2608/YM2612 FM model with a declared clock, LFO, pan and notes; the report lists every register write.
- Inspect VGM files (1.00-1.71) command by command without loss and render their SN76489-family PSG part (Master System, Game Gear, Mega Drive PSG) or explicit PSG writes through a built-in model; the report lists every write applied and every command skipped.
- Play any voice on a retained BRR sample or OPN patch through those chip models in every preview render, with explicit pitch, envelope or context and velocity policy, deterministic voice allocation and native-rate rendering resampled into the mixer.
- Place audio clips (ranges of retained WAV files) on voices' timelines with explicit channel mapping, deterministic resampling, gain and linear fades; they loop, crop in excerpts and run through the voice's mixer strip, and project bundles carry their sources.
- Render AY-3-8910/YM2149 PSG (MSX, ZX Spectrum 128, Amstrad CPC, Atari ST) and Game Boy DMG APU streams from VGM files, or explicit register writes and notes, through built-in models; several chips declared in one file are mixed under a reported policy.
- Render NES/Famicom APU streams (2A03 NTSC or 2A07 PAL: pulse, triangle, noise and DMC samples from VGM RAM-write blocks) from VGM files, or explicit register writes and notes, through a built-in model written from public hardware documentation; expansion audio is reported as not rendered.
- Render PC Engine/TurboGrafx-16 HuC6280 PSG streams (six wavetable channels with noise, direct D/A and the LFO) from VGM files, or explicit register writes and wavetable notes, through a built-in model written from Hudson's PSG manual and public notes.
- Render WonderSwan sound streams (four wavetable channels from internal RAM with channel 2 voice, channel 3 sweep and channel 4 noise; the headphone mix) from VGM files, or explicit port and RAM writes and wavetable notes, through a built-in model written from public hardware documentation.
- Derive new VGM files from retained ones: same-family copies for SN76489, AY/YM2149, Game Boy and NES streams (transposition, channel mutes, clock or variant changes) and an explicit SN76489-to-AY mapping, with every change listed, unrepresentable changes refused until accepted, and a register-state readback.
- Retain external runtime observations, such as emulator audio captures, with checked receipts and link them to the sources and candidates they informed.

Projects persist locally through restarts. No account or model service is built into Tunemend; the connected agent chooses the musical edits.

## CLI

The `tunemend` executable includes the CLI, MCP server, and listening screen. From a source checkout with Rust installed:

```sh
cargo run --locked -- tools
cargo run --locked -- schema create_project
cargo run --locked -- call get_capabilities
```

After [installation](skills/tunemend/references/install.md):

| Command | Purpose |
| --- | --- |
| `tunemend tools` | List tools and descriptions |
| `tunemend schema <tool\|bundle>` | Print a tool input schema or project bundle schema |
| `tunemend call <tool> [--input FILE]` | Run a tool with JSON input; reads stdin when no file is supplied |
| `tunemend mcp` | Serve tools over MCP stdio |
| `tunemend review [--port N]` | Serve the workspace's listening screen |

Every command accepts `--workspace DIR`; the default is the launch directory. `call` exits with 0 on success, 2 when blocked, and 1 on error. Discovery calls accept empty input.

New projects are stored in `.tunemend/`. An existing `.retro-midi/` store is used in place when `.tunemend/` is absent. If both directories exist, `.tunemend/` takes priority; stores are never merged automatically. These directories contain saved work, not disposable caches. Use `export_project` and `import_project` to move work between workspaces.

## Scope and limits

The [FM-bank workflow](skills/tunemend/references/fm-banks.md) retains a complete
original container and exposes raw records plus decoded OPN parameters. Static
patch replacement preserves opaque metadata, reserved bits and other records.
It needs a verified record layout; automatic bank discovery, native song-binding
edits and driver playback are separate. `render_fm_patch` auditions a patch
with explicitly declared chip, clock and channel/global context; SSG-EG, DAC
quantization and channel-3 special mode are not modelled. Equal register
parameters do not establish equal audio across chips or drivers.

Source loading supports SMF types 0, 1 and 2, PPQ and SMPTE timing, RIFF RMID, every channel message, SysEx, escape events and opaque metadata. `load_midi` retains the exact original bytes without requiring a note model or a target mapping. It can also discover structurally valid MIDI streams inside an opaque container; the complete parent, child ranges and duplicate occurrences remain available. It does not infer an archive’s index semantics.

Use `inspect_midi` to list sources, inspect headers and tracks, page through every original event, or read raw byte ranges. Same-tick events are never merged in this view. `export_midi_source` writes and verifies byte-identical output, including container data. Loading succeeds even when the optional music editor cannot represent the source.

`import_midi` optionally creates an editable project from a loaded source ID or a file. Its current projection uses type 0/1 PPQ, paired notes and separate channel controls. Modulation, reverb and chorus are editable alongside program, volume, pan, expression and pitch bend. Ordered or unmodeled information stays in the source. Imported voice/channel identities, markers, loop/end boundaries and RPN ranges currently remain fixed in this editor; these are editing limits, not loading limits.

Every source-backed edit is written and read back before it becomes a revision. Unchanged project export is byte-identical; edits preserve other event payloads, positions, tracks and relative order. File encoding may change. Unknown chunks, extended metadata payloads and RIFF data survive supported edits too. `compare_midi` reports decoded event and complete metadata differences, container equality and byte identity. Intentional edits should differ. Original bytes travel in the portable project bundle.

Previews use generated reference waveforms played by Tunemend's own SoundFont synthesizer, with no third-party synthesis library, except for voices assigned a chip instrument, which play on the built-in S-DSP or OPN model, and audio clips, which play their retained WAV sources. Neither reproduces a specific game driver. Passing a constraint profile does not prove in-game playback or musical quality. Sound effects in a project cover tonal notes and supported controls; PSG noise renders only through `render_psg`, not as a project voice. Audio transcription and a human note editor are outside the current scope.

The optional `prepare_target`, `inspect_target` and `export_target` workflow
supports the Compile score-module API. Supply a compatible external encoder,
score template and matching sample bank; Python 3 with its standard library is
required for this backend. Choose channel/chord placement, instrument/envelope,
drums, timing, loops and delayed copies. Inspect every original event's mapping
or omission before selecting changes for export. The tool checks encoded score
readback and ARAM bounds, preserves original MIDI exactly and records input/tool
identities. Controllers, velocities and other expressions not represented by
the driver are reported changes; this is not lossless target sound. See the
[score reference](skills/tunemend/references/score-target.md). No game data or
external encoder source is bundled.

The [instrument-bank workflow](skills/tunemend/references/instrument-banks.md)
retains original sound containers, discovers Compile LoROM modules and inspects
native tracks. It copies BRR bytes with their exact pitch/envelope programs,
remapping indices across reviewed driver variants. Missing tables can be added
in explicitly selected unused entries, with memory and reference checks. Native
initial bindings can be edited while preserving the receiving score and mix.
Export delivers matching modules without a MIDI or encoder prerequisite. Runtime
activation, ROM insertion and listening are separate; unknown drivers require a
verified adapter. Mixed-recording separation and automatic loudness normalization
are not provided.

`render_instrument` plays one bank slot or BRR sample plan on a single S-DSP
voice at the nominal 32000 Hz clock, from public hardware documentation. Echo,
noise, pitch modulation and the driver's pitch, vibrato and volume programs are
not modelled; an audition is not the game's playback of a song.

Sample conversion plans can retain original sources, decode native BRR slots and
RIFF/WAVE files (integer or float, explicit channel or downmix, optional `smpl`
loop and root), convert mono BRR/PCM and PlayStation SPU ADPCM (raw or VAG) representations with the built-in codec,
resample deterministically, keep or bake loops (with measured loop seams and
concrete fixes for unaligned loops), record a root note with SNES
pitch register values, and export source/target WAVs with measured differences. See the [sample workflow](skills/tunemend/references/sample-conversion.md).
Finite sample conversion does not establish whole-song or in-game compatibility.

Game insertion and runtime activation are owned by the consuming project.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests use original fixtures and Python 3, and require no game assets, external converter, or DAW. The listening page, server instructions, and reference sound generator are embedded in the executable.

## License

[MIT](LICENSE). Dependencies retain their own licenses.
