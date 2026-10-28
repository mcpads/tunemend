# Install Tunemend for the user

Handle download, setup and verification yourself. Ask the person only for access or information you cannot obtain. Preserve existing MCP entries and saved work.

## Select a build

Use the latest published release and verify its assets below. For an authorized
development checkout, use [source builds](#source-builds) and record its commit.
A 404 or empty release list means there is no installable release; do not invent
a tag, use withdrawn assets or silently substitute development source.

## Published binaries

1. Inspect the host OS, CPU, MCP client and any existing installation. Read the latest published release at `https://api.github.com/repos/mcpads/tunemend/releases/latest` (or `gh release view --repo mcpads/tunemend`). Pin its tag and asset URLs for this installation.
2. If a release exists, download the matching archive and `SHA256SUMS` from that release. Verify its SHA-256 before extracting or running it. Do not use a draft or substitute development source without authorization.

The workflow defines these packaging targets; only assets and checks from the
selected published release establish its host support.

| Packaging target | Archive |
| --- | --- |
| macOS 26+, Apple Silicon | `tunemend-aarch64-apple-darwin.tar.gz` |
| Linux x64, glibc 2.35+ | `tunemend-x86_64-unknown-linux-gnu.tar.gz` |
| Windows x64 | `tunemend-x86_64-pc-windows-msvc.zip` |

3. Extract into a temporary directory. Place `tunemend` (`tunemend.exe` on Windows) in a persistent user-writable application directory, preserving executable permissions. Keep the prior executable until the replacement connects; do not overwrite a running Windows executable. Archives also contain the production skill, README and license.
4. Run the installed binary with `--version`; it must match the release tag without `v`. Binaries are unsigned. Do not disable OS security controls; if launch is blocked, report the required approval or use an authorized source build.

## Connect and verify

Register one MCP stdio server named `tunemend`, with the installed executable's absolute path and arguments `["mcp"]`. Reuse an existing entry. For user-wide registration:

```sh
codex mcp add tunemend -- <absolute-path-to-tunemend> mcp
claude mcp add --scope user tunemend -- <absolute-path-to-tunemend> mcp
```

Use the command for the person's client; register both only when requested. Keep user-wide entries free of a fixed workspace: work belongs to the launch directory. A project-specific `--workspace <directory>` selects storage explicitly. Merge configuration without removing other servers.

Copy the archive's complete `skills/tunemend/` directory into the client's skill folder (`~/.codex/skills/` or `~/.claude/skills/`). Reconnect the client, verify tool discovery, then call `get_capabilities` and `inspect_project` with `view: "projects"` through it. A version check or connection status alone is not a successful tool call. If reconnection needs a new session, report that remaining step.

Keep the MCP connection open during listening. After reconnecting, retry the original `present_review` call to recover the same presentation's live URL. Report the installed version, verified client and next action briefly.

Saved work lives in `.tunemend/`. An existing `.retro-midi/` store is reused if `.tunemend/` is absent; stores are never merged. Do not remove saved work when replacing an installation.

## Source builds

For an authorized source build, use the selected checkout/commit or an existing published tag from `https://github.com/mcpads/tunemend`. Record the commit and executable hash; `--version` alone does not identify a development build. Install stable Rust and the host C build tools for bundled SQLite (Xcode Command Line Tools on macOS, MSVC on Windows, distribution build tools on Linux).

On Unix, `scripts/install.sh <prefix>` builds the binary and installs the complete skill under `<prefix>/share/tunemend/skills/`. Alternatively, run `cargo build --release --locked`, install `target/release/tunemend` (`tunemend.exe` on Windows), and copy `skills/tunemend/`. For this route, compare `--version` with the checkout’s Cargo package version, not a nonexistent release tag; verify the installed client calls as above. Other architectures require their own build and checks.

The reference sound source and sample codecs are built in. No DAW or extra sound download is required. The optional Compile score backend needs Python 3 and consumer-supplied encoder/template/bank inputs; see the [score reference](score-target.md).
