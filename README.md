# Tunemend

**Retro game music, made with an agent and refined through listening.**

Describe a scene. Your agent makes the music, gives you a listening page, and revises it when you point out what sounds wrong. You do not need a DAW or music theory.

## What would you like to make?

**A looping theme for your game**

> Make a quiet village theme with a clear melody and a warm bass line. It should loop without an awkward pause.

**A new arrangement of an existing MIDI**

> Keep this melody, but make the accompaniment softer and less busy.

**A short sound effect**

> Make a rising pickup sound. Bright and quick, with a gentle ending.

Your agent handles the notes, instruments, mix and files. You can start from a description or bring an existing MIDI, sample or supported chip instrument.

## Listen, point, ask for a change

Open the listening link your agent gives you. Mark a moment or a passage, add a comment if you want, and choose **Send to AI**. Back in the conversation, ask your agent to review the marks.

> The accompaniment gets in the way here.
>
> This ending feels too abrupt.
>
> Keep the melody. Try a softer sound underneath it.

Your agent can compare the old and new versions and bring back another preview. Earlier versions and their feedback stay available, so you can return to a direction you liked.

You can also describe a change directly in the conversation. You do not have to identify the note or know which control to adjust.

## Get started with your agent

Give your agent this request:

> Install Tunemend from https://github.com/mcpads/tunemend. Follow `skills/tunemend/references/install.md`, connect it to my MCP client, and verify that it works. Then help me make a short looping theme.

The agent handles installation and connection checks. Native downloads are provided through [Releases](https://github.com/mcpads/tunemend/releases) for macOS, Linux and Windows; the [installation guide](skills/tunemend/references/install.md) lists supported systems. Codex and Claude Code can use it through MCP.

## Take the result with you

Ask for a finished mix, separate stems, MIDI, or an editable project to continue later. Work is saved locally and survives a new conversation.

MIDI carries the notes and supported controls; the editable project also keeps Tunemend's instruments, effects and mix. Putting the result into a particular game still depends on that game's audio tools. A chip preview is not a promise of identical sound on the original console.

For agents: the [production skill](skills/tunemend/SKILL.md) covers creation, revision and delivery. Tool schemas provide the exact inputs.

## License

[MIT](LICENSE) © 2026 mcpads.
