//! Tests for `berth gui`, with a fake guest: a unix socket server that
//! speaks docs/gui-protocol.md, at a macOS VM's agent.sock or behind a fake
//! daemon (Request::Gui), so the client runs end to end. mcp.rs uses it too.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;
use serde_json::{Value, json};

use super::*;

pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

/// Requests the fake guests got, with their payloads.
type Seen = Vec<(Value, Vec<u8>)>;

/// A ~/.berth in a temp folder, with guests that record what they're sent.
pub struct Fake {
    pub root: PathBuf,
    pub paths: Paths,
    seen: Arc<Mutex<Seen>>,
}

impl Fake {
    pub fn new(config: Value) -> Fake {
        // Short: a unix socket's path must fit in 104 bytes.
        let root = std::env::temp_dir().join(format!("bg{}", &gui_log::new_id()[..10]));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("config.json"), config.to_string()).unwrap();
        Fake { paths: Paths { home: root.clone(), root: root.clone() }, root, seen: Arc::default() }
    }

    /// A running macOS VM named `vm` whose agent answers.
    pub fn mac_guest(&self, vm: &str) {
        let dir = proto::mac_dir(&self.paths, vm);
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("agent.sock")).unwrap();
        let seen = self.seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &seen);
            }
        });
    }

    /// The daemon's control socket, answering Request::Gui like daemon.rs
    /// and then being the machine's guest agent.
    pub fn linux_daemon(&self) {
        std::fs::create_dir_all(self.paths.run()).unwrap();
        let listener = UnixListener::bind(self.paths.control_socket()).unwrap();
        let seen = self.seen.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut line = Vec::new();
                let mut byte = [0u8];
                while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
                    line.push(byte[0]);
                }
                let request: Value = serde_json::from_slice(&line).unwrap();
                assert_eq!(request["cmd"], "gui");
                if request["machine"] == "missing" {
                    stream.write_all(b"{\"ok\":false,\"error\":\"no machine named missing\"}\n").unwrap();
                    continue;
                }
                stream.write_all(b"{\"ok\":true,\"error\":null}\n").unwrap();
                serve(stream, &seen);
            }
        });
    }

    pub fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().iter().map(|(r, _)| r.clone()).collect()
    }

    pub fn log_lines(&self) -> Vec<Value> {
        let mut lines = Vec::new();
        for file in std::fs::read_dir(gui_log::dir(&self.paths)).into_iter().flatten().flatten() {
            if file.path().extension().is_some_and(|e| e == "jsonl") {
                for line in std::fs::read_to_string(file.path()).unwrap().lines() {
                    lines.push(serde_json::from_str(line).unwrap());
                }
            }
        }
        lines
    }

    pub fn ctx(&self, target: Option<&str>) -> Ctx<'_> {
        Ctx {
            paths: &self.paths,
            target: target.map(|t| Target::parse(t).unwrap()),
            session: None,
            who: Who { kind: "claude-code".into(), chat: Some("chat-1".into()), ..Who::default() },
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// One request, answered the way a real agent would.
fn serve(stream: UnixStream, seen: &Mutex<Seen>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let request: Value = serde_json::from_str(&line).unwrap();
    let mut payload = vec![0u8; request["payload_len"].as_u64().unwrap_or(0) as usize];
    reader.read_exact(&mut payload).unwrap();
    let args = &request["args"];
    let (reply, bytes): (Value, &[u8]) = if request["v"] != 1 {
        (json!({"ok": false, "error": "unsupported version"}), b"")
    } else {
        match request["verb"].as_str().unwrap_or("") {
            "hello" => (json!({"ok": true, "result": {"os": "macos", "agent": "fake", "screen": [1440, 900], "scale": 2}}), b""),
            "session.new" => (json!({"ok": true, "result": {"session": "s-1234"}}), b""),
            "screenshot" => (json!({"ok": true, "result": {"width": 1440, "height": 900}, "payload_len": PNG.len()}), PNG),
            "push" => (json!({"ok": true, "result": {"path": format!("/Users/berth/Pushed/{}", args["name"].as_str().unwrap())}}), b""),
            "click" if args["label"] == "Missing" => (json!({"ok": false, "error": "no element labelled \"Missing\""}), b""),
            "click" => (json!({"ok": true, "result": {}, "hit": {"role": "AXButton", "label": "Save", "frame": [10, 20, 80, 24]}}), b""),
            "tree" => (
                json!({"ok": true, "result": {"tree": {"role": "frame", "label": "Editor", "frame": [0, 0, 800, 600], "children": [
                    {"role": "push button", "label": "Save", "frame": [10, 20, 80, 24], "enabled": true, "children": []},
                    {"role": "text", "label": "", "value": "hello", "focused": true, "children": []}]}}}),
                b"",
            ),
            "windows" => (json!({"ok": true, "result": {"windows": [{"app": "TextEdit", "title": "Untitled", "frame": [0, 0, 800, 600], "focused": true}]}}), b""),
            _ => (json!({"ok": true, "result": {}}), b""),
        }
    };
    seen.lock().unwrap().push((request, payload));
    let mut stream = stream;
    let _ = stream.write_all(format!("{reply}\n").as_bytes());
    let _ = stream.write_all(bytes);
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Top,
}

#[derive(clap::Subcommand)]
enum Top {
    Gui(GuiArgs),
}

fn parse(args: &[&str]) -> Result<GuiArgs> {
    let Top::Gui(args) = Cli::try_parse_from(["berth", "gui"].iter().chain(args))?.cmd;
    Ok(args)
}

#[test]
fn points_and_labels() {
    assert_eq!(parse_at("120,340").unwrap(), At::Point(120.0, 340.0));
    assert_eq!(parse_at("1.5, 2").unwrap(), At::Point(1.5, 2.0));
    assert_eq!(parse_at("Save").unwrap(), At::Label("Save".into()));
    assert_eq!(parse_at("Save, then quit").unwrap(), At::Label("Save, then quit".into()));
    assert_eq!(parse_at("NaN,1").unwrap(), At::Label("NaN,1".into()));
    assert!(parse_at(" ").is_err());
    assert!(parse_point("Save").is_err());
    assert_eq!(num(3.0), json!(3));
    assert_eq!(num(-2.5), json!(-2.5));
}

#[test]
fn command_lines_become_protocol_args() {
    let a = parse(&["click", "Save", "--right", "--double", "--target", "mac:test", "--json"]).unwrap();
    assert_eq!((a.target.as_deref(), a.json), (Some("mac:test"), true));
    assert_eq!(action(a.cmd).unwrap(), Action::Guest { verb: "click", args: json!({"label": "Save", "button": "right", "count": 2}) });
    let a = parse(&["--session", "s-1", "click", "120,340"]).unwrap();
    assert_eq!(a.session.as_deref(), Some("s-1"));
    assert_eq!(action(a.cmd).unwrap(), Action::Guest { verb: "click", args: json!({"x": 120, "y": 340, "button": "left", "count": 1}) });
    let a = parse(&["scroll", "10,20", "--dy", "-120"]).unwrap();
    assert_eq!(action(a.cmd).unwrap(), Action::Guest { verb: "scroll", args: json!({"x": 10, "y": 20, "dx": 0, "dy": -120}) });
    assert!(action(parse(&["scroll", "Save", "--dy", "5"]).unwrap().cmd).is_err());
    let a = parse(&["drag", "1,2", "30,40.5"]).unwrap();
    assert_eq!(action(a.cmd).unwrap(), Action::Guest { verb: "drag", args: json!({"from": [1, 2], "to": [30, 40.5]}) });
    let a = parse(&["open", "/Applications/TextEdit.app", "--", "-x", "file.txt"]).unwrap();
    assert_eq!(
        action(a.cmd).unwrap(),
        Action::Guest { verb: "open", args: json!({"app": "/Applications/TextEdit.app", "args": ["-x", "file.txt"], "wait_s": 20}) }
    );
    // Our own options after the app are still ours; the app's come after --.
    let a = parse(&["open", "TextEdit", "--target", "mac:test", "--json"]).unwrap();
    assert_eq!((a.target.as_deref(), a.json), (Some("mac:test"), true));
    assert_eq!(action(a.cmd).unwrap(), Action::Guest { verb: "open", args: json!({"app": "TextEdit", "args": [], "wait_s": 20}) });
    let a = parse(&["wait", "--idle", "--timeout", "5"]).unwrap();
    assert_eq!(action(a.cmd).unwrap(), Action::Guest { verb: "wait", args: json!({"idle": true, "timeout_s": 5}) });
    assert!(action(parse(&["wait"]).unwrap().cmd).is_err());
    assert!(parse(&["wait", "Done", "--idle"]).is_err());
    let a = parse(&["tree", "--app", "Notes"]).unwrap();
    assert_eq!(action(a.cmd).unwrap(), Action::Guest { verb: "tree", args: json!({"app": "Notes", "depth": 12}) });
    assert_eq!(action(parse(&["session", "new", "--clone"]).unwrap().cmd).unwrap(), Action::SessionNew { clone: true });
    assert_eq!(action(parse(&["session", "end", "s-9"]).unwrap().cmd).unwrap(), Action::SessionEnd { session: Some("s-9".into()) });
    assert_eq!(action(parse(&["key", "cmd+s"]).unwrap().cmd).unwrap(), Action::Guest { verb: "key", args: json!({"combo": "cmd+s"}) });
}

#[test]
fn off_without_the_flags() {
    let fake = Fake::new(json!({"gui": true}));
    let err = perform(&mut fake.ctx(Some("mac:test")), Action::Guest { verb: "windows", args: json!({}) }).unwrap_err();
    assert!(err.to_string().contains("\"mac\": true"), "{err}");
    assert!(gate(&fake.paths, Some(&Target::Linux("dev".into()))).is_ok());
    assert!(fake.log_lines().is_empty());
    // Both off: nothing at all is written under ~/.berth.
    let off = Fake::new(json!({}));
    assert!(perform(&mut off.ctx(Some("linux:dev")), Action::Note { text: "x".into() }).is_err());
    let written: Vec<_> = std::fs::read_dir(&off.root).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(written, ["config.json"]);
}

#[test]
fn a_mac_session_end_to_end() {
    let fake = Fake::new(json!({"mac": true}));
    fake.mac_guest("test");
    let mut ctx = fake.ctx(Some("mac:test"));

    let new = perform(&mut ctx, Action::SessionNew { clone: false }).unwrap();
    assert_eq!(render(&new), "export BERTH_GUI_SESSION=s-1234");
    assert_eq!(ctx.session.as_deref(), Some("s-1234"));
    // Later calls find the target from the session alone.
    let mut ctx = Ctx { target: None, session: Some("s-1234".into()), ..fake.ctx(None) };

    let app = fake.root.join("My App.app");
    std::fs::create_dir_all(app.join("Contents")).unwrap();
    std::fs::write(app.join("Contents/Info.plist"), "<plist/>").unwrap();
    let pushed = perform(&mut ctx, Action::Push { path: app.clone(), to: None }).unwrap();
    assert_eq!(render(&pushed), "pushed to /Users/berth/Pushed/My App.app");

    let shot = perform(&mut ctx, Action::Guest { verb: "screenshot", args: json!({"window": null, "format": "png"}) }).unwrap();
    let path = shot.screenshot.clone().unwrap();
    assert!(path.starts_with(gui_log::shots(&fake.paths)));
    assert_eq!(std::fs::read(&path).unwrap(), PNG);
    assert_eq!(render(&shot), path.display().to_string());

    let click = perform(&mut ctx, Action::Guest { verb: "click", args: click_args(&At::Label("Save".into()), false, 1) }).unwrap();
    assert_eq!(render(&click), "hit AXButton \"Save\" at [10,20,80,24]");
    let missing = perform(&mut ctx, Action::Guest { verb: "click", args: click_args(&At::Label("Missing".into()), false, 1) });
    assert!(missing.unwrap_err().to_string().contains("no element labelled"));
    perform(&mut ctx, Action::Note { text: "checking Save".into() }).unwrap();

    let list = perform(&mut ctx, Action::SessionList).unwrap();
    assert_eq!(list.result["sessions"][0]["note"], "checking Save");
    assert_eq!(list.result["sessions"][0]["agent"]["kind"], "claude-code");
    perform(&mut ctx, Action::SessionEnd { session: None }).unwrap();
    assert_eq!(ctx.session, None);
    assert!(sessions::find(&fake.paths, "s-1234").unwrap().is_none());

    // What the guest saw.
    let seen = fake.seen.lock().unwrap().clone();
    let verbs: Vec<&str> = seen.iter().map(|(r, _)| r["verb"].as_str().unwrap()).collect();
    assert_eq!(verbs, ["session.new", "push", "screenshot", "click", "click", "session.end"]);
    assert_eq!(seen[0].0["args"]["agent"], "claude-code");
    assert!(seen[1..].iter().all(|(r, _)| r["session"] == "s-1234"));
    let (push, tar) = &seen[1];
    assert_eq!((push["args"]["name"].as_str(), push["args"]["tar"].as_bool()), (Some("My App.app"), Some(true)));
    assert_eq!(push["payload_len"].as_u64(), Some(tar.len() as u64));
    let tar_file = fake.root.join("seen.tar");
    std::fs::write(&tar_file, tar).unwrap();
    let listing = std::process::Command::new("tar").arg("-tf").arg(&tar_file).output().unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.lines().any(|l| l == "My App.app/Contents/Info.plist"), "{listing}");
    // The temporary tar is gone.
    assert!(!std::fs::read_dir(sessions::dir(&fake.paths)).unwrap().flatten().any(|f| f.file_name().to_string_lossy().ends_with(".tar")));

    // The log: a start and an end line per command, paired by id.
    let log = fake.log_lines();
    assert_eq!(log.len(), 2 * 8);
    for pair in log.chunks(2) {
        assert_eq!((pair[0]["phase"].as_str(), pair[1]["phase"].as_str()), (Some("start"), Some("end")));
        assert_eq!(pair[0]["id"], pair[1]["id"]);
        assert_eq!(pair[1]["target"], "mac:test");
        assert_eq!(pair[1]["who"]["chat"], "chat-1");
        assert!(pair[1]["duration_ms"].is_u64());
    }
    let end = |verb: &str| log.iter().filter(|l| l["phase"] == "end" && l["verb"] == verb).cloned().collect::<Vec<_>>();
    assert_eq!(end("session.new")[0]["session"], "s-1234");
    assert_eq!(end("screenshot")[0]["screenshot"], path.display().to_string());
    let clicks = end("click");
    assert_eq!((clicks[0]["ok"].as_bool(), clicks[0]["hit"]["label"].as_str()), (Some(true), Some("Save")));
    assert_eq!(clicks[1]["ok"], false);
    assert!(clicks[1]["error"].as_str().unwrap().contains("Missing"));
    assert_eq!(end("note")[0]["args"]["text"], "checking Save");
}

#[test]
fn linux_through_the_daemon() {
    let fake = Fake::new(json!({"gui": true}));
    fake.linux_daemon();
    let mut ctx = fake.ctx(Some("linux:dev"));
    let tree = perform(&mut ctx, Action::Guest { verb: "tree", args: json!({"app": null, "depth": 12}) }).unwrap();
    assert_eq!(
        render(&tree),
        "frame \"Editor\" [0,0,800,600]\n  push button \"Save\" [10,20,80,24]\n  text = \"hello\" focused"
    );
    let windows = perform(&mut ctx, Action::Guest { verb: "windows", args: json!({}) }).unwrap();
    assert_eq!(render(&windows), "* TextEdit — Untitled [0,0,800,600]");
    let dir = fake.root.join("-odd");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("x"), "").unwrap();
    perform(&mut ctx, Action::Push { path: dir, to: Some("/tmp".into()) }).unwrap();
    let push = fake.requests().into_iter().find(|r| r["verb"] == "push").unwrap();
    assert_eq!((push["args"]["name"].as_str(), push["args"]["dest"].as_str()), (Some("-odd"), Some("/tmp")));

    let mut missing = fake.ctx(Some("linux:missing"));
    let err = perform(&mut missing, Action::Guest { verb: "windows", args: json!({}) }).unwrap_err();
    assert!(err.to_string().contains("no machine named missing"), "{err}");
}

#[test]
fn idle_sessions_end_on_the_next_call() {
    let fake = Fake::new(json!({"mac": true, "gui_idle_minutes": 30}));
    fake.mac_guest("test");
    let old = sessions::Record {
        session: "s-old".into(),
        target: "mac:test".into(),
        agent: Who::default(),
        clone: false,
        started: "2026-01-01T00:00:00Z".into(),
        last_used: "2026-01-01T00:00:00Z".into(),
        note: None,
    };
    {
        let lock = sessions::lock(&fake.paths).unwrap();
        sessions::save_sessions(&fake.paths, &lock, &[old]).unwrap();
    }
    let mut ctx = Ctx { session: Some("s-old".into()), ..fake.ctx(Some("mac:test")) };
    let err = perform(&mut ctx, Action::Note { text: "hi".into() }).unwrap_err();
    assert!(err.to_string().contains("ended after 30 minutes idle"), "{err}");
    assert_eq!(fake.requests()[0]["verb"], "session.end");
    assert_eq!(fake.requests()[0]["session"], "s-old");
    assert!(sessions::find(&fake.paths, "s-old").unwrap().is_none());
}

#[test]
fn clones_are_for_macos() {
    let fake = Fake::new(json!({"gui": true}));
    let err = perform(&mut fake.ctx(Some("linux:dev")), Action::SessionNew { clone: true }).unwrap_err();
    assert!(err.to_string().contains("--clone copies a macOS VM"), "{err}");
}

#[test]
fn a_target_is_needed() {
    let fake = Fake::new(json!({"mac": true}));
    let err = perform(&mut fake.ctx(None), Action::Guest { verb: "windows", args: json!({}) }).unwrap_err();
    assert!(err.to_string().contains("no target"), "{err}");
}

/// Found by two agents testing in parallel: a call with the base's target
/// and a clone's session went to the base, not the clone.
#[test]
fn a_clone_session_wins_over_its_base_target() {
    let fake = Fake::new(json!({"mac": true}));
    // Only the clone answers: anything sent to the base fails.
    fake.mac_guest("gui-c1");
    std::fs::write(proto::mac_dir(&fake.paths, "gui-c1").join("vm.json"), r#"{"cpus":4,"memory_mib":8192,"base":"test","created":1}"#).unwrap();
    let record = sessions::Record {
        session: "s-c1".into(),
        target: "mac:gui-c1".into(),
        agent: Who::default(),
        clone: true,
        started: gui_log::now(),
        last_used: gui_log::now(),
        note: None,
    };
    {
        let lock = sessions::lock(&fake.paths).unwrap();
        sessions::save_sessions(&fake.paths, &lock, &[record]).unwrap();
    }
    let mut ctx = Ctx { session: Some("s-c1".into()), ..fake.ctx(Some("mac:test")) };
    perform(&mut ctx, Action::Guest { verb: "windows", args: json!({}) }).unwrap();
    assert_eq!(ctx.target, Some(Target::Mac("gui-c1".into())));
    assert_eq!(fake.requests().last().unwrap()["verb"], "windows");
    // Any other target is a mistake, not something to guess around.
    let mut ctx = Ctx { session: Some("s-c1".into()), ..fake.ctx(Some("mac:other")) };
    let err = perform(&mut ctx, Action::Guest { verb: "windows", args: json!({}) }).unwrap_err();
    assert!(err.to_string().contains("session s-c1 is on mac:gui-c1, not mac:other"), "{err}");
}
