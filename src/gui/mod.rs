//! `berth gui`: drive apps in a macOS or Linux guest for testing: push an
//! app, open it, take screenshots, click by label, type, read the
//! accessibility tree. Speaks docs/gui-protocol.md to the guest's GUI agent
//! (proto.rs), logs every call with who made it (gui_log.rs), and keeps the
//! host side of sessions (sessions.rs; macOS clones through macos.rs).
//! `berth mcp` (mcp.rs) offers the same actions as MCP tools through
//! `perform`. The workflow for agents is docs/agents/SKILL.md.
//!
//! Off unless ~/.berth/config.json has "mac": true (macOS targets) or
//! "gui": true (Linux targets).

pub mod macos;
pub mod proto;
pub mod sessions;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::gui_log::{self, Line, Who};
use crate::paths::Paths;
pub use proto::Target;

#[derive(clap::Args)]
pub struct GuiArgs {
    /// mac:<vm> or linux:<machine> (default: $BERTH_GUI_TARGET, else the
    /// session's)
    #[arg(long, global = true)]
    pub target: Option<String>,
    /// The session to act in (default: $BERTH_GUI_SESSION)
    #[arg(long, global = true)]
    pub session: Option<String>,
    /// Print JSON instead of text
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: GuiCmd,
}

#[derive(clap::Subcommand)]
pub enum GuiCmd {
    /// Copy a file, folder or .app into the guest (to ~/Pushed there)
    Push {
        path: PathBuf,
        /// A folder in the guest instead
        #[arg(long)]
        to: Option<String>,
    },
    /// Open an app (a path in the guest, or a name) and wait for its window
    Open {
        app: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Seconds to wait for a window
        #[arg(long, default_value_t = 20)]
        wait: u64,
    },
    /// Save a PNG of the screen (or one app's window) and print its path
    Screenshot {
        /// Only this app's window
        #[arg(long)]
        window: Option<String>,
        /// Also copy it here
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Click a button or other element by its label, or a point: x,y
    Click {
        /// "Save" or 120,340 (guest screen points)
        at: String,
        #[arg(long)]
        right: bool,
        #[arg(long)]
        double: bool,
    },
    /// Type text into the focused element
    Type { text: String },
    /// Press a key combination: cmd+s, ctrl+shift+t, return, escape
    Key { combo: String },
    /// Scroll at a point (x,y) by --dy and --dx points
    Scroll {
        at: String,
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        dy: f64,
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        dx: f64,
    },
    /// Drag from one point to another: 10,10 200,200
    Drag { from: String, to: String },
    /// The accessibility tree (of one app, or of the focused one)
    Tree {
        #[arg(long)]
        app: Option<String>,
        #[arg(long, default_value_t = 12)]
        depth: u32,
    },
    /// Wait until an element with a label appears, or until the screen is idle
    Wait {
        label: Option<String>,
        #[arg(long, conflicts_with = "label")]
        idle: bool,
        /// Seconds
        #[arg(long, default_value_t = 20)]
        timeout: u64,
    },
    /// List windows
    Windows,
    /// Bring an app to the front
    Focus { app: String },
    /// Quit an app
    Quit { app: String },
    /// Write a note in the activity log (what you're about to check, say)
    Note { text: String },
    /// Sessions: your own screen (Linux) or VM (macOS, --clone)
    Session {
        #[command(subcommand)]
        action: SessionCmd,
    },
}

#[derive(clap::Subcommand)]
pub enum SessionCmd {
    /// Start a session; prints `export BERTH_GUI_SESSION=…`
    New {
        /// macOS: a throwaway copy of the target VM, deleted when the
        /// session ends (waits while 2 macOS VMs run)
        #[arg(long)]
        clone: bool,
    },
    /// End a session (the current one by default)
    End { session: Option<String> },
    /// List sessions
    List,
}

/// One thing `berth gui` or `berth mcp` does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Push { path: PathBuf, to: Option<String> },
    /// A verb the guest carries out, with its protocol args.
    Guest { verb: &'static str, args: Value },
    Note { text: String },
    SessionNew { clone: bool },
    SessionEnd { session: Option<String> },
    SessionList,
}

impl Action {
    pub fn verb(&self) -> &'static str {
        match self {
            Action::Push { .. } => "push",
            Action::Guest { verb, .. } => verb,
            Action::Note { .. } => "note",
            Action::SessionNew { .. } => "session.new",
            Action::SessionEnd { .. } => "session.end",
            Action::SessionList => "session.list",
        }
    }

    fn log_args(&self) -> Value {
        match self {
            Action::Push { path, to } => json!({"path": path, "dest": to}),
            Action::Guest { args, .. } => args.clone(),
            Action::Note { text } => json!({"text": text}),
            Action::SessionNew { clone } => json!({"clone": clone}),
            Action::SessionEnd { session } => json!({"session": session}),
            Action::SessionList => json!({}),
        }
    }
}

/// Where a pointer action goes.
#[derive(Debug, Clone, PartialEq)]
pub enum At {
    Point(f64, f64),
    Label(String),
}

/// `120,340` is a point, anything else a label.
pub fn parse_at(s: &str) -> Result<At> {
    if let Some((x, y)) = s.split_once(',')
        && let (Ok(x), Ok(y)) = (x.trim().parse::<f64>(), y.trim().parse::<f64>())
        && x.is_finite()
        && y.is_finite()
    {
        return Ok(At::Point(x, y));
    }
    if s.trim().is_empty() {
        bail!("an empty label");
    }
    Ok(At::Label(s.to_string()))
}

pub fn parse_point(s: &str) -> Result<(f64, f64)> {
    match parse_at(s)? {
        At::Point(x, y) => Ok((x, y)),
        At::Label(_) => bail!("{s:?} isn't a point (x,y)"),
    }
}

/// Whole numbers as integers: guests needn't care, logs read better.
pub fn num(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 1e15 { json!(x as i64) } else { json!(x) }
}

pub fn click_args(at: &At, right: bool, count: u32) -> Value {
    let mut args = match at {
        At::Point(x, y) => json!({"x": num(*x), "y": num(*y)}),
        At::Label(label) => json!({"label": label}),
    };
    args["button"] = json!(if right { "right" } else { "left" });
    args["count"] = json!(count);
    args
}

/// The action a command line asks for.
pub fn action(cmd: GuiCmd) -> Result<Action> {
    let guest = |verb, args| Ok(Action::Guest { verb, args });
    match cmd {
        GuiCmd::Push { path, to } => Ok(Action::Push { path, to }),
        GuiCmd::Open { app, args, wait } => guest("open", json!({"app": app, "args": args, "wait_s": wait})),
        GuiCmd::Screenshot { window, .. } => guest("screenshot", json!({"window": window, "format": "png"})),
        GuiCmd::Click { at, right, double } => guest("click", click_args(&parse_at(&at)?, right, if double { 2 } else { 1 })),
        GuiCmd::Type { text } => guest("type", json!({"text": text})),
        GuiCmd::Key { combo } => guest("key", json!({"combo": combo})),
        GuiCmd::Scroll { at, dy, dx } => {
            let (x, y) = parse_point(&at)?;
            guest("scroll", json!({"x": num(x), "y": num(y), "dx": num(dx), "dy": num(dy)}))
        }
        GuiCmd::Drag { from, to } => {
            let ((x0, y0), (x1, y1)) = (parse_point(&from)?, parse_point(&to)?);
            guest("drag", json!({"from": [num(x0), num(y0)], "to": [num(x1), num(y1)]}))
        }
        GuiCmd::Tree { app, depth } => guest("tree", json!({"app": app, "depth": depth})),
        GuiCmd::Wait { label: Some(label), timeout, .. } => guest("wait", json!({"label": label, "timeout_s": timeout})),
        GuiCmd::Wait { idle: true, timeout, .. } => guest("wait", json!({"idle": true, "timeout_s": timeout})),
        GuiCmd::Wait { .. } => bail!("wait for what? A label, or --idle"),
        GuiCmd::Windows => guest("windows", json!({})),
        GuiCmd::Focus { app } => guest("focus", json!({"app": app})),
        GuiCmd::Quit { app } => guest("quit", json!({"app": app})),
        GuiCmd::Note { text } => Ok(Action::Note { text }),
        GuiCmd::Session { action: SessionCmd::New { clone } } => Ok(Action::SessionNew { clone }),
        GuiCmd::Session { action: SessionCmd::End { session } } => Ok(Action::SessionEnd { session }),
        GuiCmd::Session { action: SessionCmd::List } => Ok(Action::SessionList),
    }
}

/// Who acts, where, and in which session. `perform` updates target and
/// session when a session starts or ends (the MCP server keeps them).
pub struct Ctx<'a> {
    pub paths: &'a Paths,
    pub target: Option<Target>,
    pub session: Option<String>,
    pub who: Who,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub verb: String,
    pub result: Value,
    pub hit: Option<Value>,
    pub screenshot: Option<PathBuf>,
    pub png: Vec<u8>,
}

impl Outcome {
    pub fn to_json(&self) -> Value {
        json!({"ok": true, "verb": self.verb, "result": self.result, "hit": self.hit, "screenshot": self.screenshot})
    }
}

/// The feature flags: "mac" for macOS targets, "gui" for Linux ones.
pub fn gate(paths: &Paths, target: Option<&Target>) -> Result<()> {
    let (mac, gui) = (gui_log::config_bool(paths, "mac"), gui_log::config_bool(paths, "gui"));
    match target {
        Some(Target::Mac(_)) if !mac => bail!("macOS VMs are off; turn them on with \"mac\": true in ~/.berth/config.json"),
        Some(Target::Linux(_)) if !gui => bail!("Linux GUI is off; turn it on with \"gui\": true in ~/.berth/config.json"),
        None if !mac && !gui => bail!("berth gui is off; turn it on with \"mac\": true or \"gui\": true in ~/.berth/config.json"),
        _ => Ok(()),
    }
}

pub fn run(paths: &Paths, args: GuiArgs) -> Result<()> {
    let env = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
    let target = args.target.or_else(|| env("BERTH_GUI_TARGET")).map(|t| Target::parse(&t)).transpose()?;
    let session = args.session.or_else(|| env("BERTH_GUI_SESSION"));
    let out = match &args.cmd {
        GuiCmd::Screenshot { out, .. } => out.clone(),
        _ => None,
    };
    let action = action(args.cmd)?;
    let mut ctx = Ctx { paths, target, session, who: gui_log::who(paths) };
    let mut outcome = perform(&mut ctx, action)?;
    if let (Some(out), Some(shot)) = (out, &outcome.screenshot) {
        std::fs::copy(shot, &out).with_context(|| format!("copying the screenshot to {}", out.display()))?;
        outcome.screenshot = Some(out);
    }
    if args.json {
        println!("{}", outcome.to_json());
    } else {
        let text = render(&outcome);
        if !text.is_empty() {
            println!("{text}");
        }
        if outcome.verb == "session.new" && std::io::stdout().is_terminal() {
            eprintln!("(run that line, or pass --session to each command)");
        }
    }
    Ok(())
}

/// Do an action: end idle sessions, log the start, act, log the end.
pub fn perform(ctx: &mut Ctx, action: Action) -> Result<Outcome> {
    // Before anything touches ~/.berth: with both flags off, nothing does.
    gate(ctx.paths, None)?;
    end_idle(ctx)?;
    if ctx.target.is_none()
        && let Some(session) = &ctx.session
        && let Some(record) = sessions::find(ctx.paths, session)?
    {
        ctx.target = Some(Target::parse(&record.target)?);
    }
    gate(ctx.paths, ctx.target.as_ref())?;
    let verb = action.verb();
    let note = match &action {
        Action::Note { text } => Some(text.clone()),
        _ => None,
    };
    let mut line = Line {
        id: gui_log::new_id(),
        phase: "start".into(),
        time: gui_log::now(),
        target: ctx.target.as_ref().map(Target::to_string),
        session: ctx.session.clone(),
        verb: verb.into(),
        args: action.log_args(),
        ok: None,
        error: None,
        duration_ms: None,
        screenshot: None,
        hit: None,
        who: ctx.who.clone(),
    };
    log(ctx.paths, &line);
    let started = Instant::now();
    let id = line.id.clone();
    let result = act(ctx, &id, action);
    line.phase = "end".into();
    line.time = gui_log::now();
    line.duration_ms = Some(started.elapsed().as_millis() as u64);
    // A session that just started is where the command ended up.
    line.target = ctx.target.as_ref().map(Target::to_string).or(line.target);
    line.session = ctx.session.clone().or(line.session);
    match &result {
        Ok(outcome) => {
            line.ok = Some(true);
            line.hit = outcome.hit.clone();
            line.screenshot = outcome.screenshot.as_ref().map(|p| p.to_string_lossy().into_owned());
        }
        Err(err) => {
            line.ok = Some(false);
            line.error = Some(format!("{err:#}"));
        }
    }
    log(ctx.paths, &line);
    if result.is_ok()
        && verb != "session.end"
        && let Some(session) = &ctx.session
    {
        let _ = sessions::touch(ctx.paths, session, note.as_deref());
    }
    result
}

fn log(paths: &Paths, line: &Line) {
    if let Err(err) = gui_log::write(paths, line) {
        eprintln!("berth gui: couldn't write the activity log: {err:#}");
    }
}

/// End sessions idle past gui_idle_minutes; it's an error to go on in one.
fn end_idle(ctx: &mut Ctx) -> Result<()> {
    let idle = sessions::take_idle(ctx.paths)?;
    for record in &idle {
        eprintln!("berth gui: ended session {} on {} (idle over {} min)", record.session, record.target, sessions::idle_minutes(ctx.paths));
        end_in_guest(ctx.paths, record);
    }
    if let Some(session) = &ctx.session
        && idle.iter().any(|r| &r.session == session)
    {
        bail!("session {session} was ended after {} minutes idle; start another: berth gui session new", sessions::idle_minutes(ctx.paths));
    }
    Ok(())
}

/// Best effort: the session's screen goes away with a clone anyway.
fn end_in_guest(paths: &Paths, record: &sessions::Record) {
    let Ok(target) = Target::parse(&record.target) else { return };
    let ended = call(paths, &target, Some(&record.session), "session.end", json!({}), None, Duration::from_secs(10));
    if record.clone
        && let Target::Mac(vm) = &target
    {
        if let Err(err) = macos::delete(vm) {
            eprintln!("berth gui: couldn't delete the session's VM {vm}: {err:#}");
        }
    } else if let Err(err) = ended {
        eprintln!("berth gui: ending {} in the guest: {err:#}", record.session);
    }
}

/// How long a verb may take in the guest, plus room for the round trip.
fn timeout_for(verb: &str, args: &Value) -> Duration {
    let secs = match verb {
        "open" => args["wait_s"].as_u64().unwrap_or(20) + 15,
        "wait" => args["timeout_s"].as_u64().unwrap_or(20) + 15,
        "push" => 900,
        "session.new" => 120,
        _ => 60,
    };
    Duration::from_secs(secs)
}

/// One request to the target's agent; a refusal is an error.
fn call(paths: &Paths, target: &Target, session: Option<&str>, verb: &str, args: Value, payload: Option<&Path>, timeout: Duration) -> Result<(proto::Reply, Vec<u8>)> {
    let stream = proto::connect(paths, target, timeout)?;
    let mut file = payload.map(std::fs::File::open).transpose()?;
    let payload_len = file.as_ref().map(|f| f.metadata().map(|m| m.len())).transpose()?.unwrap_or(0);
    let request = proto::Request { v: proto::VERSION, session: session.map(str::to_string), verb: verb.into(), args, payload_len };
    let (reply, bytes) = proto::exchange(&stream, &request, file.as_mut().map(|f| f as &mut dyn std::io::Read))?;
    if !reply.ok {
        bail!("{}", reply.error.clone().unwrap_or_else(|| format!("{target} refused {verb}")));
    }
    Ok((reply, bytes))
}

fn need_target(ctx: &Ctx) -> Result<Target> {
    ctx.target.clone().context(
        "no target: pass --target mac:<vm> or linux:<machine>, set BERTH_GUI_TARGET, or start a session (berth gui session new --target …)",
    )
}

fn act(ctx: &mut Ctx, id: &str, action: Action) -> Result<Outcome> {
    let verb = action.verb().to_string();
    let done = |result: Value| Ok(Outcome { verb: verb.clone(), result, ..Outcome::default() });
    match action {
        Action::Guest { verb: guest_verb, args } => {
            let target = need_target(ctx)?;
            let timeout = timeout_for(guest_verb, &args);
            let (reply, bytes) = call(ctx.paths, &target, ctx.session.as_deref(), guest_verb, args, None, timeout)?;
            let mut outcome = Outcome { verb, result: reply.result, hit: reply.hit, ..Outcome::default() };
            if guest_verb == "screenshot" {
                if bytes.is_empty() {
                    bail!("the guest sent no image");
                }
                let shots = gui_log::shots(ctx.paths);
                std::fs::create_dir_all(&shots)?;
                let path = shots.join(format!("{id}.png"));
                std::fs::write(&path, &bytes)?;
                outcome.screenshot = Some(path);
                outcome.png = bytes;
            }
            Ok(outcome)
        }
        Action::Push { path, to } => {
            let target = need_target(ctx)?;
            let tmp = sessions::dir(ctx.paths).join(format!("push-{id}.tar"));
            std::fs::create_dir_all(sessions::dir(ctx.paths))?;
            let pushed = tar(&path, &tmp, !target.is_mac()).and_then(|name| {
                let args = json!({"name": name, "dest": to, "tar": true});
                call(ctx.paths, &target, ctx.session.as_deref(), "push", args.clone(), Some(&tmp), timeout_for("push", &args))
            });
            let _ = std::fs::remove_file(&tmp);
            let (reply, _) = pushed?;
            Ok(Outcome { verb, result: reply.result, hit: reply.hit, ..Outcome::default() })
        }
        Action::Note { .. } => done(json!({})),
        Action::SessionNew { clone } => {
            let record = if clone { new_clone_session(ctx)? } else { new_session(ctx, &need_target(ctx)?, false)? };
            {
                let lock = sessions::lock(ctx.paths)?;
                let mut all = sessions::load_sessions(ctx.paths, &lock);
                all.push(record.clone());
                sessions::save_sessions(ctx.paths, &lock, &all)?;
            }
            ctx.target = Some(Target::parse(&record.target)?);
            ctx.session = Some(record.session.clone());
            done(json!({"session": record.session, "target": record.target, "clone": record.clone}))
        }
        Action::SessionEnd { session } => {
            let session = session.or_else(|| ctx.session.clone()).context("which session? Pass --session or set BERTH_GUI_SESSION")?;
            let record = {
                let lock = sessions::lock(ctx.paths)?;
                let mut all = sessions::load_sessions(ctx.paths, &lock);
                let record = all.iter().position(|r| r.session == session).map(|i| all.remove(i));
                sessions::save_sessions(ctx.paths, &lock, &all)?;
                record
            };
            match record {
                Some(record) => end_in_guest(ctx.paths, &record),
                // Not ours (another Mac user's tooling, or a lost file): still tell the guest.
                None => {
                    call(ctx.paths, &need_target(ctx)?, Some(&session), "session.end", json!({}), None, Duration::from_secs(30))?;
                }
            }
            if ctx.session.as_deref() == Some(&session) {
                ctx.session = None;
            }
            done(json!({"session": session}))
        }
        Action::SessionList => {
            let lock = sessions::lock(ctx.paths)?;
            let wanted = ctx.target.as_ref().map(Target::to_string);
            let all: Vec<_> = sessions::load_sessions(ctx.paths, &lock).into_iter().filter(|r| wanted.as_ref().is_none_or(|t| &r.target == t)).collect();
            done(json!({"sessions": all}))
        }
    }
}

/// session.new in the guest, as a host record.
fn new_session(ctx: &Ctx, target: &Target, clone: bool) -> Result<sessions::Record> {
    let (reply, _) = call(ctx.paths, target, None, "session.new", json!({"agent": ctx.who.kind}), None, timeout_for("session.new", &json!({})))?;
    let session = reply.result["session"].as_str().context("the guest started a session without naming it")?.to_string();
    let now = gui_log::now();
    Ok(sessions::Record { session, target: target.to_string(), agent: ctx.who.clone(), clone, started: now.clone(), last_used: now, note: None })
}

/// A throwaway clone of the target macOS VM, once a VM slot is free.
fn new_clone_session(ctx: &Ctx) -> Result<sessions::Record> {
    let Some(Target::Mac(base)) = &ctx.target else {
        bail!("--clone copies a macOS VM: pass --target mac:<vm>. Linux sessions each get their own screen already");
    };
    if let Ok(vms) = macos::list()
        && !vms.iter().any(|vm| &vm.name == base)
    {
        bail!("no macOS VM named {base} (berth mac list)");
    }
    let vm = format!("gui-{}", &gui_log::new_id()[..8]);
    wait_for_slot(ctx, base, &vm)?;
    let started = (|| {
        wait_for_agent(ctx.paths, &Target::Mac(vm.clone()))?;
        new_session(ctx, &Target::Mac(vm.clone()), true)
    })();
    if started.is_err() {
        let _ = macos::delete(&vm);
    }
    started
}

/// Queue for a macOS VM slot, first come first served, then clone the base
/// and start the clone while still holding the lock, so the next waiter
/// counts it.
fn wait_for_slot(ctx: &Ctx, base: &str, vm: &str) -> Result<()> {
    let ticket = gui_log::new_id();
    let me = sessions::Waiting { ticket: ticket.clone(), base: base.into(), agent: ctx.who.clone(), since: gui_log::now(), pid: std::process::id() };
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    let mut said = None;
    let result = loop {
        let lock = sessions::lock(ctx.paths)?;
        let mut queue = sessions::load_queue(ctx.paths, &lock);
        let ahead = match sessions::prune_and_place(&mut queue, &ticket, sessions::alive) {
            Some(n) => n,
            None => {
                queue.push(me.clone());
                queue.len() - 1
            }
        };
        let running = macos::running(ctx.paths);
        if ahead == 0 && running < macos::MAX_RUNNING {
            let started = macos::clone(base, vm).and_then(|()| {
                macos::start(vm).inspect_err(|_| {
                    let _ = macos::delete(vm);
                })
            });
            queue.retain(|w| w.ticket != ticket);
            sessions::save_queue(ctx.paths, &lock, &queue)?;
            break started;
        }
        sessions::save_queue(ctx.paths, &lock, &queue)?;
        drop(lock);
        if said != Some((ahead, running)) {
            eprintln!("berth gui: waiting for a macOS VM slot ({running} of {} running, {ahead} ahead in the queue)", macos::MAX_RUNNING);
            said = Some((ahead, running));
        }
        if Instant::now() > deadline {
            break Err(anyhow::anyhow!("no macOS VM slot came free in 30 minutes"));
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    if result.is_err() {
        let lock = sessions::lock(ctx.paths)?;
        let mut queue = sessions::load_queue(ctx.paths, &lock);
        queue.retain(|w| w.ticket != ticket);
        sessions::save_queue(ctx.paths, &lock, &queue)?;
    }
    result
}

/// A clone boots to a logged-in desktop in about 20 s; its agent answers
/// `hello` once it's up.
fn wait_for_agent(paths: &Paths, target: &Target) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match call(paths, target, None, "hello", json!({}), None, Duration::from_secs(5)) {
            Ok(_) => return Ok(()),
            Err(err) if Instant::now() > deadline => return Err(err.context(format!("{target}'s GUI agent didn't answer in 3 minutes"))),
            Err(_) => std::thread::sleep(Duration::from_secs(2)),
        }
    }
}

/// Tar `path` into `out`; returns the name the tar holds it under. Not
/// for a Mac: Apple's metadata files (._*) would litter a Linux guest.
fn tar(path: &Path, out: &Path, for_linux: bool) -> Result<String> {
    let real = std::fs::canonicalize(path).with_context(|| format!("{}", path.display()))?;
    let name = real.file_name().context("can't push /")?.to_string_lossy().into_owned();
    let parent = real.parent().context("can't push /")?;
    let mut cmd = std::process::Command::new("tar");
    cmd.arg("-cf").arg(out).arg("-C").arg(parent);
    // A name like "-x" would be read as an option.
    cmd.arg(if name.starts_with('-') { format!("./{name}") } else { name.clone() });
    if for_linux {
        cmd.env("COPYFILE_DISABLE", "1");
    }
    let status = cmd.status().context("running tar")?;
    if !status.success() {
        bail!("tar couldn't pack {}", path.display());
    }
    Ok(name)
}

/// What a command prints, as text.
pub fn render(o: &Outcome) -> String {
    let r = &o.result;
    let s = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| if v.is_null() { String::new() } else { v.to_string() });
    match o.verb.as_str() {
        "push" => format!("pushed to {}", s(&r["path"])),
        "open" => format!("opened {} (pid {})", s(&r["window"]), s(&r["pid"])),
        "screenshot" => o.screenshot.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        "tree" => {
            let mut out = String::new();
            tree_text(&r["tree"], 0, &mut out);
            out.trim_end().to_string()
        }
        "windows" => r["windows"]
            .as_array()
            .map(|ws| {
                ws.iter()
                    .map(|w| format!("{}{} — {} {}", if w["focused"] == true { "* " } else { "  " }, s(&w["app"]), s(&w["title"]), s(&w["frame"])))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "wait" => format!("done after {} ms", s(&r["waited_ms"])),
        "session.new" => format!("export BERTH_GUI_SESSION={}", s(&r["session"])),
        "session.end" => format!("ended session {}", s(&r["session"])),
        "session.list" => {
            let list = r["sessions"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                return "no sessions".into();
            }
            list.iter()
                .map(|x| {
                    let clone = if x["clone"] == true { " (clone)" } else { "" };
                    format!("{}  {}{clone}  {}  since {}", s(&x["session"]), s(&x["target"]), s(&x["agent"]["kind"]), s(&x["started"]))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "note" => String::new(),
        _ => match &o.hit {
            Some(hit) => format!("hit {} {:?} at {}", s(&hit["role"]), s(&hit["label"]), s(&hit["frame"])),
            None => "ok".into(),
        },
    }
}

/// `AXButton "Save" [10,20,80,24]`, children indented.
fn tree_text(node: &Value, depth: usize, out: &mut String) {
    if !node.is_object() {
        return;
    }
    let text = |k: &str| node[k].as_str().filter(|v| !v.is_empty());
    out.push_str(&"  ".repeat(depth));
    out.push_str(text("role").unwrap_or("?"));
    if let Some(label) = text("label") {
        out.push_str(&format!(" {label:?}"));
    }
    if let Some(value) = text("value") {
        out.push_str(&format!(" = {value:?}"));
    }
    if let Some(frame) = node["frame"].as_array() {
        out.push_str(&format!(" [{}]", frame.iter().map(Value::to_string).collect::<Vec<_>>().join(",")));
    }
    if node["enabled"] == false {
        out.push_str(" disabled");
    }
    if node["focused"] == true {
        out.push_str(" focused");
    }
    out.push('\n');
    for child in node["children"].as_array().into_iter().flatten() {
        tree_text(child, depth + 1, out);
    }
}

#[cfg(test)]
pub mod tests;
