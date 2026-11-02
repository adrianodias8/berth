# berth display protocol

How a live picture of a Linux surface gets from the guest to Berth.app,
and input gets back. Three parties speak it:

- the **agent** in the VM (`berth-agent`, `screen.stream` and
  `input.stream`, docs/gui-protocol.md), which captures a GUI session's
  compositor and injects input into it;
- the **daemon** (`src/gui/display.rs`, `src/gpu/frames.rs`,
  `src/gpu/ring.rs`), which keeps the latest frame of every surface in a
  shared-memory ring and serves `display` streams on its control socket;
- the **viewer** (Berth.app's `SurfaceStream.swift`, `berth gui watch`),
  which maps the ring and draws from it.

A **surface** is one picture: a GUI session's screen (fed by the agent's
`screen.stream`), or a scanout of the VM's GPU device (fed by the device
itself, for a desktop session that drives `/dev/dri/card0`). Plan:
`docs/plans/phase3-build.md`.

Everything here is version 1. Numbers in the ring are little-endian.

## The ring

Each surface has a file `~/.berth/run/surfaces/<key>.<gen>.ring` (mode
0600, the directory 0700), where `<key>` is `session-<machine>-<session>`
or `scanout-<n>` and `<gen>` the ring's generation. The daemon creates it,
maps it `MAP_SHARED` and writes it; viewers map it read-only. A surface
that changes size gets a new file with a new gen; the old file is removed
(a viewer that still has it mapped keeps reading valid, if stale, memory
until it unmaps it). The daemon removes every `*.ring` there when it starts.

Layout, for a surface `width` x `height`:

- `stride = align_up(width * 4, 256)`
- `slot_bytes = align_up(stride * height, 16384)`
- the header: bytes 0 to 16383
- slot `i` (0, 1, 2): at `16384 + i * slot_bytes`
- the cursor image: at `16384 + 3 * slot_bytes`, 256 x 256 BGRA, rows of
  1024 bytes (262144 bytes)
- file size: `16384 + 3 * slot_bytes + 262144`

Header fields:

| Offset | Type | Field |
|---|---|---|
| 0 | `u8[8]` | magic `BRTHSURF` |
| 8 | `u32` | version, 1 |
| 12 | `u32` | header size, 16384 |
| 16 | `u64` | gen (the same as in the file name) |
| 24 | `u32` | width (pixels) |
| 28 | `u32` | height |
| 32 | `u32` | stride (bytes per row) |
| 36 | `u32` | format: 1 = bgra8 (bytes B, G, R, A in memory: Metal's `bgra8Unorm`) |
| 40 | `u32` | slots, 3 |
| 44 | `u32` | slot_bytes |
| 48 | `u64[3]` | slot_offset (16384-aligned) |
| 72 | `u64[3]` | slot_seq: each slot's seqlock |
| 96 | `u32` | latest: the slot with the newest frame, `0xFFFFFFFF` before the first |
| 100 | `u32` | reserved (0) |
| 104 | `u64` | cursor_offset (where the cursor image is) |
| 112 | `i32` | cursor x: where the hot spot is on the surface |
| 116 | `i32` | cursor y |
| 120 | `u32` | cursor hot_x (in the image) |
| 124 | `u32` | cursor hot_y |
| 128 | `u32` | cursor w (≤ 256) |
| 132 | `u32` | cursor h (≤ 256) |
| 136 | `u32` | cursor visible (0 or 1) |
| 140 | `u32` | reserved (0) |
| 144 | `u64` | cursor seq: the cursor's seqlock |
| 152 to 16383 | | reserved (0) |

The cursor image's top-left corner goes at `(x - hot_x, y - hot_y)`.
Scanouts have a cursor (from the GPU's cursor commands); session surfaces
don't yet (the capture leaves the cursor out, so it isn't drawn at all).

A slot holds BGRA pixels, `height` rows of `stride` bytes; alpha is 255
where the source has none. Each frame is complete in its slot: the daemon
copies whatever the new frame didn't redraw from the previous one.

### Seqlocks

`slot_seq[i]` is **`2 * seq`** once frame `seq` is complete in slot `i`,
and **odd** (`2 * seq - 1`) while frame `seq` is being written into it.
0 means the slot has never held a frame. The cursor seq follows the same
rule with the cursor's own sequence number.

The writer: store the odd value, release fence, write the pixels, store
the even value (release), then store `latest` (release). It always writes
the slot after `latest`, so the latest frame stays whole while the next one
is written.

A reader of slot `i` that a doorbell said holds frame `seq`:

1. load `slot_seq[i]` (acquire); if it isn't `2 * seq`, the slot has moved
   on (skip this frame, a newer doorbell is coming);
2. read the pixels (for Metal, blit them to a private texture);
3. acquire fence, then load `slot_seq[i]` again: if it changed, the copy
   may be torn; drop it and keep the previous picture.

Frame and cursor sequence numbers are the daemon's, shared by all
surfaces: they only grow, but they skip.

## The display stream

A viewer connects to the daemon's control socket (`~/.berth/run/control.sock`)
and sends one line:

```json
{"cmd":"display","machine":"dev","session":"s-1","scanout":null,"max_fps":60,"once":false}
```

- `machine`: the Linux machine.
- `session`: the GUI session to show, and where input goes.
- `scanout`: show this scanout of the VM's GPU instead of the session's own
  capture (for a desktop session, whose JSON has `"scanout": 0`); input
  still goes to `session` if one is given.
- `max_fps`: at most this many doorbells a second, default 60, clamped to
  1..=120.
- `once`: one PNG instead of a stream (below).

One of `session` and `scanout` is needed. Missing fields take their
defaults. The daemon refuses the stream unless `"gui": true` is set in
config.json.

The reply is one line:

```json
{"ok":true,"transports":["ring"],"ring":"/Users/me/.berth/run/surfaces/session-dev-s-1.7451.ring","gen":7451,"width":1920,"height":1080,"stride":7680,"format":"bgra8","slots":3,"live":true}
```

or `{"ok":false,"error":"…"}`, after which the daemon closes. `transports`
lists how frames are shared; `"ring"` is the only one so far (others, an
IOSurface over XPC say, may follow). `live: false` (with `ring` null and
the sizes 0) means the surface has no frame yet, a session's capture still
starting, or a scanout that's off: a `resize` doorbell follows when it
has one.

A daemon from before this protocol hangs up without a reply; the viewer
should fall back to screenshots.

### Doorbells

Then the daemon sends one JSON line per event:

| Line | Meaning |
|---|---|
| `{"ev":"frame","slot":1,"seq":42,"damage":[[x,y,w,h]]}` | frame `seq` is in `slot`; `damage` covers everything that changed since the last frame this viewer was told about (the whole surface for the first one, after a resize, or when the viewer is more than 16 frames behind) |
| `{"ev":"resize","ring":…,"gen":…,"width":…,"height":…,"stride":…,"format":"bgra8","slots":3,"live":true}` | a new ring generation: map the new file (the reply's fields, without `ok` and `transports`); `live: false` means the surface went away for now. A full frame follows |
| `{"ev":"cursor","seq":7}` | the cursor fields or image changed; read them from the header |
| `{"ev":"keepalive"}` | nothing happened for 1 s |
| `{"ev":"end","error":"…"}` | the surface's source ended (the session's capture stopped, say); the daemon closes after it |

Frame and cursor doorbells come at most `max_fps` a second: frames within
one interval collapse into the newest, its damage the union. A viewer that
reads slower than that still sees every pixel change, only later.

### Input lines

The viewer may send one JSON line per event on the same connection:

| Line | What |
|---|---|
| `{"ev":"control","on":true}` | take control (`false`: give it back). Handled by the daemon, not forwarded; each change is one line in the activity log (`verb: "control"`, `who.kind: "berth-app"`) |
| `{"ev":"pointer","x":640,"y":400}` | move the pointer to a surface position (`0 ≤ x < width`, `0 ≤ y < height`) |
| `{"ev":"button","button":272,"down":true}` | a button: 272 left, 273 right, 274 middle (evdev `BTN_*`) |
| `{"ev":"scroll","dx":0,"dy":-15,"discrete":true}` | scroll by `dx`, `dy` (pixels, \|v\| ≤ 10000); `discrete` (optional, true/false or a number) marks wheel steps |
| `{"ev":"key","code":30,"down":true}` | a key by evdev code (`< 0x300`; 30 is `KEY_A`) |
| `{"ev":"release_all"}` | release every held key and button |

While the viewer has control, the daemon forwards each valid line,
unchanged, to the session's `input.stream` (one per session, opened on the
first event and closed when no viewer has control). Lines are dropped while
it doesn't, without a session, and when invalid (unknown `ev`, a field
missing or out of range). Giving control back, or hanging up while holding
it, sends `{"ev":"release_all"}`.

### Screenshots: once

With `"once":true` the reply is `{"ok":true,"payload_len":N}` followed by N
bytes of PNG of the surface's latest frame, or `{"ok":false,"error":"no
live surface"}` at once when there is none (no viewer is feeding the
session). It never starts a capture. `berth gui screenshot` on a Linux
session tries this first and falls back to the agent's `screenshot` verb.

## The agent's records (screen.stream)

The daemon's feeder for a session opens `gui <machine>` to the agent (as
docs/gui-protocol.md) and sends

```json
{"v":1,"session":"s-1","verb":"screen.stream","args":{"format":"bgra8","max_fps":60}}
```

The agent answers its usual reply line, `{"ok":true,"payload_len":0}` (or
`ok: false` with an error), and then streams records until either side
closes. Each record is a JSON line

```json
{"seq":12,"w":1920,"h":1080,"stride":7680,"format":"bgra8","damage":[[x,y,w,h]],"payload_len":16384}
```

followed by `payload_len` bytes: the packed rows of each damaged rect in
turn (for each rect in `damage` order, `h` rows of `w * 4` bytes, top row
first, BGRA). `payload_len` is the sum of `w * h * 4` over the rects;
`stride` describes the agent's own buffer and is informational. The first
record is always a full frame (one rect covering the screen); a record
with a new `w` or `h` is a resize. `seq` is the agent's own counter.

A record `{"error":"…"}` ends the stream (`{"error":"session ended"}` when
the session's compositor exits); the daemon passes it to viewers as an
`end` doorbell.

Reserved for GPU-rendered sessions (not built yet):

```json
{"format":"resource","res_id":5,"damage":[[x,y,w,h]],"payload_len":0}
```

names a virtio-gpu resource whose pixels the daemon reads from the GPU
device itself (`w` and `h` may be given; otherwise the surface keeps its
size). Until the daemon can (`display::RESOURCE_READER`), such a record
ends the stream with an error. An agent asked for `"format":"resource"`
answers `{"error":"not implemented"}`.

The feeder stops 10 s after the last viewer of the session leaves, and the
surface (and its ring) goes with it.

## The agent's input (input.stream)

For input the daemon opens another `gui <machine>` connection and sends

```json
{"v":1,"session":"s-1","verb":"input.stream","args":{}}
```

then, after the agent's reply line, the input lines above (without
`control`), one per line, verbatim. The agent releases everything held
when the connection closes.
