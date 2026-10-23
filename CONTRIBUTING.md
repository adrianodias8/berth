# Contributing to berth

Thanks for helping. berth is a free, open-source take on what OrbStack does,
built in Rust on Apple's Virtualization.framework. This guide covers setting
up, the layout, the rules, and how changes get tested.

## The one hard rule: clean room

berth must not contain OrbStack code. Work only from OrbStack's public
documentation and from observing a running install from the outside (what
it answers on the network, what it mounts, how long things take). Don't
decompile, disassemble or translate its binaries, and don't paste code from
it. If you're unsure whether something crosses the line, ask in your pull
request before writing it.

The same goes for other closed-source products (Docker Desktop and others).
Open-source code may be used under its license. The guest kernel module is
derived from Linux and stays GPL-2.0.

## Setting up

You need an Apple Silicon Mac. macOS 26+ gives ASIF disks; macOS 27 adds the
custom Virtio device API that berth's own file sharing needs.

```sh
brew install rustup && rustup default stable
rustup target add aarch64-unknown-linux-musl     # the guest agent
xcode-select -p                                  # Xcode, for the app and for ring's C code
./scripts/build.sh                               # agent + signed target/release/berth
./target/release/berth start                     # first start: a few minutes
./scripts/build-app.sh && open target/Berth.app  # the app
```

If `cargo` isn't found, put the toolchain on your `PATH`
(`~/.rustup/toolchains/stable-aarch64-apple-darwin/bin`). If ring fails to
compile, set `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`
(`build.sh` does this for you).

berth keeps everything in `~/.berth` and its own Docker context, so it runs
next to OrbStack or Docker Desktop. Only one of them should own
`/var/run/docker.sock` and published ports at a time.

## Layout

| Path | What |
|---|---|
| `src/` | the `berth` CLI, daemon and root helper (macOS) |
| `agent/` | `berth-agent`, a static Linux binary embedded in `berth` and run in the VM |
| `guest/user-data.yaml` | first-boot provisioning (cloud-init) |
| `guest/kmod/virtiofs_poll/` | the polling virtio-fs guest driver (GPL-2.0, experimental) |
| `app/` | Berth.app (SwiftUI) |
| `scripts/` | build and signing |
| `bench/` | benchmarks against OrbStack; results in `bench/results/` |
| `docs/` | `LEARNINGS.md` (platform quirks, numbers, dead ends), `file-sharing.md` |

The README's "How it works" section explains the data paths; read it, and
`docs/LEARNINGS.md`, before changing networking, memory or file sharing.

## Making changes

- **Keep it small and focused.** One change per pull request, with the
  reason in the description.
- **Match the code around you.** Comments explain why, not what; module
  headers say what a file is for and what it relies on; names are plain
  words. Read a neighboring file before writing a new one.
- **No new background services or ports without a reason.** The guest
  agent runs as root and is reachable from inside the VM, and the helper
  runs as root on the Mac. Treat input from containers and from other local
  users as untrusted.
- **Root helper changes:** bump `helper::VERSION`, so the app asks users to
  update it. Validate everything the helper is asked to do; it only serves
  one user.
- **Dependencies:** prefer the standard library and the crates already in
  use. Pin downloaded tools to a version and a SHA-256.
- **Behavior visible to users** (CLI, app, README): update the README in the
  same change.

## Testing

```sh
cargo test --release -p berth          # host unit tests (FUSE server, helper rules, watcher, …)
cargo clippy --release -p berth
cargo build --release -p berth-agent --target aarch64-unknown-linux-musl
```

Unit tests don't cover the VM. For anything that touches it, also run the
real thing and say in your pull request what you ran:

- `berth start`, then `docker run --rm hello-world`, and `berth status`
  (it should show no errors);
- for networking: a published port, `curl <name>.berth.local`, a container IP
  with the helper installed, `host.docker.internal` from a container;
- for file sharing: the `bench/fs` scripts (see `docs/file-sharing.md`) and,
  for real workloads, `bench/ddev-drupal.sh`;
- for the app: launch it, check the screen you changed, and the first-run
  flow if you touched setup.

Logs are in `~/.berth/log/` (`daemon.log`, `console.log`); the helper's is
`/var/log/dev.berth.helper.log`.

## Performance work

Measure first and keep the numbers.

- `bench/compare.sh` runs the same suite on berth and OrbStack (startup, idle
  CPU and memory, ports, bind mounts) and writes `bench/results/<date>.md`.
- `bench/fs/` covers file sharing: small files, large files, per-request
  latency, and the polling driver's counters.
- `BERTH_FS_STATS=1` and `BERTH_FS_TRACE=1` show where file-sharing time goes.

Record what didn't work too, in `docs/LEARNINGS.md`, so nobody repeats it.

## The guest kernel module

`guest/kmod/virtiofs_poll` is a copy of the guest kernel's
`fs/fuse/virtio_fs.c` (Linux 6.12) with changes marked `berth:`. It is built
inside the VM against the running kernel:

```sh
berth shell -- sudo $PWD/bench/fs/guest-kbuild.sh $PWD/guest/kmod/virtiofs_poll
```

When Debian's kernel moves to a new major version, refresh the copy from
that version's `fs/fuse/virtio_fs.c` and `fuse_i.h`, and reapply the
`berth:` changes. A bug here can hang or crash the guest. Test unmounting
and reloading the module, and keep the built-in driver's devices (Apple's
shares) untouched.

## Commits and pull requests

- Write commit messages that explain what changed and why: a short subject,
  then the reasoning, measurements and anything surprising.
- Note any change in behavior, and how you tested it.
- If you used AI assistance, say so; that's welcome here, and the project
  itself is built that way.

## License

By contributing you agree your work is released under the MIT license
(`LICENSE`), except changes to `guest/kmod/`, which are GPL-2.0 like the
Linux code they derive from.
