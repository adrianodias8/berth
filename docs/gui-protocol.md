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
then speaks the protocol below for that machine. The daemon exposes it to
the CLI as `Request::Gui { target, request }` → `GuiReply`.

## Messages

After the transport header, the host sends **one JSON line** and the guest
answers **one JSON line**, optionally followed by a binary payload of exactly
`payload_len` bytes. One request per connection.

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

`note` never reaches a guest: the host logs it.

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
