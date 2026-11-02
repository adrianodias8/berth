# berth-gui-mac

The guest agent inside berth's macOS VMs. It runs as a LaunchAgent in the
logged-in user's session, listens on vsock port 2401 and answers the
[gui protocol](../../docs/gui-protocol.md): `hello`, `session.*`, `push`,
`open`, `screenshot`, `click`, `type`, `key`, `scroll`, `drag`, `tree`,
`wait`, `windows`, `focus`, `quit`. The host reaches it through the VM
process's `~/.berth/mac/<name>/agent.sock`.

## Build

```sh
cd guest/mac-agent
swift build -c release        # .build/release/berth-gui-mac (arm64)
swift test                    # protocol, key combos, label matching, …
```

It's a plain Swift package with no dependencies (macOS 14+, the guest's
macOS needs no Xcode).

## Install in a base image

Copy the binary, `install.sh` and `dev.berth.gui-mac.plist` into the guest
(the push share or `scp`), then, as the auto-login user, in the guest:

```sh
./install.sh ./berth-gui-mac
```

It installs to `~/Library/Application Support/Berth/berth-gui-mac`, writes
`~/Library/LaunchAgents/dev.berth.gui-mac.plist`, starts it in `gui/<uid>`
and logs to `~/Library/Logs/berth-gui-mac.log`. `./install.sh --status` and
`./install.sh --uninstall` do what they say.

**Grant it once, in the base image**, through the VM's display window:

1. On its first start the agent asks macOS for Accessibility and Screen
   Recording. In System Settings > Privacy & Security, switch berth-gui-mac
   on under **Accessibility** and under **Screen & System Audio Recording**
   (`install.sh` opens both panes).
2. Restart it so Screen Recording applies: run `./install.sh` again.
3. `./install.sh --status` shows `accessibility: true, screen recording: true`.

Clones of the base image inherit the grants (spike S5 in the plan checks
this). TCC ties a grant to the binary's signature, and an ad-hoc build
changes with every rebuild, so after installing a new build grant it again
in the base image. Grants can't be scripted without MDM.

Without the grants, `hello`, `windows` (owners and frames from
CGWindowList), `open`, `push`, `focus`, `quit` and `session.*` still work;
the rest fail with a message naming the missing grant.

## Develop on the Mac

```sh
.build/release/berth-gui-mac --unix /tmp/gui.sock
printf '%s\n' '{"v":1,"verb":"hello","args":{}}' | nc -U /tmp/gui.sock
```

Careful on your own Mac: `click`, `type`, `key`, `scroll`, `drag`, `focus`,
`quit`, `open` and `push` act on your real screen and home folder. `hello`,
`windows`, `tree`, `screenshot` and `wait` only read. Run from a terminal,
the agent uses the terminal app's grants; `--check` prints them.

## How it works

| File | What |
|---|---|
| `Sources/GuiCore/` | pure logic, unit-tested: JSON and the wire format (`Protocol.swift`), key combos to virtual key codes, `Node` and label matching, app lookup by name, push paths, sessions, idle detection |
| `Sources/berth-gui-mac/Server.swift` | vsock / unix listener, one thread per connection, request line + payload framing |
| `Handler.swift` | one function per verb |
| `AX.swift` | the accessibility tree (AXUIElement, 2 s messaging timeout, 4000-element cap) |
| `Input.swift` | CGEvent mouse, scroll, keys and Unicode typing, serialised by one lock |
| `Capture.swift` | ScreenCaptureKit screenshots, `/usr/sbin/screencapture` fallback, idle thumbnails |
| `Apps.swift` | NSWorkspace launch, wait for the first window, focus, quit, window list |

Some choices:

- **Labels before coordinates.** A label click searches the target app's
  windows, then its menu bar; exact (case-insensitive) beats substring, and
  two distinct matches are an error naming both. Every pointer action reports
  `hit`, the element under the pointer before it acted.
- **Sessions are bookkeeping.** A macOS VM has one screen; agents are kept
  apart by the host giving each a clone. `session.list` reports who started
  each session and which apps it opened.
- **Push** pipes the tar into `/usr/bin/tar -x -C ~/Pushed`. bsdtar refuses
  absolute and `..` member paths, and files it writes carry no quarantine
  flag, so a pushed unsigned app opens without a Gatekeeper prompt.
- **`wait {"idle":true}`** compares 192x120 thumbnails every ~200 ms until
  two comparisons in a row change under 0.2 % of pixels (a caret blink or a
  clock is below that).
