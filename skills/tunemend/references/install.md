# Installing Tunemend

Handle setup and verification yourself. Ask the person only for access or information you cannot obtain. Preserve existing MCP entries and saved work.

1. Inspect the host, MCP client and any existing Tunemend installation. Get the source from `https://github.com/mcpads/tunemend` and record the checked-out commit. This initial publication has no prebuilt release binaries. Install Rust and the host C build tools if missing (Xcode Command Line Tools on macOS, for bundled SQLite). Current installation evidence covers macOS; verify other hosts locally before claiming support.
2. From the source checkout or extracted archive, build and install the binary into a directory on `PATH`, for example:

   ```sh
   scripts/install.sh "$HOME/.local"
   ```

   This runs `cargo install --locked --path . --root <prefix>` and copies this skill to `<prefix>/share/tunemend/skills/`.
3. Register one stdio server named `tunemend` using the installed executable's absolute path and arguments `["mcp"]`. Reuse an existing entry. For user-wide registration:

   ```sh
   codex mcp add tunemend -- <absolute-prefix>/bin/tunemend mcp
   claude mcp add --scope user tunemend -- <absolute-prefix>/bin/tunemend mcp
   ```

   Use the command for the person's client; register both only when requested. Keep user-wide entries free of a fixed workspace: the server stores work in its launch directory. For a specific project, `--workspace <directory>` selects its storage explicitly.
4. Copy the installed `<prefix>/share/tunemend/skills/tunemend/` directory, including `references/`, into the client's skill folder (`~/.codex/skills/` or `~/.claude/skills/`).
5. Check the installed binary with `--version`. Reconnect the client, verify tool discovery, then call `get_capabilities` and `inspect_project` with `view: "projects"` through that client. A successful build or connection status alone is not a successful tool call. If reconnection needs a new session, report that remaining step.

Keep the MCP connection open during listening. After reconnecting, an identical `present_review` retry returns the existing presentation's live URL. Report the installed commit, verified client and next action briefly.

New work is stored in `.tunemend/`. An existing `.retro-midi/` store is reused when `.tunemend/` is absent; the directories are not merged. Do not remove saved work when replacing an installation.

The reference sound source is generated inside the binary; nothing else needs downloading. No DAW is needed.

## Optional score backend

The Compile score workflow additionally needs Python 3 (standard library only)
and consumer-supplied encoder/template/bank inputs. They are not needed for
ordinary MIDI creation, source loading, reference preview or feedback. See the
[score reference](score-target.md) before choosing a backend.

## Sample conversion

Sample conversion is built in; no external codec or Python is needed. See the
[sample reference](sample-conversion.md).
