---
name: berth-gui
description: Test a Mac or Linux GUI app in a disposable berth guest (a macOS VM or a Linux machine) without touching the user's screen. Push the app, open it, take screenshots, click by accessibility label, type, read the accessibility tree and verify. Use when asked to try, test, check or screenshot a desktop app (SwiftUI, AppKit, GTK, Qt, Electron), or when a change to one should be seen running.
---

# Testing apps with `berth gui`

berth runs the app in a guest: a macOS VM (`mac:<vm>`) or a Linux machine
(`linux:<machine>`). You drive it through `berth gui` (or the same actions as
MCP tools from `berth mcp`: `gui_session_new`, `gui_push`, `gui_click`, …).
Never use the user's own screen, mouse or keyboard for this.

Every command is logged with who ran it, and the user can watch in
Berth.app's Agents section. Say what you're doing with `berth gui note`.

## The loop

1. **Start a session** on the target the user named (`berth mac list` and
   `berth machine list` show what exists). It gives you your own screen
   (Linux) or your own copy of the VM (macOS with `--clone`), so you never
   click in another agent's app.

   ```sh
   eval "$(berth gui session new --target mac:test --clone)"   # macOS: a throwaway clone
   eval "$(berth gui session new --target linux:dev)"           # Linux: a fresh screen
   ```

   This sets `BERTH_GUI_SESSION`; later commands find the target from it.
   Each `berth gui` call is its own shell command, so if your shell doesn't
   keep variables between commands, pass `--session <id>` instead. A macOS
   clone may wait for a slot: only two macOS VMs run at once.

2. **Say what you're checking.**

   ```sh
   berth gui note "checking that Save writes the file"
   ```

3. **Push the app** from this Mac (a `.app`, a binary, a folder). It lands in
   `~/Pushed` in the guest unless you pass `--to`; the command prints the
   path there.

   ```sh
   berth gui push build/Release/MyApp.app
   ```

4. **Open it** and wait for its window.

   ```sh
   berth gui open /Users/berth/Pushed/MyApp.app
   ```

5. **Look before you act.** A screenshot is a PNG file whose path is
   printed; read the image. The tree gives roles, labels and frames.

   ```sh
   berth gui screenshot                 # or --window MyApp
   berth gui tree --app MyApp --depth 6
   ```

6. **Act by label**, not by coordinates. Labels are matched without case,
   exact first, then as a substring; an ambiguous label is an error that
   lists the candidates, so pick a longer label. Use `x,y` (guest screen
   points, from the tree's frames) only when an element has no label.

   ```sh
   berth gui click "Save"
   berth gui type "hello world"
   berth gui key cmd+s
   berth gui click 412,230 --double
   berth gui scroll 400,300 --dy 240
   berth gui drag 100,100 300,100
   ```

7. **Verify**: wait for what should appear, then check it with a screenshot
   or the tree. Don't report success from the actions alone.

   ```sh
   berth gui wait "Saved" --timeout 10    # or: berth gui wait --idle
   berth gui screenshot
   berth gui note "Save works: the title shows Saved"
   ```

8. **End the session** when you're done, also after a failure. A macOS clone
   is deleted. Idle sessions end on their own after 30 minutes.

   ```sh
   berth gui session end
   ```

## Also

- `berth gui windows`, `berth gui focus <app>`, `berth gui quit <app>`.
- `--json` on any command prints the result as JSON (with `hit`: the element
  a click landed on, and `screenshot`: the PNG's path).
- On Linux, `cmd` in a key combination is the Super key.
- `berth gui session list` shows sessions and who started them.
- If `berth gui` says it's off, ask the user to turn it on (`"mac": true` or
  `"gui": true` in `~/.berth/config.json`); don't change their config yourself.
- If the guest's agent can't be reached, the VM or machine isn't running:
  tell the user rather than starting or stopping VMs you didn't create.

## MCP

`berth mcp` is an MCP server on stdin/stdout. Add it to the agent's MCP
config as the command `berth` with the argument `mcp` (optionally with
`BERTH_GUI_TARGET` in its environment). `gui_screenshot` returns the image
itself.

| Tool | Arguments (all also take `target`, `session`) |
|---|---|
| `gui_session_new` | `clone` (macOS: your own throwaway copy) |
| `gui_session_end`, `gui_session_list`, `gui_windows` | – |
| `gui_note` | `text` |
| `gui_push` | `path` (on this Mac), `to` |
| `gui_open` | `app` (a path, name or bundle ID), `args`, `wait_s` |
| `gui_screenshot` | `window` (an app name) |
| `gui_click` | `label`, or `x` and `y`; `button` (`left`/`right`), `count` |
| `gui_type` | `text` |
| `gui_key` | `combo` (`cmd+s`) |
| `gui_scroll` | `x`, `y`, `dx`, `dy` |
| `gui_drag` | `from`, `to` (`[x, y]`) |
| `gui_tree` | `app`, `depth` |
| `gui_wait` | `label` or `idle`, `timeout_s` |
| `gui_focus`, `gui_quit` | `app` |

- The server remembers the session after `gui_session_new` only while it
  keeps running. If your client starts it per call, pass `session` on every
  call. The session decides where a call goes, so passing the `target` you
  started from along with it is fine: the call still goes to your clone.
- `app` is the name `gui_open` reports (`opened BerthHello (window "Berth
  Hello", …)`), the bundle ID or the path; spaces are ignored, and a name
  that matches nothing gets the running apps listed.
