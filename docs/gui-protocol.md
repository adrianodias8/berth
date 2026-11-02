# berth gui protocol

The contract between the host (`berth gui`, `berth mcp`, Berth.app) and the
guest GUI agents (`berth-gui-mac` in macOS VMs, the `gui` module of
`berth-agent` in the Linux VM). Plan: `docs/plans/gui-vms-agent-testing.md`.

## Transport

**macOS guests.** Each running macOS VM is its own process
(`berth mac run <name>`, hidden; it holds the `VZVirtualMachine` on the main
dispatch queue and, with `--display`, a window with a `VZVirtualMachineView`).
That process owns `~/.berth/mac/<name>/`:

| File | What |
|---|---|
| `disk.img`, `aux.img`, `hw.json` | the VM; `hw.json` is `{ "hardware_model", "machine_identifier" (both base64 of their `dataRepresentation`), "mac_address" }`. A clone shares its base's hardware model but gets its own machine identifier and MAC address |
| `vm.json` | `{ "cpus", "memory_mib", "base": "<name>" or null, "created" (Unix seconds) }`, plus optional `"user"` (the account made at install), `"display": {"width","height","ppi"}` (pixels) and `"build"` (the macOS build) |
| `run.pid` | the VM process, while it runs; it holds an `flock` on the file, so a VM counts as running only while the lock is held |
| `run.log` | the VM process's output |
| `control.sock` | one JSON request line, one JSON reply line: `{"op":"status"}` → `{"ok":true,"name","state","pid","started","display":<window shown>,"agent":<agent.sock path>}`, `{"op":"stop"}` (guest shutdown, hard stop after 60 s; `"force":true` stops at once), `{"op":"display"}`; errors are `{"ok":false,"error":"…"}` |
| `agent.sock` | every connection is forwarded to vsock port **2401** in the guest; if the guest can't be reached the host answers one line itself, `{"ok":false,"error":"…","payload_len":0}`, and closes |
| `push/` | shared into the guest (automount tag): `/Volumes/My Shared Files/berth-push` |
| `provision.json` | the account for the first boot after install (mode 0600), removed once that boot has started |
| `base/disk.img`, `base/aux.img` | the clean state `berth mac set-base` saved: clones are made from it and `reset` goes back to it |

`~/.berth/mac/default-base` names the VM `set-base` was last run on, which
throwaway clones start from. At most 2 macOS VMs run at once: `berth mac
start` (and `macvm::start`) refuse a third, naming the running ones.

`berth-gui-mac` listens on `AF_VSOCK` port 2401 (`VMADDR_CID_ANY`).

**Linux guests.** The host reaches `berth-agent` on vsock 2400 as today (the
daemon's `connect_vsock`) with the header line `gui <machine>\n`; the agent
then speaks the protocol below for that machine. The CLI reaches it through
the daemon's control socket: it sends `{"cmd":"gui","machine":"<machine>"}`
(`Request::Gui { machine }`), the daemon answers one `GuiReply` line
(`{"ok":true,"error":null}`, or `ok: false` with why, e.g. the VM isn't
running), and from then on carries the connection's bytes to and from the
agent's `gui <machine>` stream unchanged, so the client speaks the messages
below exactly as it does to a macOS VM's `agent.sock`.

`berth machine run|display` use the existing
`machine gui` action instead (`Request::Machine`, args
`[machine, request line]`): the reply line comes back as the action's
output, followed by the payload as one base64 line; it carries no `push`.
`berth-agent gui-local <machine> '<request>' [payload-file]`, as root in the
VM, runs the same handler from a shell (testing a build without replacing
the agent). Every Linux entry point answers
`{"ok":false,"error":"Linux GUI sessions are off: …"}` unless berth's
config.json has `"gui": true`.

## Messages

After the transport header, the host sends **one JSON line**, optionally
followed by a binary payload of exactly the request's `payload_len` bytes
(only `push` has one), and the guest answers **one JSON line**, optionally
followed by a binary payload of exactly the reply's `payload_len` bytes. One
request per connection. The host doesn't half-close its side after sending,
so the guest reads exactly `payload_len` bytes rather than to EOF.

Request:

```json
{"v":1, "session":"s-3f9a", "verb":"click", "args":{"label":"Save"}}
```

Reply:

```json
{"ok":true, "result":{...}, "error":null, "payload_len":0, "hit":{"role":"AXButton","label":"Save","frame":[x,y,w,h]}}
```

- `v` is the protocol version; a guest that doesn't know it replies
  `{"ok":false,"error":"unsupported version"}`.
- `session` is `null` outside a session (and for `hello` and `session.new`).
- `payload_len` in a request is omitted when 0.
- `hit` (optional) names the element a pointer action landed on; the host
  logs it. Coordinates are guest screen points, origin top-left.
- Errors never close the connection without a reply line.

## Verbs

| verb | args | result | payload |
|---|---|---|---|
| `hello` | `{}` | `{"os":"macos"\|"linux","agent":"<version>","screen":[w,h],"scale":2}` | – |
| `session.new` | `{"agent":"<who>"}` | `{"session":"s-…"}` (Linux: starts a headless compositor) | – |
| `session.end` | `{}` | `{}` | – |
| `session.list` | `{}` | `{"sessions":[{"session","agent","since","apps":[…]}]}` | – |
| `push` | `{"name":"MyApp.app","dest":null,"tar":true}` | `{"path":"/Users/berth/Pushed/MyApp.app"}` | a tar stream of `name` |
| `open` | `{"app":"/path or name","args":[…],"wait_s":20}` | `{"pid":123,"window":"MyApp"}` once a window exists | – |
| `screenshot` | `{"window":null\|"<app>","format":"png"}` | `{"width","height"}` | PNG bytes |
| `click` | `{"x":..,"y":..}` or `{"label":"Save"}`, `"button":"left"\|"right"`, `"count":1` | `{}` | – |
| `type` | `{"text":"hello"}` | `{}` | – |
| `key` | `{"combo":"cmd+a"}` (modifiers `cmd ctrl alt shift`; on Linux `cmd` is `super`) | `{}` | – |
| `scroll` | `{"x","y","dx","dy"}` | `{}` | – |
| `drag` | `{"from":[x,y],"to":[x,y]}` | `{}` | – |
| `tree` | `{"app":null\|"<app>","depth":12}` | `{"tree": Node}` | – |
| `wait` | `{"label":"Done"}` or `{"idle":true}`, `"timeout_s":20` | `{"waited_ms":…}` | – |
| `windows` | `{}` | `{"windows":[{"app","title","frame","focused"}]}` | – |
| `focus` | `{"app":"…"}` | `{}` | – |
| `quit` | `{"app":"…"}` | `{}` | – |

`Node` is `{"role","label","value","frame":[x,y,w,h],"enabled","focused","children":[Node]}`.
Label lookup is case-insensitive on `label` (AX title/description, AT-SPI
name), exact match first, then substring; an ambiguous label is an error
listing the candidates.

A container role (application, frame, window, dialog, panel, filler, scroll
pane, viewport) loses to any other match, and a match inside another match
is the same element (a GTK button "Save" holds a label "Save"): the outer
one is clicked, at its frame's centre.

`note` never reaches a guest: the host logs it.

`push`'s tar (the request's payload) holds `name` at its top level (a
leading `./` when `name` starts with `-`); `dest` null means the guest's
default folder (`~/Pushed`). For a Linux guest the host leaves out Apple's
`._*` metadata files; for a macOS guest it keeps them (bsdtar restores them).

`berth gui screenshot` saves the PNG as
`~/.berth/logs/gui/shots/<log id>.png`, the path in the log's `screenshot`.

### Linux specifics

- `session` may be null when the machine has exactly one session.
- `session.new` also takes `"app":["cmd","arg",…]` for an **app session**
  (cage, that one app full screen; `berth machine run`) and
  `"size":[w,h]` (agent sessions, default 1280×800). Its result adds
  `"size"` and `"kind"` (`"agent"` or `"app"`); `session.list` entries do
  too. In an app session `open` is refused, `windows` is the app, and
  `quit` ends the session.
- `push` extracts into `~/Pushed` of the machine's user unless `dest` is an
  absolute path or `~/…`; `open` finds a bare name there first (a folder
  runs its `AppRun`, `<name>` or `bin/<name>`), then on `PATH`. Its result
  adds `"app"` (the Wayland app id or X11 class).
- `click`'s result is `{"x","y"}`, the point clicked. `windows` entries add
  `"pid"`; `frame` is the client's surface.
- `scroll`: `dx`/`dy` in points, positive `dy` scrolls the content down; sent
  as mouse-wheel clicks of 15 points (at least one), so toolkits move by their
  own step per click.
- `wait` with `"label"` also returns the element as `hit`.
- Frames come from the toolkits over AT-SPI in window coordinates, moved by
  where sway put the window. GTK 4.18 reports some nested widgets 12 points
  off (they still land inside the element: clicks aim at the centre).

## Activity log

`~/.berth/logs/gui/<YYYY-MM-DD>.jsonl`, one line when a command starts
(`"phase":"start"`) and one when it ends (`"phase":"end"`), both with the
same `id`:

```json
{"id":"…","phase":"end","time":"2026-11-02T10:00:00Z","target":"mac:test","session":"s-3f9a",
 "verb":"click","args":{"label":"Save"},"ok":true,"error":null,"duration_ms":120,
 "screenshot":"/Users/…/.berth/logs/gui/shots/….png","hit":{…},
 "who":{"kind":"claude-code"|"cursor"|"codex"|"mcp"|"terminal","chat":"<id or null>","title":null,"pid":123,"cwd":"…"}}
```

Targets are written `mac:<vm>` or `linux:<machine>`.

## Sessions

Host-side state in `~/.berth/gui/sessions.json`:
`[{"session","target","agent":who,"clone":bool,"started","last_used","note"}]`.
Sessions idle longer than `gui_idle_minutes` (default 30) are ended by the
next `berth gui` call or the daemon. macOS: at most 2 VMs run at once
(Apple's license); `session new --clone` past that waits in
`~/.berth/gui/queue.json`, first come first served.

Details: times are UTC, `2026-11-02T10:00:00Z`; `note` is the session's
latest `berth gui note`. `queue.json` is
`[{"ticket","base","agent":who,"since","pid"}]`; an entry whose `pid` is
gone is dropped. Running macOS VMs are counted by live
`~/.berth/mac/*/run.pid`. A clone is named `gui-<8 hex>` and deleted when
its session ends. Both files change only under `flock` on
`~/.berth/gui/lock`. `gui_idle_minutes: 0` turns expiry off.
