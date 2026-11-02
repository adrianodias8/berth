//! GUI sessions in Linux machines, for `berth gui` and `berth machine run`
//! (docs/gui-protocol.md is the contract; docs/plans/gui-vms-agent-testing.md
//! the plan).
//!
//! A session is a Wayland compositor inside a machine, run as the machine's
//! user: headless sway for agent sessions (any number of apps, windows
//! managed over sway's IPC), headless cage for an app session (one app, full
//! screen), and for a desktop session sway or Hyprland on the VM's display
//! (card0, through seatd), which the host reads back as scanout 0. A desktop
//! is the DRM master of card0, so a VM has at most one. Each session has its
//! own runtime folder in the machine, /tmp/berth-gui-<id>, which is its
//! XDG_RUNTIME_DIR: its Wayland socket, the compositor's IPC socket, its own
//! session D-Bus (where at-spi2 runs, for the accessibility tree) and its
//! logs. So sessions never see each other's windows or input.
//!
//! Rendering is wlroots' GLES on the render node when the VM's GPU does 3D
//! and the machine has the node (install.rs binds it in), else pixman (CPU);
//! a compositor that fails to start on GLES is started again on pixman.
//! Screenshots are grim (wlr-screencopy), the keyboard is wtype
//! (virtual-keyboard), the pointer our own virtual-pointer client
//! (wayland.rs), the tree a python3 AT-SPI script (atspi.py), window
//! management swaymsg or hyprctl (hypr.rs).
//!
//! The agent keeps each session's record in /run/berth-gui/<machine>/ in the
//! VM; a session whose compositor is gone is dropped the next time it's
//! used or listed. Requests come over vsock (`gui <machine>`), from the
//! `machine gui` action (machines.rs, for `berth machine run`), or from
//! `berth-agent gui-local` for testing a build without replacing the agent.
//! Only with `"gui": true` in berth's config.json.

pub mod capture;
mod exec;
mod hypr;
pub mod input;
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
use install::Flavour;

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
    /// "agent" (sway), "app" (cage) or "desktop".
    kind: String,
    size: [u32; 2],
    apps: Vec<App>,
    #[serde(default)]
    next_app: u32,
    /// "gles2", "pixman" or (Hyprland without 3D) "software"; unknown for
    /// sessions an older agent started.
    #[serde(default)]
    renderer: Option<String>,
    /// A desktop's compositor: "sway" or "hyprland".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flavour: Option<String>,
}

/// What a session is, for the verbs that differ.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Agent,
    App,
    Desktop(Flavour),
}

impl Kind {
    fn of(s: &Session) -> Kind {
        match s.kind.as_str() {
            "app" => Kind::App,
            "desktop" => Kind::Desktop(s.flavour.as_deref().and_then(Flavour::parse).unwrap_or(Flavour::Sway)),
            _ => Kind::Agent,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Agent => "agent",
            Kind::App => "app",
            Kind::Desktop(_) => "desktop",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Renderer {
    Gles2,
    Pixman,
    /// Hyprland has no pixman: Mesa's software GL.
    Software,
}

impl Renderer {
    fn name(self) -> &'static str {
        match self {
            Renderer::Gles2 => "gles2",
            Renderer::Pixman => "pixman",
            Renderer::Software => "software",
        }
    }
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
/// payload; or, for a stream verb, the stream until either side closes.
pub fn serve(machine: &str, mut host: std::os::unix::net::UnixStream) -> io::Result<()> {
    let mut input = host.try_clone()?;
    let reply = if enabled() {
        match read_request(&mut input) {
            Ok(request) if STREAMS.contains(&request.verb.as_str()) => {
                let peer = host.try_clone()?;
                return stream(machine, &request, Box::new(input), &mut host, &move || {
                    let _ = peer.shutdown(std::net::Shutdown::Both);
                });
            }
            Ok(request) => answer(machine, &request, &mut input),
            Err(reply) => (reply, Vec::new()),
        }
    } else {
        (fail("Linux GUI sessions are off: set \"gui\": true in ~/.berth/config.json"), Vec::new())
    };
    host.write_all(&reply.0)?;
    host.write_all(&reply.1)
}

/// The request line; a bad one is its reply.
fn read_request(input: &mut dyn Read) -> Result<Request, Vec<u8>> {
    let line = read_line(input, 4 << 20).map_err(|err| fail(&format!("bad request: {err}")))?;
    serde_json::from_str::<Request>(&line).map_err(|err| fail(&format!("bad request: {err}")))
}

/// Read the request (and, for push, the tar stream after it) and answer.
fn handle_stream(machine: &str, input: &mut dyn Read) -> (Vec<u8>, Vec<u8>) {
    match read_request(input) {
        Ok(request) => answer(machine, &request, input),
        Err(reply) => (reply, Vec::new()),
    }
}

/// Verbs that keep their connection open (docs/gui-protocol.md, Streams).
const STREAMS: [&str; 2] = ["screen.stream", "input.stream"];

/// How long a stream's process has to end by itself once the host has
/// closed (input releases what it held) before it is stopped.
const STREAM_GRACE: Duration = Duration::from_secs(2);

/// A stream verb: the reply line, then the session's frame records to the
/// host (screen.stream) or the host's input lines to the session
/// (input.stream), until either side closes. What the process says on
/// stdout goes to the host either way (input's `{"error"}` lines). `end`
/// lets go of the host's connection, which a reader thread may be waiting
/// on.
fn stream(machine: &str, request: &Request, mut from_host: Box<dyn Read + Send>, to_host: &mut dyn Write, end: &dyn Fn()) -> io::Result<()> {
    let mut child = match start_stream(machine, request) {
        Ok(child) => child,
        Err(err) => {
            to_host.write_all(&fail(&err))?;
            end();
            return Ok(());
        }
    };
    to_host.write_all(&line_of(&Reply { ok: true, result: json!({}), error: None, payload_len: 0, hit: None }))?;
    to_host.flush()?;
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin(), child.stdout()) else {
        return Err(io::Error::other("the stream's process has no pipes"));
    };
    let stopper = child.stopper();
    std::thread::spawn(move || {
        let _ = io::copy(&mut from_host, &mut stdin);
        // The host has closed (or the stream is over): EOF for the process,
        // and a stop if it's still there a moment later.
        drop(stdin);
        std::thread::sleep(STREAM_GRACE);
        stopper.stop();
    });
    let sent = io::copy(&mut stdout, to_host).and_then(|_| to_host.flush());
    drop(stdout);
    if sent.is_ok() {
        // The process ended: say why, when it didn't say itself.
        let (code, stderr) = child.wait();
        let last = if code == Some(3) || stderr.contains("berth-session-ended") {
            Some("session ended".to_string())
        } else {
            (code != Some(0) && !stderr.is_empty()).then_some(stderr)
        };
        if let Some(error) = last {
            let _ = to_host.write_all(capture::error_line(&error).as_bytes()).and_then(|()| to_host.flush());
        }
    }
    // Stops the process if the host went away first.
    drop(child);
    end();
    Ok(())
}

/// The process behind a stream, as the machine's user in the session.
fn start_stream(machine: &str, request: &Request) -> Result<exec::Piped, String> {
    if request.v != 1 {
        return Err("unsupported version".into());
    }
    let m = Machine::open(machine).map_err(|e| e.to_string())?;
    let s = resolve(&m, request.session.as_deref())?;
    let rt = runtime(&s.session);
    // The session's copy of this binary, current: one from an agent before
    // this would take gui-capture for a request to start a whole agent.
    let me = std::fs::read("/proc/self/exe").map_err(|e| e.to_string())?;
    let refresh = format!("{}umask 077; cat > {rt}/.berth-gui-input.$$ && chmod 700 {rt}/.berth-gui-input.$$ && mv -f {rt}/.berth-gui-input.$$ {rt}/berth-gui-input", prelude(&s.session));
    m.run(&refresh, &me, SHORT).map_err(|e| if e.to_string().contains("berth-session-ended") { format!("session {} has ended", s.session) } else { e.to_string() })?;
    let (kind, command) = if request.verb == "screen.stream" {
        let args = if request.args.is_null() { json!({}) } else { request.args.clone() };
        ("cap", format!("gui-capture {}", quote(&args.to_string())))
    } else {
        ("inp", format!("gui-input {}x{}", s.size[0], s.size[1]))
    };
    // A unit per stream: several may watch one session.
    let unit = format!("berth-gui-{}-{kind}-{}", s.session, &new_id()[2..]);
    m.spawn_piped(&unit, &format!("{}exec $RT/berth-gui-input {command}", prelude(&s.session))).map_err(|e| format!("starting the stream: {e}"))
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
    // A stream runs on stdin and stdout until either closes.
    if let Ok(request) = serde_json::from_str::<Request>(json.trim())
        && STREAMS.contains(&request.verb.as_str())
    {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        return if stream(machine, &request, Box::new(io::stdin()), &mut out, &|| {}).is_ok() { 0 } else { 1 };
    }
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
    if STREAMS.contains(&verb) {
        return Err(format!("{verb} needs the gui channel (a stream)"));
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
/// through Xwayland) with their accessibility exposed over AT-SPI, and a
/// UTF-8 locale when the machine's is plain C (or unset).
const APP_ENV: &str = "case \"${LANG:-C}\" in C|POSIX) export LANG=C.UTF-8;; esac; export XDG_SESSION_TYPE=wayland GDK_BACKEND=wayland,x11 QT_QPA_PLATFORM='wayland;xcb' MOZ_ENABLE_WAYLAND=1 \
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
         for s in $RT/hypr/*/.socket.sock; do [ -S \"$s\" ] && d=${{s%/.socket.sock}} && export HYPRLAND_INSTANCE_SIGNATURE=${{d##*/}} && break; done; \
         [ -s $RT/display ] && export DISPLAY=$(cat $RT/display); \
         {APP_ENV}cd ~ 2>/dev/null || true; "
    )
}

/// sway's config: a headless output of `size`, or (a desktop) the display
/// in the mode its EDID gives.
fn sway_config(size: Option<[u32; 2]>) -> String {
    let output = match size {
        Some([w, h]) => format!("# berth: a headless agent session (agent/src/gui/mod.rs)\noutput HEADLESS-1 resolution {w}x{h} position 0 0 scale 1 bg #2e3440 solid_color\n"),
        None => "# berth: a desktop session on the VM's display (agent/src/gui/mod.rs)\noutput * bg #2e3440 solid_color\n".to_string(),
    };
    format!(
        "{output}default_border pixel 1\nfocus_on_window_activation focus\nxwayland enable\n\
         exec sh -c 'echo $DISPLAY > $XDG_RUNTIME_DIR/display'\n"
    )
}

/// What every launcher does: record its pid, start the session's own bus
/// (so AT-SPI works), then become the compositor.
fn launch_script(id: &str, env: &str, compositor: &str) -> String {
    let rt = runtime(id);
    format!(
        "RT={rt}; echo $$ > $RT/compositor.pid; export XDG_RUNTIME_DIR=$RT; {env}{APP_ENV}\
         if command -v dbus-daemon >/dev/null; then dbus-daemon --session --address=unix:path=$RT/bus --fork --nopidfile --print-pid > $RT/dbus.pid; fi; \
         export DBUS_SESSION_BUS_ADDRESS=unix:path=$RT/bus; \
         exec {compositor} >$RT/compositor.log 2>&1"
    )
}

/// wlroots' renderer: GLES on the render node, or pixman.
fn wlr_renderer(renderer: Renderer) -> String {
    match renderer {
        Renderer::Gles2 => format!("export WLR_RENDERER=gles2 WLR_RENDER_DRM_DEVICE={}; ", install::RENDER_NODE),
        Renderer::Pixman | Renderer::Software => "export WLR_RENDERER=pixman; ".to_string(),
    }
}

/// A headless compositor's launcher: sway, or cage for an app.
fn launcher(id: &str, app: Option<&[String]>, renderer: Renderer) -> String {
    let compositor = match app {
        Some(argv) => format!("cage -- {}", argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")),
        None => format!("sway -c {}/sway.conf", runtime(id)),
    };
    let env = format!("export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1; {}", wlr_renderer(renderer));
    launch_script(id, &env, &compositor)
}

/// A desktop's launcher: its compositor on the VM's display, as DRM master
/// through seatd. Input comes only from virtual devices (there are no
/// libinput ones in the machine).
fn launcher_desktop(id: &str, desktop: install::Desktop, renderer: Renderer) -> String {
    let seat = "export LIBSEAT_BACKEND=seatd; ";
    match desktop.flavour {
        Flavour::Sway => {
            let env = format!("{seat}export WLR_BACKENDS=drm WLR_DRM_DEVICES={} WLR_LIBINPUT_NO_DEVICES=1; {}", install::CARD_NODE, wlr_renderer(renderer));
            launch_script(id, &env, &format!("sway -c {}/sway.conf", runtime(id)))
        }
        Flavour::Hyprland => {
            // Hyprland needs GLES 3. Mesa's virgl reports 2.0 while the host
            // renderer is ANGLE's ES 3.0 (docs/LEARNINGS.md, S2): ask for 3.0.
            let software = match renderer {
                Renderer::Software => " LIBGL_ALWAYS_SOFTWARE=1",
                _ => " MESA_GLES_VERSION_OVERRIDE=3.0",
            };
            // Omarchy's config finds itself through OMARCHY_PATH (else
            // /usr/share/omarchy, where its own installer puts it), and its
            // scripts through PATH; berth clones it to ~/.local/share.
            let omarchy = "[ -d \"$HOME/.local/share/omarchy\" ] && export OMARCHY_PATH=\"$HOME/.local/share/omarchy\" PATH=\"$HOME/.local/share/omarchy/bin:$PATH\"; ";
            let env = format!("{seat}export AQ_DRM_DEVICES={} XDG_CURRENT_DESKTOP=Hyprland{software}; {omarchy}", install::CARD_NODE);
            // start-hyprland (Hyprland 0.57+) is its supported way in.
            // Without uwsm there's no systemd user manager for Omarchy's
            // `uwsm-app -- app` launches (its terminal, its menus' apps), so
            // a stand-in first on PATH runs the app itself.
            let (env, run) = if desktop.uwsm {
                (env, "uwsm start -- hyprland.desktop")
            } else {
                (format!("{env}{UWSM_APP_SHIM}"), "$(command -v start-hyprland || echo Hyprland)")
            };
            launch_script(id, &env, run)
        }
    }
}

/// `uwsm-app [options] -- cmd…` as `cmd…` (`$RT` is the session's runtime
/// folder); without `--`, the arguments run as they are.
const UWSM_APP_SHIM: &str = "mkdir -p $RT/bin; printf '%s\\n' '#!/bin/sh' \
     'for a; do if [ \"$a\" = -- ]; then while [ \"$1\" != -- ]; do shift; done; shift; break; fi; done' \
     'exec \"$@\"' > $RT/bin/uwsm-app; chmod +x $RT/bin/uwsm-app; export PATH=\"$RT/bin:$PATH\"; ";

/// The script that waits for a compositor: up when its Wayland socket and
/// its IPC socket (sway's or Hyprland's; cage has none) are there, failed
/// when it exits first or after `tries` tenths of a second.
fn ready_script(id: &str, kind: Kind, tries: u32) -> String {
    let ipc = match kind {
        Kind::App => "true",
        Kind::Agent | Kind::Desktop(Flavour::Sway) => "ls $RT/sway-ipc.*.sock >/dev/null 2>&1",
        Kind::Desktop(Flavour::Hyprland) => "ls $RT/hypr/*/.socket.sock >/dev/null 2>&1",
    };
    format!(
        "RT={}; for i in $(seq {tries}); do \
           if ls $RT/wayland-[0-9] >/dev/null 2>&1 && {ipc}; then exit 0; fi; \
           pid=$(cat $RT/compositor.pid 2>/dev/null); if [ -n \"$pid\" ] && ! kill -0 $pid 2>/dev/null; then break; fi; sleep 0.1; done; \
         tail -n 5 $RT/compositor.log >&2; exit 1",
        runtime(id)
    )
}

fn missing_tools_message(missing: &str) -> String {
    format!(
        "GUI tools missing in this machine ({}). Install them with: berth machine create <name> --gui (new machines), \
         or in this one: sway cage grim wtype dbus at-spi2-core python3-gi (python-gobject on Arch, python3-gobject on Fedora) \
         and the Atspi typelib (gir1.2-atspi-2.0 on Debian/Ubuntu); a desktop also needs seatd, and Mesa for the GPU",
        missing.trim()
    )
}

/// The VM's one desktop: who has card0, as "<machine> <session> <since>".
const DESKTOP_CLAIM: &str = "/run/berth-gui-desktop";
/// A claim this young counts even before its session is saved.
const CLAIM_GRACE_S: u64 = 120;

fn epoch_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn open_claim(path: &str) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
    unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
    Ok(file)
}

/// Claim the VM's desktop for `machine`'s session `id`, unless another
/// session that's still alive (`alive(machine, session)`) has it, across
/// machines.
fn claim_desktop(path: &str, machine: &str, id: &str, alive: impl Fn(&str, &str) -> bool) -> Result<(), String> {
    use std::io::Seek;
    let mut file = open_claim(path).map_err(|e| format!("{path}: {e}"))?;
    let mut held = String::new();
    let _ = file.read_to_string(&mut held);
    let mut words = held.split_whitespace();
    if let (Some(m), Some(s)) = (words.next(), words.next()) {
        let since: u64 = words.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        if (m, s) != (machine, id) && (epoch_s().saturating_sub(since) < CLAIM_GRACE_S || alive(m, s)) {
            return Err(format!(
                "this VM already has a desktop session: {s} in {m} (it drives the VM's display, so there's one at a time); end it first"
            ));
        }
    }
    file.set_len(0).and_then(|_| file.rewind()).and_then(|_| writeln!(file, "{machine} {id} {}", epoch_s())).map_err(|e| format!("{path}: {e}"))
}

/// Give the desktop back, if `machine`'s session `id` has it.
fn release_desktop(path: &str, machine: &str, id: &str) {
    let Ok(mut file) = open_claim(path) else { return };
    let mut held = String::new();
    let _ = file.read_to_string(&mut held);
    let mut words = held.split_whitespace();
    if words.next() == Some(machine) && words.next() == Some(id) {
        let _ = file.set_len(0);
    }
}

/// Whether `machine`'s session `id` still has a compositor (for a claim
/// made from another machine). Its record isn't asked for: a desktop that's
/// still starting has none yet, and holds card0 all the same.
fn session_alive(machine: &str, id: &str) -> bool {
    valid_session(id)
        && Machine::open(machine).is_ok_and(|m| {
            m.run_text(&format!("p=$(cat {}/compositor.pid 2>/dev/null); [ -n \"$p\" ] && kill -0 $p 2>/dev/null && echo alive; true", runtime(id)), SHORT)
                .is_ok_and(|o| o.trim() == "alive")
        })
}

/// The session session.new asks for.
#[derive(Debug, PartialEq)]
enum Wanted {
    Agent,
    App(Vec<String>),
    Desktop(Option<Flavour>),
}

/// `kind` (agent, app or desktop; by default app with an `app`, else agent),
/// `app` and `flavour` (hyprland or sway; by default whichever is ready,
/// Hyprland first).
fn wanted(args: &Value) -> Result<Wanted, String> {
    let app: Option<Vec<String>> = args.get("app").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
    if app.as_ref().is_some_and(|a| a.is_empty()) {
        return Err("session.new: app is an empty command".into());
    }
    let flavour = match args.get("flavour").and_then(Value::as_str) {
        None | Some("" | "auto") => None,
        Some(f) => Some(Flavour::parse(f).ok_or_else(|| format!("session.new: flavour {f:?}: hyprland or sway"))?),
    };
    let kind = args.get("kind").and_then(Value::as_str).filter(|k| !k.is_empty());
    if flavour.is_some() && kind != Some("desktop") {
        return Err("session.new: flavour is for desktop sessions".into());
    }
    match (kind, app) {
        (None | Some("app"), Some(app)) => Ok(Wanted::App(app)),
        (Some("app"), None) => Err("session.new: an app session needs app, its command".into()),
        (None | Some("agent"), None) => Ok(Wanted::Agent),
        (Some("desktop"), None) => Ok(Wanted::Desktop(flavour)),
        (Some(k @ ("agent" | "desktop")), Some(_)) => Err(format!("session.new: app is for app sessions, not {k} ones (open apps in them)")),
        (Some(other), _) => Err(format!("session.new: kind {other:?}: agent, app or desktop")),
    }
}

/// What the machine has, in one call: a `missing <tool>` line for each
/// tool the session needs and lacks, `render` and `card0` for the GPU nodes
/// it has, and for a desktop what each flavour lacks (`desktop_probe`).
fn probe_script(wanted: &Wanted) -> String {
    let (compositor, desktop) = match wanted {
        Wanted::Agent => ("sway swaymsg ", String::new()),
        Wanted::App(_) => ("cage ", String::new()),
        Wanted::Desktop(_) => ("", format!("{}; ", install::desktop_probe())),
    };
    format!(
        "for t in {compositor}grim wtype; do command -v $t >/dev/null || echo \"missing $t\"; done; \
         [ -e {} ] && echo render; [ -e {} ] && echo card0; {desktop}true",
        install::RENDER_NODE,
        install::CARD_NODE
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
    let wanted = wanted(args)?;
    let size = args
        .get("size")
        .and_then(Value::as_array)
        .and_then(|s| Some([s.first()?.as_u64()? as u32, s.get(1)?.as_u64()? as u32]))
        .filter(|[w, h]| (320..=7680).contains(w) && (240..=4320).contains(h))
        .unwrap_or(DEFAULT_SIZE);
    let probe = m.run_text(&probe_script(&wanted), SHORT).map_err(|e| e.to_string())?;
    let missing: Vec<&str> = probe.lines().filter_map(|l| l.strip_prefix("missing ")).collect();
    if !missing.is_empty() {
        return Err(missing_tools_message(&missing.join(" ")));
    }
    let has = |word: &str| probe.lines().any(|l| l.trim() == word);
    let (kind, app, desktop) = match wanted {
        Wanted::Agent => (Kind::Agent, None, None),
        Wanted::App(app) => (Kind::App, Some(app), None),
        Wanted::Desktop(flavour) => {
            if !has("card0") {
                return Err(format!(
                    "a desktop session needs the VM's display, and {} has no {}: turn on \"gui\" and restart berth (the machine gets it when the agent starts)",
                    m.name,
                    install::CARD_NODE
                ));
            }
            let desktop = install::desktop_ready(flavour, &probe)?;
            (Kind::Desktop(desktop.flavour), None, Some(desktop))
        }
    };
    // gles2 only on a GPU that does 3D and is bound into the machine.
    let gles2 = has("render") && install::gpu_has_3d();
    let mut renderer = match (gles2, kind) {
        (true, _) => Renderer::Gles2,
        (false, Kind::Desktop(Flavour::Hyprland)) => Renderer::Software,
        (false, _) => Renderer::Pixman,
    };
    let id = new_id();
    let rt = runtime(&id);
    let mut session = Session {
        session: id.clone(),
        agent,
        since: now(),
        kind: kind.name().into(),
        size,
        apps: app.iter().map(|a| App { app: a[0].clone(), pid: 0 }).collect(),
        next_app: 0,
        renderer: None,
        flavour: desktop.map(|d| d.flavour.as_str().to_string()),
    };
    let ctx = Ctx { m, s: session.clone() };
    let fail = |err: String| {
        let _ = ctx.end();
        Err(err)
    };
    // The runtime folder, sway's config (Hyprland reads its own), and this
    // binary for pointer input (fed over stdin: it's the user writing in
    // their own folder).
    let me = std::fs::read("/proc/self/exe").map_err(|e| e.to_string())?;
    // The GPU's nodes are root:render and root:video in the machine; the
    // user's compositor needs them.
    if renderer == Renderer::Gles2 || desktop.is_some() {
        let _ = m.run_root(&install::gpu_groups_script(&m.user), SHORT);
    }
    if desktop.is_some() {
        claim_desktop(DESKTOP_CLAIM, &m.name, &id, session_alive)?;
        if let Err(e) = start_seatd(m) {
            return fail(format!("starting seatd: {e}"));
        }
    }
    let sway_conf = match kind {
        Kind::Desktop(Flavour::Hyprland) => String::new(),
        Kind::Desktop(Flavour::Sway) => format!("; printf '%s' {} > {rt}/sway.conf", quote(&sway_config(None))),
        _ => format!("; printf '%s' {} > {rt}/sway.conf", quote(&sway_config(Some(size)))),
    };
    let setup = format!("set -e; umask 077; rm -rf {rt}; mkdir -m 700 {rt}; cat > {rt}/berth-gui-input; chmod 700 {rt}/berth-gui-input{sway_conf}");
    if let Err(e) = m.run(&setup, &me, SHORT) {
        return fail(format!("setting up the session: {e}"));
    }
    let launch = |renderer: Renderer| match desktop {
        Some(desktop) => launcher_desktop(&id, desktop, renderer),
        None => launcher(&id, app.as_deref(), renderer),
    };
    // A desktop takes longer: DRM, seatd, and Hyprland's own start.
    let (tries, wait) = if desktop.is_some() { (250, Duration::from_secs(40)) } else { (100, SHORT) };
    let mut unit = format!("berth-gui-{id}");
    let mut started = m.spawn(&unit, &launch(renderer)).map_err(|e| format!("starting the compositor: {e}"));
    if started.is_ok() {
        started = m.run(&ready_script(&id, kind, tries), &[], wait).map(|_| ()).map_err(|e| e.to_string());
    }
    // GLES that doesn't come up (a GPU without 3D after all, a missing
    // driver): once more on pixman, keeping the first log. Hyprland has no
    // pixman renderer.
    if let Err(err) = &started
        && renderer == Renderer::Gles2
        && kind != Kind::Desktop(Flavour::Hyprland)
    {
        eprintln!("berth-agent: gui: {}: {id}: the compositor didn't start on gles2 ({err}); trying pixman", m.name);
        m.stop_units(&unit);
        let reset = format!(
            "RT={rt}; for f in compositor dbus; do p=$(cat $RT/$f.pid 2>/dev/null); [ -n \"$p\" ] && kill -9 $p 2>/dev/null; done; \
             mv -f $RT/compositor.log $RT/compositor-gles2.log 2>/dev/null; rm -f $RT/wayland-* $RT/sway-ipc.* $RT/compositor.pid; true"
        );
        let _ = m.run(&reset, &[], SHORT);
        renderer = Renderer::Pixman;
        unit = format!("berth-gui-{id}-pixman");
        started = m.spawn(&unit, &launch(renderer)).map_err(|e| format!("starting the compositor: {e}"));
        if started.is_ok() {
            started = m.run(&ready_script(&id, kind, tries), &[], wait).map(|_| ()).map_err(|e| e.to_string());
        }
    }
    if let Err(err) = started {
        return fail(format!("the compositor didn't start: {err}"));
    }
    session.renderer = Some(renderer.name().into());
    // The real screen size (cage takes the headless default), and a check
    // that capture works.
    if let Ok(png) = ctx.run(&format!("{}grim -t png -l 0 -", prelude(&id)), Duration::from_secs(30))
        && let Some(size) = png_size(&png)
    {
        session.size = size;
    }
    // Toolkits that wait for a screen reader before exposing anything.
    let _ = ctx.atspi(&json!({"mode": "enable"}));
    if let Err(e) = save(&m.name, &session) {
        return fail(e.to_string());
    }
    let mut reply = describe(&session);
    reply["session"] = json!(id);
    Ok(Done::of(reply))
}

/// What session.new and session.list say about a session's screen: its
/// kind and renderer, and for a desktop its flavour and the host scanout it
/// shows on.
fn describe(s: &Session) -> Value {
    let mut out = json!({"size": s.size, "kind": s.kind, "renderer": s.renderer});
    if let Kind::Desktop(flavour) = Kind::of(s) {
        out["flavour"] = json!(flavour.as_str());
        out["scanout"] = json!(0);
    }
    out
}

/// The machine's seatd, which hands a desktop's compositor card0: the seat
/// group set up, then seatd started unless one runs, and waited for.
fn start_seatd(m: &Machine) -> Result<(), String> {
    let state = m.run_root(&install::seat_script(&m.user), SHORT).map_err(|e| e.to_string())?;
    if state.trim() != "running" {
        m.spawn_root("berth-seatd", &install::seatd_script(&m.user)).map_err(|e| e.to_string())?;
    }
    m.run_text(
        &format!("for i in $(seq 50); do [ -S {} ] && exit 0; sleep 0.1; done; echo 'seatd did not start' >&2; exit 1", install::SEATD_SOCK),
        SHORT,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
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
        let mut entry = describe(&s);
        for (k, v) in [("session", json!(s.session)), ("agent", json!(s.agent)), ("since", json!(s.since)), ("apps", json!(apps))] {
            entry[k] = v;
        }
        out.push(entry);
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

    fn kind(&self) -> Kind {
        Kind::of(&self.s)
    }

    /// Stop everything the session started and forget it (a desktop also
    /// gives the VM's display back; the machine's seatd stays).
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
        if matches!(self.kind(), Kind::Desktop(_)) {
            release_desktop(DESKTOP_CLAIM, &self.m.name, self.id());
        }
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

    /// The windows the compositor manages: sway's, or Hyprland's.
    fn managed_windows(&self) -> Result<Vec<sway::Window>, String> {
        if self.kind() != Kind::Desktop(Flavour::Hyprland) {
            return self.sway_windows();
        }
        let out = self.sh_text("hyprctl -j clients")?;
        let clients: Value = serde_json::from_str(&out).map_err(|e| format!("hyprctl: {e}"))?;
        Ok(hypr::windows(&clients))
    }

    /// The session's windows. An app session has one: its app, full screen.
    fn windows(&self) -> Result<Vec<sway::Window>, String> {
        if self.kind() != Kind::App {
            return self.managed_windows();
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
        if self.kind() == Kind::App {
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
        let before: Vec<i64> = self.managed_windows()?.iter().map(|w| w.id).collect();
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
            let windows = self.managed_windows()?;
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
        match self.kind() {
            Kind::App => {
                sway::find(&self.windows()?, app)?;
            }
            Kind::Desktop(Flavour::Hyprland) => {
                let windows = self.managed_windows()?;
                let w = sway::find(&windows, app)?;
                hypr::dispatch_result(&self.sh_text(&hypr::focus_cmd(w.id))?)?;
            }
            Kind::Agent | Kind::Desktop(Flavour::Sway) => {
                let windows = self.sway_windows()?;
                let w = sway::find(&windows, app)?;
                self.sh(&format!("swaymsg -q '[con_id={}]' focus", w.id), SHORT)?;
            }
        }
        Ok(Done::of(json!({})))
    }

    fn quit(&self, app: &str) -> Result<Done, String> {
        if self.kind() != Kind::App {
            let windows = self.managed_windows()?;
            let w = sway::find(&windows, app)?;
            if self.kind() == Kind::Desktop(Flavour::Hyprland) {
                hypr::dispatch_result(&self.sh_text(&hypr::close_cmd(w.id))?)?;
            } else {
                self.sh(&format!("swaymsg -q '[con_id={}]' kill", w.id), SHORT)?;
            }
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
    fn streams_need_the_channel() {
        // Through answer (the machine action, a plain request) a stream verb
        // is refused before any machine is looked at.
        for verb in STREAMS {
            let req: Request = serde_json::from_str(&format!(r#"{{"v":1,"verb":"{verb}"}}"#)).unwrap();
            let (line, _) = answer("no-such-machine", &req, &mut io::empty());
            let reply: Value = serde_json::from_slice(&line).unwrap();
            assert_eq!(reply["error"], format!("{verb} needs the gui channel (a stream)"));
        }
    }

    #[test]
    fn launchers_quote_apps() {
        let l = launcher("s-1", Some(&["gnome-calculator".to_string(), "it's".to_string()]), Renderer::Pixman);
        assert!(l.contains("exec cage -- 'gnome-calculator' 'it'\\''s'"), "{l}");
        assert!(l.contains("export WLR_RENDERER=pixman; ") && !l.contains("gles2") && !l.contains("WLR_RENDER_DRM_DEVICE"), "{l}");
        assert!(l.contains("WLR_BACKENDS=headless"), "{l}");
        let l = launcher("s-1", None, Renderer::Gles2);
        assert!(l.contains("sway -c /tmp/berth-gui-s-1/sway.conf"));
        assert!(l.contains("WLR_RENDERER=gles2 WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128"), "{l}");
        assert!(l.contains("dbus-daemon --session --address=unix:path=$RT/bus"), "{l}");
    }

    #[test]
    fn uwsm_app_shim_runs_the_app() {
        let rt = std::env::temp_dir().join(format!("berth-uwsm-shim-{}", std::process::id()));
        std::fs::create_dir_all(&rt).unwrap();
        let run = |args: &str| {
            let script = format!("RT={}; {UWSM_APP_SHIM} uwsm-app {args}", rt.display());
            let out = std::process::Command::new("sh").arg("-c").arg(script).output().unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(run("-- echo a -- 'b c'"), "a -- b c\n");
        assert_eq!(run("-s a -t scope -- echo hi"), "hi\n");
        assert_eq!(run("echo plain"), "plain\n");
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[test]
    fn desktop_launchers() {
        let sway = install::Desktop { flavour: Flavour::Sway, uwsm: false };
        let l = launcher_desktop("s-1", sway, Renderer::Gles2);
        assert!(l.contains("WLR_BACKENDS=drm WLR_DRM_DEVICES=/dev/dri/card0 WLR_LIBINPUT_NO_DEVICES=1"), "{l}");
        assert!(l.contains("WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128") && l.contains("LIBSEAT_BACKEND=seatd"), "{l}");
        assert!(l.contains("exec sway -c /tmp/berth-gui-s-1/sway.conf") && !l.contains("headless"), "{l}");
        let l = launcher_desktop("s-1", sway, Renderer::Pixman);
        assert!(l.contains("WLR_RENDERER=pixman") && !l.contains("WLR_RENDER_DRM_DEVICE"), "{l}");

        for uwsm in [false, true] {
            let l = launcher_desktop("s-1", install::Desktop { flavour: Flavour::Hyprland, uwsm }, Renderer::Gles2);
            assert!(l.contains("AQ_DRM_DEVICES=/dev/dri/card0") && l.contains("LIBSEAT_BACKEND=seatd"), "{l}");
            assert!(l.contains("XDG_SESSION_TYPE=wayland") && l.contains("XDG_CURRENT_DESKTOP=Hyprland"), "{l}");
            assert!(!l.contains("WLR_BACKENDS") && !l.contains("LIBGL_ALWAYS_SOFTWARE"), "{l}");
            assert!(l.contains("dbus-daemon --session"), "{l}");
            let exec = if uwsm { "exec uwsm start -- hyprland.desktop >" } else { "exec $(command -v start-hyprland || echo Hyprland) >" };
            assert!(l.contains(exec), "{l}");
        }
        let l = launcher_desktop("s-1", install::Desktop { flavour: Flavour::Hyprland, uwsm: false }, Renderer::Software);
        assert!(l.contains("LIBGL_ALWAYS_SOFTWARE=1"), "{l}");
        assert!(l.contains("$RT/bin/uwsm-app") && l.find("uwsm-app").unwrap() > l.find("omarchy/bin").unwrap(), "the stand-in comes first on PATH: {l}");
        let l = launcher_desktop("s-1", install::Desktop { flavour: Flavour::Hyprland, uwsm: true }, Renderer::Gles2);
        assert!(!l.contains("$RT/bin/uwsm-app"), "uwsm's own: {l}");
    }

    #[test]
    fn sway_configs() {
        assert!(sway_config(Some([1280, 800])).contains("output HEADLESS-1 resolution 1280x800"));
        let desktop = sway_config(None);
        assert!(desktop.contains("output * bg") && !desktop.contains("HEADLESS") && !desktop.contains("resolution"), "{desktop}");
    }

    #[test]
    fn the_prelude_finds_hyprland() {
        let p = prelude("s-1");
        assert!(p.contains("$RT/hypr/*/.socket.sock") && p.contains("export HYPRLAND_INSTANCE_SIGNATURE=${d##*/}"), "{p}");
        // The signature is the socket's folder name.
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", "s=/tmp/x/hypr/abc_123_456/.socket.sock; d=${s%/.socket.sock}; echo ${d##*/}"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "abc_123_456");
    }

    #[test]
    fn session_kinds() {
        let app = |a: &str| json!({"app": [a]});
        assert_eq!(wanted(&json!({})), Ok(Wanted::Agent));
        assert_eq!(wanted(&json!({"kind": "agent"})), Ok(Wanted::Agent));
        assert_eq!(wanted(&app("foot")), Ok(Wanted::App(vec!["foot".into()])));
        assert_eq!(wanted(&json!({"kind": "desktop"})), Ok(Wanted::Desktop(None)));
        assert_eq!(wanted(&json!({"kind": "desktop", "flavour": "auto"})), Ok(Wanted::Desktop(None)));
        assert_eq!(wanted(&json!({"kind": "desktop", "flavour": "sway"})), Ok(Wanted::Desktop(Some(Flavour::Sway))));
        assert_eq!(wanted(&json!({"kind": "desktop", "flavour": "hyprland"})), Ok(Wanted::Desktop(Some(Flavour::Hyprland))));
        assert!(wanted(&json!({"kind": "desktop", "flavour": "kde"})).unwrap_err().contains("hyprland or sway"));
        assert!(wanted(&json!({"flavour": "sway"})).unwrap_err().contains("desktop sessions"));
        assert!(wanted(&json!({"kind": "app"})).unwrap_err().contains("needs app"));
        assert!(wanted(&json!({"kind": "desktop", "app": ["foot"]})).is_err());
        assert!(wanted(&json!({"kind": "game"})).unwrap_err().contains("agent, app or desktop"));
        assert!(wanted(&json!({"app": []})).is_err());
    }

    #[test]
    fn probes_run_for_every_kind() {
        // An empty PATH: every tool is missing, and the script still runs.
        let run = |w: &Wanted| {
            let out = std::process::Command::new("/bin/sh").args(["-c", &probe_script(w)]).env("PATH", "/nonexistent").output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let agent = run(&Wanted::Agent);
        assert!(agent.contains("missing sway\nmissing swaymsg\nmissing grim\nmissing wtype\n"), "{agent}");
        let app = run(&Wanted::App(vec!["foot".into()]));
        assert!(app.starts_with("missing cage\nmissing grim\n") && !app.contains("desktop"), "{app}");
        let desktop = run(&Wanted::Desktop(None));
        assert!(desktop.starts_with("missing grim\n") && desktop.contains("desktop sway missing sway swaymsg seatd"), "{desktop}");
    }

    fn session(kind: &str, flavour: Option<&str>) -> Session {
        Session {
            session: "s-1".into(),
            agent: String::new(),
            since: String::new(),
            kind: kind.into(),
            size: [1920, 1080],
            apps: Vec::new(),
            next_app: 0,
            renderer: Some("gles2".into()),
            flavour: flavour.map(str::to_string),
        }
    }

    #[test]
    fn kinds_and_replies() {
        assert_eq!(Kind::of(&session("agent", None)), Kind::Agent);
        assert_eq!(Kind::of(&session("app", None)), Kind::App);
        assert_eq!(Kind::of(&session("desktop", Some("hyprland"))), Kind::Desktop(Flavour::Hyprland));
        assert_eq!(Kind::of(&session("desktop", Some("sway"))), Kind::Desktop(Flavour::Sway));
        let d = describe(&session("desktop", Some("hyprland")));
        assert_eq!(d, json!({"size": [1920, 1080], "kind": "desktop", "renderer": "gles2", "flavour": "hyprland", "scanout": 0}));
        let a = describe(&session("agent", None));
        assert!(a.get("scanout").is_none() && a["renderer"] == "gles2");
        // Records from an older agent have no renderer.
        let old: Session = serde_json::from_str(r#"{"session":"s-1","agent":"","since":"","kind":"agent","size":[1,1],"apps":[]}"#).unwrap();
        assert_eq!((old.renderer, old.flavour), (None, None));
    }

    #[test]
    fn one_desktop_per_vm() {
        let path = std::env::temp_dir().join(format!("berth-desktop-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        claim_desktop(path, "dev", "s-1", |_, _| panic!("nothing to check")).unwrap();
        // Another session, in this machine or another, is refused while the
        // first lives (or is young enough to be starting).
        let err = claim_desktop(path, "desk", "s-2", |_, _| false).unwrap_err();
        assert!(err.contains("s-1 in dev"), "{err}");
        // The same session again is fine.
        claim_desktop(path, "dev", "s-1", |_, _| false).unwrap();
        // An old claim whose session is gone is taken over.
        std::fs::write(path, "dev s-1 100\n").unwrap();
        assert!(claim_desktop(path, "desk", "s-2", |m, s| (m, s) == ("dev", "s-1")).is_err());
        claim_desktop(path, "desk", "s-2", |_, _| false).unwrap();
        assert!(std::fs::read_to_string(path).unwrap().starts_with("desk s-2 "));
        // Only its holder gives it back.
        release_desktop(path, "dev", "s-1");
        assert!(std::fs::read_to_string(path).unwrap().starts_with("desk s-2 "));
        release_desktop(path, "desk", "s-2");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "");
        claim_desktop(path, "dev", "s-3", |_, _| panic!("no claim to check")).unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn readiness_per_kind() {
        use std::os::unix::net::UnixListener;
        let id = format!("s-{:x}", std::process::id());
        let rt = runtime(&id);
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(format!("{rt}/hypr/sig")).unwrap();
        let ready = |kind: Kind| std::process::Command::new("/bin/sh").args(["-c", &ready_script(&id, kind, 3)]).output().unwrap().status.success();
        let kinds = [Kind::App, Kind::Agent, Kind::Desktop(Flavour::Sway), Kind::Desktop(Flavour::Hyprland)];
        assert!(kinds.iter().all(|&k| !ready(k)), "nothing is up yet");
        let _wayland = UnixListener::bind(format!("{rt}/wayland-1")).unwrap();
        assert!(ready(Kind::App) && !ready(Kind::Agent) && !ready(Kind::Desktop(Flavour::Hyprland)));
        let _sway = UnixListener::bind(format!("{rt}/sway-ipc.1000.42.sock")).unwrap();
        assert!(ready(Kind::Agent) && ready(Kind::Desktop(Flavour::Sway)) && !ready(Kind::Desktop(Flavour::Hyprland)));
        let _hypr = UnixListener::bind(format!("{rt}/hypr/sig/.socket.sock")).unwrap();
        assert!(ready(Kind::Desktop(Flavour::Hyprland)));
        // A compositor that exited fails at once, with its log.
        std::fs::remove_file(format!("{rt}/wayland-1")).unwrap();
        std::fs::write(format!("{rt}/compositor.pid"), "999999\n").unwrap();
        std::fs::write(format!("{rt}/compositor.log"), "no GPU\n").unwrap();
        let out = std::process::Command::new("/bin/sh").args(["-c", &ready_script(&id, Kind::App, 100)]).output().unwrap();
        assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("no GPU"));
        let _ = std::fs::remove_dir_all(&rt);
    }
}
