//! GUI sessions in Linux machines, for `berth gui` and `berth machine run`
//! (docs/gui-protocol.md is the contract; docs/plans/gui-vms-agent-testing.md
//! the plan).
//!
//! A session is a headless Wayland compositor inside a machine, run as the
//! machine's user: sway for agent sessions (any number of apps, windows
//! managed over sway's IPC), cage for an app session (one app, full screen).
//! Each has its own runtime folder in the machine, /tmp/berth-gui-<id>,
//! which is its XDG_RUNTIME_DIR: its Wayland socket, sway's IPC socket, its
//! own session D-Bus (where at-spi2 runs, for the accessibility tree) and its
//! logs. So sessions never see each other's windows or input.
//!
//! Rendering is wlroots' pixman (CPU) unless the machine has the VM's GPU
//! render node (install.rs binds it in), then GLES. Screenshots are grim
//! (wlr-screencopy), the keyboard is wtype (virtual-keyboard), the pointer
//! our own virtual-pointer client (wayland.rs), the tree a python3 AT-SPI
//! script (atspi.py), window management swaymsg.
//!
//! The agent keeps each session's record in /run/berth-gui/<machine>/ in the
//! VM; a session whose compositor is gone is dropped the next time it's
//! used or listed. Requests come over vsock (`gui <machine>`), from the
//! `machine gui` action (machines.rs, for `berth machine run`), or from
//! `berth-agent gui-local` for testing a build without replacing the agent.
//! Only with `"gui": true` in berth's config.json.

mod exec;
pub mod install;
mod keys;
mod sway;
mod tree;
pub mod wayland;

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use exec::{Machine, quote};

const STATE: &str = "/run/berth-gui";
const ATSPI: &str = include_str!("atspi.py");
/// Agent sessions' screen, in points.
const DEFAULT_SIZE: [u32; 2] = [1280, 800];
const SHORT: Duration = Duration::from_secs(20);
/// wtype's new virtual keyboard brings its own keymap, which the focused
/// app applies a moment later: keys sent at once are lost (seen with GTK 4).
const WTYPE_SETTLE: &str = "-s 150";

#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    pub v: u32,
    #[serde(default)]
    pub session: Option<String>,
    pub verb: String,
    #[serde(default)]
    pub args: Value,
    /// The payload's length (push's tar). The host doesn't half-close
    /// after sending, so this is how the guest knows where it ends; without
    /// it, the payload runs to EOF.
    #[serde(default)]
    pub payload_len: Option<u64>,
}

#[derive(Serialize)]
struct Reply {
    ok: bool,
    result: Value,
    error: Option<String>,
    payload_len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    hit: Option<Value>,
}

/// What a verb produced.
#[derive(Default)]
struct Done {
    result: Value,
    payload: Vec<u8>,
    hit: Option<Value>,
}

impl Done {
    fn of(result: Value) -> Self {
        Done { result, ..Default::default() }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Session {
    session: String,
    agent: String,
    since: String,
    /// "agent" (sway) or "app" (cage).
    kind: String,
    size: [u32; 2],
    apps: Vec<App>,
    #[serde(default)]
    next_app: u32,
}

#[derive(Serialize, Deserialize, Clone)]
struct App {
    app: String,
    pid: i64,
}

pub fn enabled() -> bool {
    crate::setting("gui").and_then(|v| v.as_bool()) == Some(true)
}

/// The `gui <machine>` header: one request line, one reply line and its
/// payload.
pub fn serve(machine: &str, mut host: std::os::unix::net::UnixStream) -> io::Result<()> {
    let mut input = host.try_clone()?;
    let reply = if enabled() {
        handle_stream(machine, &mut input)
    } else {
        (fail("Linux GUI sessions are off: set \"gui\": true in ~/.berth/config.json"), Vec::new())
    };
    host.write_all(&reply.0)?;
    host.write_all(&reply.1)
}

/// Read the request (and, for push, the tar stream after it) and answer.
fn handle_stream(machine: &str, input: &mut dyn Read) -> (Vec<u8>, Vec<u8>) {
    let line = match read_line(input, 4 << 20) {
        Ok(line) => line,
        Err(err) => return (fail(&format!("bad request: {err}")), Vec::new()),
    };
    match serde_json::from_str::<Request>(&line) {
        Ok(request) => answer(machine, &request, input),
        Err(err) => (fail(&format!("bad request: {err}")), Vec::new()),
    }
}

/// One request: the reply line (with its newline) and the payload.
pub fn answer(machine: &str, request: &Request, payload: &mut dyn Read) -> (Vec<u8>, Vec<u8>) {
    if request.v != 1 {
        return (fail("unsupported version"), Vec::new());
    }
    let mut limited;
    let payload: &mut dyn Read = match request.payload_len {
        Some(n) => {
            limited = (&mut *payload).take(n);
            &mut limited
        }
        None => payload,
    };
    match dispatch(machine, request, payload) {
        Ok(done) => {
            let reply = Reply { ok: true, result: done.result, error: None, payload_len: done.payload.len(), hit: done.hit };
            (line_of(&reply), done.payload)
        }
        Err(err) => (fail(&err), Vec::new()),
    }
}

fn fail(error: &str) -> Vec<u8> {
    line_of(&Reply { ok: false, result: json!({}), error: Some(error.to_string()), payload_len: 0, hit: None })
}

fn line_of(reply: &Reply) -> Vec<u8> {
    let mut out = serde_json::to_vec(reply).unwrap_or_default();
    out.push(b'\n');
    out
}

fn read_line(input: &mut dyn Read, max: usize) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8];
    while line.len() < max {
        if input.read(&mut byte)? == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// `berth-agent gui-local <machine> <request json> [payload file]`: the same
/// handler, from a shell in the VM (as root), for testing a new build. The
/// reply line goes to stdout; the payload to the file, or after the line. A
/// push reads its tar stream from stdin.
pub fn local(args: &[String]) -> i32 {
    let (Some(machine), Some(json)) = (args.first(), args.get(1)) else {
        eprintln!("usage: berth-agent gui-local <machine> '<request json>' [payload-file]");
        return 2;
    };
    // The request line then stdin, read exactly as from the host.
    let mut input = io::Cursor::new(format!("{}\n", json.trim())).chain(io::stdin());
    let (line, payload) = handle_stream(machine, &mut input);
    let mut out = io::stdout();
    let _ = out.write_all(&line);
    match args.get(2) {
        Some(file) if !payload.is_empty() => {
            if let Err(err) = std::fs::write(file, &payload) {
                eprintln!("{file}: {err}");
                return 1;
            }
        }
        _ => {
            let _ = out.write_all(&payload);
        }
    }
    if line.starts_with(b"{\"ok\":true") { 0 } else { 1 }
}

/// `berth-agent gui-install <machine> [omarchy]`: the --gui setup for a
/// machine that exists (testing; create runs the same).
pub fn local_install(args: &[String]) -> i32 {
    let Some(machine) = args.first() else {
        eprintln!("usage: berth-agent gui-install <machine> [omarchy]");
        return 2;
    };
    match crate::machines::setup_gui(machine, args.get(1).map_or("1", String::as_str)) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

/// `berth-agent gui-pointer <plan json>`, inside a session.
pub fn local_pointer(args: &[String]) -> i32 {
    let plan = match args.first().map(|j| serde_json::from_str::<wayland::Plan>(j)) {
        Some(Ok(plan)) => plan,
        _ => {
            eprintln!("usage: berth-agent gui-pointer '<plan json>'");
            return 2;
        }
    };
    match wayland::run(&plan) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

fn dispatch(machine: &str, request: &Request, payload: &mut dyn Read) -> Result<Done, String> {
    let args = &request.args;
    let s = |k: &str| args.get(k).and_then(Value::as_str);
    let verb = request.verb.as_str();
    if verb == "hello" {
        return Ok(Done::of(json!({"os": "linux", "agent": env!("CARGO_PKG_VERSION"), "screen": DEFAULT_SIZE, "scale": 1})));
    }
    let m = Machine::open(machine).map_err(|e| e.to_string())?;
    match verb {
        "session.new" => session_new(&m, args),
        "session.list" => session_list(&m),
        _ => {
            let session = resolve(&m, request.session.as_deref())?;
            let ctx = Ctx { m: &m, s: session };
            match verb {
                "session.end" => ctx.end(),
                "push" => ctx.push(s("name").unwrap_or(""), s("dest"), payload),
                "open" => ctx.open(args),
                "screenshot" => ctx.screenshot(s("window")),
                "click" => ctx.click(args),
                "type" => ctx.type_text(s("text").ok_or("type: give text")?),
                "key" => ctx.key(s("combo").ok_or("key: give a combo")?),
                "scroll" => ctx.scroll(args),
                "drag" => ctx.drag(args),
                "tree" => {
                    let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(12);
                    Ok(Done::of(json!({"tree": ctx.tree(s("app"), depth)?})))
                }
                "wait" => ctx.wait(args),
                "windows" => Ok(Done::of(json!({"windows": ctx.windows()?}))),
                "focus" => ctx.focus(s("app").ok_or("focus: give an app")?),
                "quit" => ctx.quit(s("app").ok_or("quit: give an app")?),
                other => Err(format!("unknown verb {other}")),
            }
        }
    }
}

// ---- Session records ----

fn state_dir(machine: &str) -> String {
    format!("{STATE}/{machine}")
}

fn valid_session(id: &str) -> bool {
    id.strip_prefix("s-").is_some_and(|h| (1..=16).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The runtime folder in the machine.
fn runtime(id: &str) -> String {
    format!("/tmp/berth-gui-{id}")
}

fn load(machine: &str, id: &str) -> Option<Session> {
    serde_json::from_slice(&std::fs::read(format!("{}/{id}.json", state_dir(machine))).ok()?).ok()
}

fn save(machine: &str, session: &Session) -> io::Result<()> {
    let dir = state_dir(machine);
    std::fs::create_dir_all(&dir)?;
    let tmp = format!("{dir}/.{}.tmp", session.session);
    std::fs::write(&tmp, serde_json::to_vec(session)?)?;
    std::fs::rename(tmp, format!("{dir}/{}.json", session.session))
}

fn forget(machine: &str, id: &str) {
    let _ = std::fs::remove_file(format!("{}/{id}.json", state_dir(machine)));
}

fn all(machine: &str) -> Vec<Session> {
    let mut out: Vec<Session> = std::fs::read_dir(state_dir(machine))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            load(machine, name.strip_suffix(".json").filter(|id| valid_session(id))?)
        })
        .collect();
    out.sort_by(|a, b| a.since.cmp(&b.since));
    out
}

/// One machine's records change one request at a time (an `open` adds an
/// app while another request may end the session).
struct Lock(#[allow(dead_code)] std::fs::File);

fn lock(machine: &str) -> io::Result<Lock> {
    std::fs::create_dir_all(state_dir(machine))?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(format!("{}/.lock", state_dir(machine)))?;
    unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
    Ok(Lock(file))
}

/// The session a request names, or the machine's only one.
fn resolve(m: &Machine, id: Option<&str>) -> Result<Session, String> {
    match id {
        Some(id) if !valid_session(id) => Err(format!("bad session id {id:?}")),
        Some(id) => load(&m.name, id).ok_or_else(|| format!("no session {id} in {}", m.name)),
        None => {
            let mut sessions = all(&m.name);
            if sessions.len() > 1 {
                sessions = live(m)?;
            }
            match sessions.as_slice() {
                [one] => Ok(one.clone()),
                [] => Err(format!("no GUI session in {}: start one with session.new", m.name)),
                many => Err(format!("{} has several sessions, name one: {}", m.name, many.iter().map(|s| s.session.as_str()).collect::<Vec<_>>().join(", "))),
            }
        }
    }
}

fn new_id() -> String {
    let mut bytes = [0u8; 3];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        bytes = [(nanos >> 16) as u8, (nanos >> 8) as u8, nanos as u8];
    }
    format!("s-{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

/// RFC 3339 UTC, without a date crate.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn now() -> String {
    rfc3339(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

// ---- Scripts run in the machine ----

/// Environment for programs in a session: toolkits on Wayland (X11 apps
/// through Xwayland) with their accessibility exposed over AT-SPI.
const APP_ENV: &str = "export XDG_SESSION_TYPE=wayland GDK_BACKEND=wayland,x11 QT_QPA_PLATFORM='wayland;xcb' MOZ_ENABLE_WAYLAND=1 \
     ELECTRON_OZONE_PLATFORM_HINT=auto SDL_VIDEODRIVER=wayland,x11 \
     GTK_A11Y=atspi NO_AT_BRIDGE=0 GNOME_ACCESSIBILITY=1 QT_ACCESSIBILITY=1 QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 ACCESSIBILITY_ENABLED=1; ";

/// The start of every script that runs in a live session: its environment,
/// or exit 3 when its compositor is gone.
fn prelude(id: &str) -> String {
    let rt = runtime(id);
    format!(
        "RT={rt}; pid=$(cat $RT/compositor.pid 2>/dev/null); \
         if [ -z \"$pid\" ] || ! kill -0 \"$pid\" 2>/dev/null; then echo 'berth-session-ended' >&2; exit 3; fi; \
         export XDG_RUNTIME_DIR=$RT DBUS_SESSION_BUS_ADDRESS=unix:path=$RT/bus; \
         for s in $RT/wayland-[0-9]; do [ -S \"$s\" ] && export WAYLAND_DISPLAY=${{s##*/}} && break; done; \
         for s in $RT/sway-ipc.*.sock; do [ -S \"$s\" ] && export SWAYSOCK=$s && break; done; \
         [ -s $RT/display ] && export DISPLAY=$(cat $RT/display); \
         {APP_ENV}cd ~ 2>/dev/null || true; "
    )
}

fn sway_config(size: [u32; 2]) -> String {
    format!(
        "# berth: a headless agent session (agent/src/gui/mod.rs)\n\
         output HEADLESS-1 resolution {}x{} position 0 0 scale 1 bg #2e3440 solid_color\n\
         default_border pixel 1\nfocus_on_window_activation focus\nxwayland enable\n\
         exec sh -c 'echo $DISPLAY > $XDG_RUNTIME_DIR/display'\n",
        size[0], size[1]
    )
}

/// The compositor's launcher: its own session bus, then sway or cage.
fn launcher(id: &str, app: Option<&[String]>, gpu_3d: bool) -> String {
    let rt = runtime(id);
    // gles2 only on a GPU that does 3D and is bound into the machine.
    let renderer = if gpu_3d {
        format!("if [ -e {} ]; then export WLR_RENDERER=gles2; else export WLR_RENDERER=pixman; fi; ", install::RENDER_NODE)
    } else {
        "export WLR_RENDERER=pixman; ".to_string()
    };
    let compositor = match app {
        Some(argv) => format!("cage -- {}", argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")),
        None => format!("sway -c {rt}/sway.conf"),
    };
    format!(
        "RT={rt}; echo $$ > $RT/compositor.pid; \
         export XDG_RUNTIME_DIR=$RT WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1; \
         {renderer}{APP_ENV}\
         if command -v dbus-daemon >/dev/null; then dbus-daemon --session --address=unix:path=$RT/bus --fork --nopidfile --print-pid > $RT/dbus.pid; fi; \
         export DBUS_SESSION_BUS_ADDRESS=unix:path=$RT/bus; \
         exec {compositor} >$RT/compositor.log 2>&1"
    )
}

fn missing_tools_message(missing: &str) -> String {
    format!(
        "GUI tools missing in this machine ({}). Install them with: berth machine create <name> --gui (new machines), \
         or in this one: sway cage grim wtype dbus at-spi2-core python3-gi (python-gobject on Arch, python3-gobject on Fedora) \
         and the Atspi typelib (gir1.2-atspi-2.0 on Debian/Ubuntu)",
        missing.trim()
    )
}

/// PNG width and height, from its IHDR.
fn png_size(png: &[u8]) -> Option<[u32; 2]> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let n = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
    Some([n(16), n(20)])
}

fn session_new(m: &Machine, args: &Value) -> Result<Done, String> {
    let agent = args.get("agent").and_then(Value::as_str).unwrap_or("").chars().take(200).collect::<String>();
    let app: Option<Vec<String>> = args.get("app").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
    if app.as_ref().is_some_and(|a| a.is_empty()) {
        return Err("session.new: app is an empty command".into());
    }
    let size = args
        .get("size")
        .and_then(Value::as_array)
        .and_then(|s| Some([s.first()?.as_u64()? as u32, s.get(1)?.as_u64()? as u32]))
        .filter(|[w, h]| (320..=7680).contains(w) && (240..=4320).contains(h))
        .unwrap_or(DEFAULT_SIZE);
    let compositor = if app.is_some() { "cage" } else { "sway swaymsg" };
    let missing = m
        .run_text(&format!("for t in {compositor} grim wtype; do command -v $t >/dev/null || printf '%s ' $t; done"), SHORT)
        .map_err(|e| e.to_string())?;
    if !missing.trim().is_empty() {
        return Err(missing_tools_message(&missing));
    }
    let id = new_id();
    let rt = runtime(&id);
    // The runtime folder, sway's config, and this binary for pointer input
    // (fed over stdin: it's the user writing in their own folder).
    let me = std::fs::read("/proc/self/exe").map_err(|e| e.to_string())?;
    let setup = format!(
        "set -e; umask 077; rm -rf {rt}; mkdir -m 700 {rt}; cat > {rt}/berth-gui-input; chmod 700 {rt}/berth-gui-input; \
         printf '%s' {} > {rt}/sway.conf",
        quote(&sway_config(size))
    );
    m.run(&setup, &me, SHORT).map_err(|e| format!("setting up the session: {e}"))?;
    m.spawn(&format!("berth-gui-{id}"), &launcher(&id, app.as_deref(), install::gpu_has_3d())).map_err(|e| format!("starting the compositor: {e}"))?;
    // Up when its Wayland socket (and sway's IPC) is there.
    let ready = format!(
        "RT={rt}; for i in $(seq 100); do \
           if ls $RT/wayland-[0-9] >/dev/null 2>&1 && {{ [ ! -e $RT/sway.conf ] || [ {} = cage ] || ls $RT/sway-ipc.*.sock >/dev/null 2>&1; }}; then exit 0; fi; \
           pid=$(cat $RT/compositor.pid 2>/dev/null); if [ -n \"$pid\" ] && ! kill -0 $pid 2>/dev/null; then break; fi; sleep 0.1; done; \
         tail -n 5 $RT/compositor.log >&2; exit 1",
        if app.is_some() { "cage" } else { "sway" }
    );
    let session = Session {
        session: id.clone(),
        agent,
        since: now(),
        kind: if app.is_some() { "app" } else { "agent" }.into(),
        size,
        apps: app.iter().map(|a| App { app: a[0].clone(), pid: 0 }).collect(),
        next_app: 0,
    };
    if let Err(err) = m.run(&ready, &[], SHORT) {
        let ctx = Ctx { m, s: session };
        let _ = ctx.end();
        return Err(format!("the compositor didn't start: {err}"));
    }
    let mut session = session;
    let ctx = Ctx { m, s: session.clone() };
    // The real screen size (cage takes the headless default), and a check
    // that capture works.
    if let Ok(png) = ctx.run(&format!("{}grim -t png -l 0 -", prelude(&id)), Duration::from_secs(30))
        && let Some(size) = png_size(&png)
    {
        session.size = size;
    }
    // Toolkits that wait for a screen reader before exposing anything.
    let _ = ctx.atspi(&json!({"mode": "enable"}));
    save(&m.name, &session).map_err(|e| e.to_string())?;
    Ok(Done::of(json!({"session": id, "size": session.size, "kind": session.kind})))
}

/// The machine's sessions whose compositor still runs; the others are
/// cleaned up.
fn live(m: &Machine) -> Result<Vec<Session>, String> {
    let sessions = all(&m.name);
    // Which are still alive, in one call.
    let check: Vec<String> = sessions
        .iter()
        .map(|s| format!("p=$(cat {}/compositor.pid 2>/dev/null); [ -n \"$p\" ] && kill -0 $p 2>/dev/null && echo {}", runtime(&s.session), s.session))
        .collect();
    let alive = if check.is_empty() { String::new() } else { m.run_text(&format!("{}; true", check.join("; ")), SHORT).map_err(|e| e.to_string())? };
    let mut out = Vec::new();
    for s in sessions {
        if alive.lines().any(|l| l.trim() == s.session) {
            out.push(s);
        } else {
            let _lock = lock(&m.name);
            Ctx { m, s }.cleanup();
        }
    }
    Ok(out)
}

fn session_list(m: &Machine) -> Result<Done, String> {
    let mut out = Vec::new();
    for s in live(m)? {
        let apps: Vec<&str> = s.apps.iter().map(|a| a.app.as_str()).collect();
        out.push(json!({"session": s.session, "agent": s.agent, "since": s.since, "apps": apps, "kind": s.kind, "size": s.size}));
    }
    Ok(Done::of(json!({"sessions": out})))
}

/// A verb on one session.
struct Ctx<'a> {
    m: &'a Machine,
    s: Session,
}

impl Ctx<'_> {
    fn id(&self) -> &str {
        &self.s.session
    }

    /// Run a script in the session (prelude included by the caller).
    fn run(&self, script: &str, timeout: Duration) -> Result<Vec<u8>, String> {
        self.m.run(script, &[], timeout).map_err(|e| {
            let e = e.to_string();
            if e.contains("berth-session-ended") {
                // Why, before its folder goes.
                let log = self.m.run_text(&format!("tail -n 3 {}/compositor.log 2>/dev/null; true", runtime(self.id())), SHORT).unwrap_or_default();
                let _lock = lock(&self.m.name);
                self.cleanup();
                let log = log.trim().replace('\n', " | ");
                format!("session {} has ended (its compositor exited){}", self.id(), if log.is_empty() { String::new() } else { format!(": {log}") })
            } else {
                e
            }
        })
    }

    fn sh(&self, body: &str, timeout: Duration) -> Result<Vec<u8>, String> {
        self.run(&format!("{}{body}", prelude(self.id())), timeout)
    }

    fn sh_text(&self, body: &str) -> Result<String, String> {
        self.sh(body, SHORT).map(|o| String::from_utf8_lossy(&o).into_owned())
    }

    fn sway(&self) -> bool {
        self.s.kind == "agent"
    }

    /// Stop everything the session started and forget it.
    fn cleanup(&self) {
        let rt = runtime(self.id());
        self.m.stop_units(&format!("berth-gui-{}*", self.id()));
        // Whatever is left (machines without systemd; apps sway started):
        // the processes with this session's runtime folder.
        let kill = format!(
            "RT={rt}; for sig in TERM KILL; do for p in /proc/[0-9]*; do \
               tr '\\0' '\\n' < $p/environ 2>/dev/null | grep -qx \"XDG_RUNTIME_DIR=$RT\" && kill -$sig ${{p##*/}} 2>/dev/null; done; \
               [ $sig = TERM ] && sleep 0.3; done; \
             for f in $RT/*.pid; do [ -s \"$f\" ] && kill -9 $(cat \"$f\") 2>/dev/null; done; rm -rf $RT; true"
        );
        let _ = self.m.run(&kill, &[], SHORT);
        forget(&self.m.name, self.id());
    }

    fn end(&self) -> Result<Done, String> {
        let _lock = lock(&self.m.name).map_err(|e| e.to_string())?;
        self.cleanup();
        Ok(Done::of(json!({})))
    }

    fn sway_windows(&self) -> Result<Vec<sway::Window>, String> {
        let out = self.sh_text("swaymsg -r -t get_tree")?;
        let tree: Value = serde_json::from_str(&out).map_err(|e| format!("swaymsg: {e}"))?;
        Ok(sway::windows(&tree))
    }

    /// The session's windows. An app session has one: its app, full screen.
    fn windows(&self) -> Result<Vec<sway::Window>, String> {
        if self.sway() {
            return self.sway_windows();
        }
        let [w, h] = self.s.size;
        Ok(self
            .s
            .apps
            .iter()
            .map(|a| sway::Window { id: 0, app: a.app.clone(), title: a.app.clone(), frame: [0, 0, i64::from(w), i64::from(h)], focused: true, pid: a.pid })
            .collect())
    }

    fn push(&self, name: &str, dest: Option<&str>, payload: &mut dyn Read) -> Result<Done, String> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err("push: name is a file or folder name".into());
        }
        let dest = match dest {
            None | Some("") => "~/Pushed".to_string(),
            Some(d) if d.starts_with('/') => quote(d),
            Some(d) if d.starts_with("~/") => format!("~/{}", quote(&d[2..])),
            Some(d) => return Err(format!("push: dest {d:?} is an absolute path or ~/…")),
        };
        let mut tar = Vec::new();
        payload.read_to_end(&mut tar).map_err(|e| format!("reading the tar stream: {e}"))?;
        if tar.is_empty() {
            return Err("push: no tar stream after the request".into());
        }
        let out = self
            .m
            .run(
                &format!("set -e; mkdir -p {dest}; cd {dest}; rm -rf ./{0}; tar -x --no-same-owner -f -; test -e ./{0}; pwd", quote(name)),
                &tar,
                Duration::from_secs(300),
            )
            .map_err(|e| format!("push: {e}"))?;
        let dir = String::from_utf8_lossy(&out).trim().to_string();
        Ok(Done::of(json!({"path": format!("{dir}/{name}")})))
    }

    fn open(&self, args: &Value) -> Result<Done, String> {
        if !self.sway() {
            return Err("this is an app session (one app); start an agent session with session.new to open more".into());
        }
        let app = args.get("app").and_then(Value::as_str).filter(|a| !a.is_empty()).ok_or("open: give an app")?;
        let mut argv: Vec<String> = args.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
        let wait = Duration::from_secs(args.get("wait_s").and_then(Value::as_u64).unwrap_or(20).clamp(1, 300));
        // A pushed app by name, a path, or a command.
        let exe = if let Some(rest) = app.strip_prefix("~/") { format!("\"$HOME\"/{}", quote(rest)) } else { quote(app) };
        let resolve = format!(
            "e={exe}; if [ \"${{e#*/}}\" = \"$e\" ] && [ -e \"$HOME/Pushed/$e\" ]; then e=\"$HOME/Pushed/$e\"; fi; \
             if [ -d \"$e\" ]; then for c in \"$e/AppRun\" \"$e/$(basename \"$e\")\" \"$e/bin/$(basename \"$e\")\"; do [ -x \"$c\" ] && e=$c && break; done; fi; \
             command -v \"$e\" || {{ echo \"no app $e\" >&2; exit 1; }}"
        );
        let path = self.sh_text(&resolve)?.trim().to_string();
        // Electron and Chromium expose their tree only when asked.
        let base = path.rsplit('/').next().unwrap_or("").to_lowercase();
        let electron = ["electron", "chromium", "chrome", "google-chrome", "code", "brave"].contains(&base.as_str())
            || self.sh_text(&format!("[ -e \"$(dirname {})/resources/app.asar\" ] && echo yes; true", quote(&path)))?.trim() == "yes";
        if electron && !argv.iter().any(|a| a.contains("force-renderer-accessibility")) {
            argv.insert(0, "--force-renderer-accessibility".into());
        }
        let appimage = path.to_lowercase().ends_with(".appimage");
        let before: Vec<i64> = self.sway_windows()?.iter().map(|w| w.id).collect();
        let n = {
            let _lock = lock(&self.m.name).map_err(|e| e.to_string())?;
            let mut s = load(&self.m.name, self.id()).ok_or("the session has ended")?;
            s.next_app += 1;
            save(&self.m.name, &s).map_err(|e| e.to_string())?;
            s.next_app
        };
        let words: Vec<String> = std::iter::once(path.clone()).chain(argv).map(|a| quote(&a)).collect();
        let script = format!(
            "{}echo $$ > $RT/app-{n}.pid; {}exec {} >$RT/app-{n}.log 2>&1",
            prelude(self.id()),
            if appimage { "export APPIMAGE_EXTRACT_AND_RUN=1; " } else { "" },
            words.join(" ")
        );
        self.m.spawn(&format!("berth-gui-{}-a{n}", self.id()), &script).map_err(|e| format!("open: {e}"))?;
        let deadline = Instant::now() + wait;
        let mut pid = 0i64;
        loop {
            std::thread::sleep(Duration::from_millis(300));
            if pid == 0 {
                pid = self.sh_text(&format!("cat $RT/app-{n}.pid 2>/dev/null; true"))?.trim().parse().unwrap_or(0);
            }
            let windows = self.sway_windows()?;
            let new: Vec<&sway::Window> = windows.iter().filter(|w| !before.contains(&w.id)).collect();
            // Its own window, or (apps that hand off to a running instance or
            // a D-Bus-activated process) any new one.
            if let Some(w) = new.iter().find(|w| w.pid == pid).or(new.first()) {
                let record = App { app: if w.app.is_empty() { base.clone() } else { w.app.clone() }, pid: w.pid };
                let _lock = lock(&self.m.name).map_err(|e| e.to_string())?;
                if let Some(mut s) = load(&self.m.name, self.id()) {
                    s.apps.push(record);
                    let _ = save(&self.m.name, &s);
                }
                return Ok(Done::of(json!({"pid": w.pid, "window": w.title, "app": w.app})));
            }
            let exited = pid != 0 && self.sh_text(&format!("kill -0 {pid} 2>/dev/null || echo gone"))?.trim() == "gone";
            if exited || Instant::now() > deadline {
                let log = self.sh_text(&format!("tail -n 5 $RT/app-{n}.log 2>/dev/null; true")).unwrap_or_default();
                let what = if exited { "exited before showing a window" } else { "showed no window in time" };
                return Err(format!("{app} {what}{}", if log.trim().is_empty() { String::new() } else { format!(": {}", log.trim().replace('\n', " | ")) }));
            }
        }
    }

    fn screenshot(&self, window: Option<&str>) -> Result<Done, String> {
        let geometry = match window {
            Some(app) if !app.is_empty() => {
                let windows = self.windows()?;
                let w = sway::find(&windows, app)?;
                format!("-g {}", quote(&format!("{},{} {}x{}", w.frame[0], w.frame[1], w.frame[2], w.frame[3])))
            }
            _ => String::new(),
        };
        let png = self.sh(&format!("grim -t png -l 3 {geometry} -"), Duration::from_secs(30))?;
        let [width, height] = png_size(&png).ok_or("grim gave no PNG")?;
        Ok(Done { result: json!({"width": width, "height": height}), payload: png, hit: None })
    }

    fn pointer(&self, steps: Vec<wayland::Step>) -> Result<(), String> {
        let plan = json!({"extent": self.s.size, "steps": steps});
        self.sh(&format!("$RT/berth-gui-input gui-pointer {}", quote(&plan.to_string())), SHORT).map(|_| ())
    }

    fn click(&self, args: &Value) -> Result<Done, String> {
        let button = match args.get("button").and_then(Value::as_str).unwrap_or("left") {
            "left" => wayland::BTN_LEFT,
            "right" => wayland::BTN_RIGHT,
            "middle" => wayland::BTN_MIDDLE,
            other => return Err(format!("click: button {other:?} (left, right, middle)")),
        };
        let count = args.get("count").and_then(Value::as_u64).unwrap_or(1) as u32;
        let (x, y, hit) = if let Some(label) = args.get("label").and_then(Value::as_str) {
            // A window that has just appeared (right after open, or a dialog)
            // can show before its app has published its tree: look again for
            // a moment before giving up.
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let root = self.tree(None, 30)?;
                match tree::find(&root, label) {
                    Ok(node) => {
                        let (x, y) = node.center();
                        break (x, y, Some(node.summary()));
                    }
                    Err(e) if e.starts_with("no element") && Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            let x = args.get("x").and_then(Value::as_f64).ok_or("click: give x and y, or a label")?.round() as i64;
            let y = args.get("y").and_then(Value::as_f64).ok_or("click: give x and y, or a label")?.round() as i64;
            // What's there, for the log; a missing tree doesn't stop the click.
            let hit = self.tree(None, 30).ok().and_then(|root| tree::hit(&root, x, y).map(tree::Node::summary));
            (x, y, hit)
        };
        self.pointer(wayland::click_steps(x, y, button, count))?;
        Ok(Done { result: json!({"x": x, "y": y}), payload: Vec::new(), hit })
    }

    fn type_text(&self, text: &str) -> Result<Done, String> {
        if !text.is_empty() {
            self.sh(&format!("wtype {WTYPE_SETTLE} -d 4 -- {}", quote(text)), Duration::from_secs(120))?;
        }
        Ok(Done::of(json!({})))
    }

    fn key(&self, combo: &str) -> Result<Done, String> {
        let args = keys::wtype_args(combo)?;
        self.sh(&format!("wtype {WTYPE_SETTLE} {}", args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")), SHORT)?;
        Ok(Done::of(json!({})))
    }

    fn scroll(&self, args: &Value) -> Result<Done, String> {
        let n = |k: &str| args.get(k).and_then(Value::as_f64);
        let mut steps = Vec::new();
        if let (Some(x), Some(y)) = (n("x"), n("y")) {
            steps.push(wayland::Step::Move([x.round() as i64, y.round() as i64]));
            steps.push(wayland::Step::Sleep(20));
        }
        steps.push(wayland::Step::Axis { dx: n("dx").unwrap_or(0.0), dy: n("dy").unwrap_or(0.0) });
        self.pointer(steps)?;
        Ok(Done::of(json!({})))
    }

    fn drag(&self, args: &Value) -> Result<Done, String> {
        let point = |k: &str| -> Result<[i64; 2], String> {
            let a = args.get(k).and_then(Value::as_array).ok_or(format!("drag: give {k} as [x, y]"))?;
            match (a.first().and_then(Value::as_f64), a.get(1).and_then(Value::as_f64)) {
                (Some(x), Some(y)) => Ok([x.round() as i64, y.round() as i64]),
                _ => Err(format!("drag: give {k} as [x, y]")),
            }
        };
        self.pointer(wayland::drag_steps(point("from")?, point("to")?))?;
        Ok(Done::of(json!({})))
    }

    fn atspi(&self, opts: &Value) -> Result<String, String> {
        let script = format!("python3 - {} <<'BERTH_ATSPI'\n{ATSPI}\nBERTH_ATSPI\n", quote(&opts.to_string()));
        self.sh(&script, Duration::from_secs(40)).map(|o| String::from_utf8_lossy(&o).into_owned())
    }

    fn tree(&self, app: Option<&str>, depth: u64) -> Result<tree::Node, String> {
        let windows = self.windows()?;
        let pids: Vec<i64> = match app {
            Some(a) if !a.is_empty() => sway::find(&windows, a).map(|w| vec![w.pid]).unwrap_or_default(),
            _ => Vec::new(),
        };
        let [w, h] = self.s.size;
        let opts = json!({
            "mode": "tree", "depth": depth.clamp(1, 60), "max": 4000, "app": app, "pids": pids,
            "windows": windows.iter().map(|w| json!({"pid": w.pid, "title": w.title, "frame": w.frame})).collect::<Vec<_>>(),
            "screen": [0, 0, w, h],
        });
        let out = self.atspi(&opts)?;
        serde_json::from_str(&out).map_err(|e| format!("the accessibility tree: {e}"))
    }

    fn wait(&self, args: &Value) -> Result<Done, String> {
        let timeout = Duration::from_secs_f64(args.get("timeout_s").and_then(Value::as_f64).unwrap_or(20.0).clamp(0.1, 600.0));
        let start = Instant::now();
        if let Some(label) = args.get("label").and_then(Value::as_str) {
            loop {
                let found = self.tree(None, 30).ok().and_then(|root| tree::find(&root, label).ok().map(tree::Node::summary));
                if let Some(node) = found {
                    return Ok(Done { result: json!({"waited_ms": start.elapsed().as_millis() as u64}), payload: Vec::new(), hit: Some(node) });
                }
                if start.elapsed() > timeout {
                    return Err(format!("no {label:?} after {:.0}s", timeout.as_secs_f64()));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        if args.get("idle").and_then(Value::as_bool) == Some(true) {
            // Idle: two screenshots in a row, 300 ms apart, are the same.
            let mut last = None;
            loop {
                let png = self.sh("grim -t png -l 0 -", Duration::from_secs(30))?;
                let hash = png.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
                if last == Some(hash) {
                    return Ok(Done::of(json!({"waited_ms": start.elapsed().as_millis() as u64})));
                }
                if start.elapsed() > timeout {
                    return Err(format!("the screen kept changing for {:.0}s", timeout.as_secs_f64()));
                }
                last = Some(hash);
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        Err("wait: give a label, or idle: true".into())
    }

    fn focus(&self, app: &str) -> Result<Done, String> {
        if self.sway() {
            let windows = self.sway_windows()?;
            let w = sway::find(&windows, app)?;
            self.sh(&format!("swaymsg -q '[con_id={}]' focus", w.id), SHORT)?;
        } else {
            sway::find(&self.windows()?, app)?;
        }
        Ok(Done::of(json!({})))
    }

    fn quit(&self, app: &str) -> Result<Done, String> {
        if self.sway() {
            let windows = self.sway_windows()?;
            let w = sway::find(&windows, app)?;
            self.sh(&format!("swaymsg -q '[con_id={}]' kill", w.id), SHORT)?;
            let _lock = lock(&self.m.name).map_err(|e| e.to_string())?;
            if let Some(mut s) = load(&self.m.name, self.id()) {
                if let Some(i) = s.apps.iter().position(|a| a.pid == w.pid) {
                    s.apps.remove(i);
                }
                let _ = save(&self.m.name, &s);
            }
            return Ok(Done::of(json!({})));
        }
        // An app session is its app: quitting it ends the session.
        sway::find(&self.windows()?, app)?;
        self.end()
    }
}

/// Base64 (standard alphabet, padded), for payloads that travel inside the
/// `machine gui` action's text reply.
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `machine gui` action: a request line in, the reply line out, and a
/// payload (a screenshot) as a base64 line after it. No push: it has no
/// stream to read.
pub fn machine_action(machine: &str, request: &str) -> String {
    if !enabled() {
        return String::from_utf8_lossy(&fail("Linux GUI sessions are off: set \"gui\": true in ~/.berth/config.json")).into_owned();
    }
    let (line, payload) = match serde_json::from_str::<Request>(request) {
        Ok(r) if r.verb == "push" => (fail("push needs the gui channel (a tar stream)"), Vec::new()),
        Ok(r) => answer(machine, &r, &mut io::empty()),
        Err(err) => (fail(&format!("bad request: {err}")), Vec::new()),
    };
    let mut out = String::from_utf8_lossy(&line).into_owned();
    if !payload.is_empty() {
        out.push_str(&base64(&payload));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_without_the_setting() {
        // Tests run with no berth config.json beside them.
        let out = machine_action("x", r#"{"v":1,"verb":"hello"}"#);
        assert!(out.starts_with("{\"ok\":false") && out.contains("Linux GUI sessions are off") && out.ends_with('\n'), "{out}");
    }

    #[test]
    fn a_request_names_its_payload_length() {
        let r: Request = serde_json::from_str(r#"{"v":1,"verb":"push","args":{"name":"a"},"payload_len":5}"#).unwrap();
        assert_eq!(r.payload_len, Some(5));
        let r: Request = serde_json::from_str(r#"{"v":1,"verb":"hello"}"#).unwrap();
        assert_eq!(r.payload_len, None);
    }

    #[test]
    fn requests_from_a_stream() {
        let mut input = io::Cursor::new(b"{\"v\":1,\"verb\":\"hello\",\"args\":{}}\nrest".to_vec());
        let (line, _) = handle_stream("x", &mut input);
        assert!(line.starts_with(b"{\"ok\":true"));
        let mut rest = String::new();
        input.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "rest", "the payload after the line is left unread");
        let (line, _) = handle_stream("x", &mut io::Cursor::new(b"not json\n".to_vec()));
        assert!(String::from_utf8_lossy(&line).contains("bad request"));
    }

    #[test]
    fn dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_793_613_600), "2026-11-02T10:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn session_ids() {
        let id = new_id();
        assert!(valid_session(&id), "{id}");
        assert!(!valid_session("s-"));
        assert!(!valid_session("s-12; rm"));
        assert!(!valid_session("x-12"));
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (raw, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(raw.as_bytes()), enc);
        }
    }

    #[test]
    fn png_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1280u32.to_be_bytes());
        png.extend_from_slice(&800u32.to_be_bytes());
        assert_eq!(png_size(&png), Some([1280, 800]));
        assert_eq!(png_size(b"GIF89a"), None);
    }

    #[test]
    fn replies_and_versions() {
        let req: Request = serde_json::from_str(r#"{"v":2,"verb":"hello","args":{}}"#).unwrap();
        let (line, payload) = answer("x", &req, &mut io::empty());
        let reply: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(reply["error"], "unsupported version");
        assert!(payload.is_empty() && line.ends_with(b"\n"));
        let req: Request = serde_json::from_str(r#"{"v":1,"verb":"hello"}"#).unwrap();
        let (line, _) = answer("x", &req, &mut io::empty());
        let reply: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!((reply["ok"].as_bool(), reply["result"]["os"].as_str()), (Some(true), Some("linux")));
    }

    #[test]
    fn launchers_quote_apps() {
        let l = launcher("s-1", Some(&["gnome-calculator".to_string(), "it's".to_string()]), false);
        assert!(l.contains("exec cage -- 'gnome-calculator' 'it'\\''s'"), "{l}");
        assert!(l.contains("export WLR_RENDERER=pixman; ") && !l.contains("gles2"), "{l}");
        let l = launcher("s-1", None, true);
        assert!(l.contains("sway -c /tmp/berth-gui-s-1/sway.conf"));
        assert!(l.contains("WLR_RENDERER=gles2"), "{l}");
    }
}
