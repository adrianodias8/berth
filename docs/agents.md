# Agent testing: berth gui and berth mcp (experimental)

Coding agents (Claude Code, Codex, Cursor, …) can test a desktop app in a
berth guest instead of on your screen: push the app, open it, take
screenshots, click by accessibility label, type, read the accessibility
tree and check the result. Targets are macOS VMs (`mac:<vm>`,
`docs/mac-vms.md`) and Linux GUI machines (`linux:<machine>`,
`docs/gui.md`). Turn on the side you use:

```sh
berth config set mac true      # mac:* targets
berth config set gui true      # linux:* targets
```

## berth gui

```sh
eval "$(berth gui session new --target mac:base --clone)"   # a throwaway clone of the VM
berth gui note "checking the Save dialog"
berth gui push ./build/MyApp.app
berth gui open MyApp
berth gui screenshot                     # prints the PNG's path
berth gui click "Save"                   # by label, or a point: 640,400
berth gui type "hello"; berth gui key cmd+s
berth gui wait "Saved" --timeout 10
berth gui tree --app MyApp
berth gui session end
```

Each session is the agent's own screen: a fresh compositor on Linux, or on
macOS with `--clone` its own copy of the VM, deleted when the session ends.
Two agents never click in each other's apps. Clones need a VM slot (at most
2 macOS VMs run at once), so `--clone` waits in a queue when both are
taken. Without `--target`, `--clone` copies the VM `berth mac set-base`
saved last.

`session new` prints `export BERTH_GUI_SESSION=…`; later commands find the
target from the session. `--json` prints JSON. Sessions idle for
`gui_idle_minutes` (default 30) are ended by the next `berth gui` call.

`docs/agents/SKILL.md` is a skill that teaches an agent this loop; copy it
into the agent's skills folder (for Claude Code,
`~/.claude/skills/berth-gui/SKILL.md`).

## berth mcp

The same actions as MCP tools (`gui_session_new`, `gui_push`,
`gui_screenshot`, `gui_click`, … 17 in all), over stdin/stdout.
`gui_screenshot` returns the image itself, so the agent sees it. To add it
to Claude Code:

```sh
claude mcp add berth -- berth mcp
```

Other agents take the command `berth` with the argument `mcp` in their MCP
settings.

## Watching: the activity log and Berth.app

Every command is logged, with the agent that ran it, to
`~/.berth/logs/gui/<date>.jsonl` (screenshots in `shots/`); sessions and
the clone queue are in `~/.berth/gui/`. berth tells agents apart by their
environment and parent processes. With `"gui_titles": true` it also reads
Claude Code chat titles from `~/.claude/projects` (on your Mac only) so the
log names the chat.

Berth.app's **Agents** section shows a card per live session with its
latest screenshot and current command, a timeline per chat, and an
inspector for each command (screenshot, element hit, the command line to
rerun it). **Take Over** opens a macOS VM's window; **Watch** opens a Linux
session's Display window.

The protocol between host and guest agents is `docs/gui-protocol.md`.
