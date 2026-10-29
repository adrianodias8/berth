# Contributing to berth

Thanks for helping. berth is a free, open-source take on what OrbStack does,
built in Rust on Apple's Virtualization.framework. This guide covers setting
up, the layout, the rules, and how changes get tested.

## The one hard rule: clean room

berth must not contain OrbStack code. Work only from OrbStack's public
documentation and from observing a running install (what it answers on the
network, what it mounts, how long things take, what its running Linux kernel
reports in `/proc` and `/sys`). Don't
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
| `guest/berth-debug.sh`, `guest/berth-ca-sync.sh` | the debug shell; the Mac's CAs into the VM's trust store |
| `.github/workflows/` | release builds |
| `guest/kmod/virtiofs_poll/` | the polling virtio-fs guest driver (GPL-2.0) |
| `guest/kernel/` | berth's guest kernel: build scripts, config, the driver's patch for 7.2 (GPL-2.0), `check.sh` |
| `app/` | Berth.app (SwiftUI) |
| `scripts/` | build and signing |
| `bench/` | benchmarks against OrbStack; results in `bench/results/` |
| `docs/` | `LEARNINGS.md` (platform quirks, numbers, dead ends), `file-sharing.md`, `configuration.md`, `troubleshooting.md`, `KNOWN_ISSUES.md` |

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
- **Behavior visible to users** (CLI, app, README): update the README, and
  `docs/configuration.md` for new settings, in the same change. Add a line to
  `CHANGELOG.md`.

## Testing

```sh
cargo test --release -p berth          # host unit tests (FUSE server, helper rules, watcher, …)
cargo clippy --release -p berth
cargo build --release -p berth-agent --target aarch64-unknown-linux-musl
# the agent's tests run in the VM:
t=$(cd agent && cargo test --release --target aarch64-unknown-linux-musl --no-run 2>&1 | grep -o '/[^ )]*berth_agent-[0-9a-f]*')
berth shell -- sudo "$t"
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
  flow if you touched setup;
- for the guest kernel or anything the VM's kernel must support:
  `KCHECK_K8S=1 guest/kernel/check.sh` (19 checks plus k3s).

Logs are in `~/.berth/log/` (`daemon.log`, `console.log`); the helper's is
`/var/log/dev.berth.helper.log`.

## Performance work

Measure first and keep the numbers.

- `bench/compare.sh` runs the same suite on berth and OrbStack (startup, idle
  CPU and memory, ports, bind mounts) and writes `bench/results/<date>-<HHMM>.md`.
- `bench/fs/` covers file sharing: small files, large files, per-request
  latency, and the polling driver's counters.
- `BERTH_FS_STATS=1` and `BERTH_FS_TRACE=1` show where file-sharing time goes.

Record what didn't work too, in `docs/LEARNINGS.md`, so nobody repeats it.

## The guest kernel and its driver

berth's kernel (`guest/kernel`, see its README) is Linux 7.2 with the
polling driver built in; Debian's 6.12 kernel stays as the fallback and
the opt-out. The driver is in two forms, which must change together:

- `guest/kmod/virtiofs_poll`: a copy of 6.12's `fs/fuse/virtio_fs.c` with
  changes marked `berth:`, built at runtime on Debian's kernel. To try a
  change: `berth shell -- sudo $PWD/bench/fs/guest-kbuild.sh $PWD/guest/kmod/virtiofs_poll`.
- `guest/kernel/virtiofs_poll-7.2.patch`: the same changes on 7.2's
  `virtio_fs.c`, applied when the kernel is built.

One more patch, `guest/kernel/fuse-owner-7.2.patch`, changes 7.2's
`fs/fuse/dir.c`: on berth's shares `stat` shows the caller as the owner.

Moving to another kernel series means porting the patch to that series'
FUSE internals. A bug here can hang or crash the guest: run
`KCHECK_K8S=1 guest/kernel/check.sh`, a DDEV run, and test unmounting, and
keep the built-in driver's devices (Apple's shares) untouched.

## Releasing

`scripts/release.sh` builds `target/dist/Berth-<version>.zip` (version from
`Cargo.toml`, build number from the commit count). With
`BERTH_SIGN_IDENTITY` (a "Developer ID Application" certificate in the
keychain) it signs with the hardened runtime, and with
`BERTH_NOTARY_PROFILE` (`xcrun notarytool store-credentials`) it notarizes
and staples. Without them the app is ad-hoc signed and opens only on the Mac
that built it.

Pushing a tag such as `v0.2.0` runs the same script in GitHub Actions
(`.github/workflows/release.yml`) and attaches the zip to a release. The
workflow's header lists the secrets it needs for signing and notarization;
without them it publishes an ad-hoc prerelease. It also attaches the guest
kernel's source (`berth-kernel-<release>-source.tar`), which GPL-2.0 asks to
go with the kernel binary in the app. A release made by hand needs it too:
build the kernel with `KERNEL_SOURCE=1 guest/kernel/build.sh` and attach
`target/kernel/source.tar` under that name.

## Commits and pull requests

- Write commit messages that explain what changed and why: a short subject,
  then the reasoning, measurements and anything surprising.
- Note any change in behavior, and how you tested it.
- If you used AI assistance, say so; that's welcome here, and the project
  itself is built that way.

## License

By contributing you agree your work is released under the MIT license
(`LICENSE`), except changes to `guest/kmod/` and the kernel patches in
`guest/kernel/`, which are GPL-2.0 (`guest/LICENSE-GPL-2.0`) like the Linux
code they derive from.
