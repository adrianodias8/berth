//! `berth mcp`: a Model Context Protocol server on stdin/stdout, so coding
//! agents can use `berth gui` as tools (gui_click, gui_screenshot, …)
//! instead of shelling out. JSON-RPC 2.0, one message per line, protocol
//! version 2025-06-18; only tools (initialize, ping, tools/list,
//! tools/call). Each call goes through gui::perform, so it is logged like
//! the CLI's; who is the client's own clientInfo plus what gui_log finds
//! in our environment (the agent started us, so its variables are ours).
//!
//! The server remembers the session gui_session_new starts: an MCP client
//! can't export BERTH_GUI_SESSION for the next call.

use std::io::{BufRead, Write};

use anyhow::Result;
use serde_json::{Value, json};

use crate::gui::{self, Action, At, Ctx, Outcome, Target};
use crate::gui_log::{self, Who};
use crate::paths::Paths;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve(paths: &Paths) -> Result<()> {
    let mut server = Server::new(paths, gui_log::who(paths));
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        if let Some(reply) = server.handle(&line?) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

pub struct Server<'a> {
    paths: &'a Paths,
    who: Who,
    /// From BERTH_GUI_TARGET, if the client's MCP config sets it.
    default_target: Option<Target>,
    target: Option<Target>,
    session: Option<String>,
}

impl<'a> Server<'a> {
    pub fn new(paths: &'a Paths, who: Who) -> Self {
        let env = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
        let default_target = env("BERTH_GUI_TARGET").and_then(|t| Target::parse(&t).ok());
        Server { paths, who, target: default_target.clone(), default_target, session: env("BERTH_GUI_SESSION") }
    }

    /// One incoming line; the reply line, if it needs one.
    pub fn handle(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(msg) => msg,
            Err(_) => return Some(error(Value::Null, -32700, "parse error")),
        };
        let id = msg.get("id").cloned();
        let Some(method) = msg["method"].as_str() else {
            // A response (we never ask the client anything) or junk.
            return match id {
                Some(id) if msg.get("result").is_none() && msg.get("error").is_none() => Some(error(id, -32600, "invalid request")),
                _ => None,
            };
        };
        // Notifications (initialized, cancelled) get no reply.
        let id = id?;
        let result = match method {
            "initialize" => Ok(self.initialize(&msg["params"])),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => self.call(&msg["params"]),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
            Err((code, message)) => error(id, code, &message),
        })
    }

    fn initialize(&mut self, params: &Value) -> Value {
        let client = params["clientInfo"]["name"].as_str().unwrap_or("");
        let kind = client_kind(client);
        // The client's word wins over the environment's, except for an
        // unknown client run by an agent we did recognize.
        if kind != "mcp" || self.who.kind == "terminal" {
            if kind != self.who.kind {
                self.who.chat = None;
                self.who.title = None;
            }
            self.who.kind = kind.into();
        }
        let asked = params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION);
        let version = if SUPPORTED.contains(&asked) { asked } else { PROTOCOL_VERSION };
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "berth", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Test apps in a macOS or Linux guest. Start with gui_session_new (target mac:<vm> or linux:<machine>), \
                gui_push the app, gui_open it, gui_screenshot, act by label (gui_click label=…), verify with gui_screenshot or gui_tree, \
                and gui_session_end when done. gui_note records what you're checking.",
        })
    }

    fn call(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params["name"].as_str().unwrap_or("");
        if !TOOLS.iter().any(|(tool, _)| *tool == name) {
            return Err((-32602, format!("unknown tool {name:?}")));
        }
        let args = if params["arguments"].is_object() { params["arguments"].clone() } else { json!({}) };
        Ok(match self.run(name, &args) {
            Ok(outcome) => content(&outcome),
            Err(err) => json!({"content": [{"type": "text", "text": format!("{err:#}")}], "isError": true}),
        })
    }

    fn run(&mut self, name: &str, args: &Value) -> Result<Outcome> {
        let action = action_for(name, args)?;
        let target = match args["target"].as_str() {
            Some(t) => Some(Target::parse(t)?),
            None => self.target.clone(),
        };
        let session = args["session"].as_str().map(str::to_string).or_else(|| self.session.clone());
        let ended = match &action {
            Action::SessionEnd { session: s } => s.clone().or_else(|| session.clone()),
            _ => None,
        };
        let mut ctx = Ctx { paths: self.paths, target, session, who: self.who.clone() };
        let outcome = gui::perform(&mut ctx, action)?;
        match outcome.verb.as_str() {
            "session.new" => {
                self.session = ctx.session;
                self.target = ctx.target;
            }
            "session.end" if ended.is_some() && ended == self.session => {
                self.session = None;
                self.target = self.default_target.clone();
            }
            _ => {}
        }
        Ok(outcome)
    }
}

fn error(id: Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

fn client_kind(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase();
    if name.contains("claude") {
        "claude-code"
    } else if name.contains("cursor") {
        "cursor"
    } else if name.contains("codex") {
        "codex"
    } else {
        "mcp"
    }
}

fn content(o: &Outcome) -> Value {
    let mut blocks = Vec::new();
    if !o.png.is_empty() {
        blocks.push(json!({"type": "image", "data": base64(&o.png), "mimeType": "image/png"}));
    }
    let text = match o.verb.as_str() {
        "screenshot" => format!("saved to {}", o.screenshot.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
        "session.new" => format!(
            "session {} on {}; later calls use it until gui_session_end",
            o.result["session"].as_str().unwrap_or(""),
            o.result["target"].as_str().unwrap_or("")
        ),
        _ => gui::render(o),
    };
    blocks.push(json!({"type": "text", "text": if text.is_empty() { "ok".to_string() } else { text }}));
    json!({"content": blocks, "isError": false})
}

/// Tool names with their descriptions; the schemas are in `tools`.
const TOOLS: [(&str, &str); 17] = [
    ("gui_session_new", "Start a session on a target (mac:<vm> or linux:<machine>): your own screen on Linux; with clone=true on macOS, a throwaway copy of the VM. Later calls use it."),
    ("gui_session_end", "End the session (the current one by default); a cloned macOS VM is deleted."),
    ("gui_session_list", "List sessions, with their targets and agents."),
    ("gui_push", "Copy a file, folder or .app from this Mac into the guest (~/Pushed by default). Returns the path in the guest."),
    ("gui_open", "Open an app (a path in the guest, or an app name) and wait for its window."),
    ("gui_screenshot", "A PNG of the guest's screen, or of one app's window."),
    ("gui_click", "Click an element by its accessibility label (preferred), or a point x,y in guest screen points."),
    ("gui_type", "Type text into the focused element."),
    ("gui_key", "Press a key combination: cmd+s, ctrl+shift+t, return, escape, tab."),
    ("gui_scroll", "Scroll at a point by dx, dy points."),
    ("gui_drag", "Drag from one point to another."),
    ("gui_tree", "The accessibility tree (role, label, value, frame) of an app, or of the focused one."),
    ("gui_wait", "Wait until an element with a label appears, or (idle=true) until the screen settles."),
    ("gui_windows", "List windows: app, title, frame, focused."),
    ("gui_focus", "Bring an app to the front."),
    ("gui_quit", "Quit an app."),
    ("gui_note", "Write a note in the activity log: what you're about to check, or what you found."),
];

fn tools() -> Vec<Value> {
    let string = |d: &str| json!({"type": "string", "description": d});
    let number = |d: &str| json!({"type": "number", "description": d});
    let point = |d: &str| json!({"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2, "description": d});
    TOOLS
        .iter()
        .map(|(name, description)| {
            let (mut props, required): (Value, &[&str]) = match *name {
                "gui_session_new" => (json!({"clone": {"type": "boolean", "description": "macOS: a throwaway copy of the target VM"}}), &[]),
                "gui_push" => (json!({"path": string("a path on this Mac"), "to": string("a folder in the guest")}), &["path"]),
                "gui_open" => (
                    json!({"app": string("a path in the guest or an app name"), "args": {"type": "array", "items": {"type": "string"}},
                        "wait_s": number("seconds to wait for a window (20)")}),
                    &["app"],
                ),
                "gui_screenshot" => (json!({"window": string("only this app's window")}), &[]),
                "gui_click" => (
                    json!({"label": string("the element's label, e.g. Save"), "x": number("x"), "y": number("y"),
                        "button": {"type": "string", "enum": ["left", "right"]}, "count": number("2 for a double click")}),
                    &[],
                ),
                "gui_type" => (json!({"text": string("the text")}), &["text"]),
                "gui_key" => (json!({"combo": string("e.g. cmd+s")}), &["combo"]),
                "gui_scroll" => (json!({"x": number("x"), "y": number("y"), "dx": number("points right"), "dy": number("points down")}), &["x", "y"]),
                "gui_drag" => (json!({"from": point("[x, y]"), "to": point("[x, y]")}), &["from", "to"]),
                "gui_tree" => (json!({"app": string("an app; default the focused one"), "depth": number("levels (12)")}), &[]),
                "gui_wait" => (
                    json!({"label": string("a label to wait for"), "idle": {"type": "boolean"}, "timeout_s": number("seconds (20)")}),
                    &[],
                ),
                "gui_focus" | "gui_quit" => (json!({"app": string("the app")}), &["app"]),
                "gui_note" => (json!({"text": string("the note")}), &["text"]),
                _ => (json!({}), &[]),
            };
            props["target"] = string("mac:<vm> or linux:<machine>; default: the session's");
            props["session"] = string("a session id; default: the one gui_session_new started");
            json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": props, "required": required}})
        })
        .collect()
}

/// What the agent may copy out of this Mac. Guest text the agent reads
/// (labels, titles) could steer it, so nothing in the home folder's hidden
/// folders (~/.ssh, ~/.aws, ~/.config), no hidden file or folder pushed by
/// name (.env, .git), not the home folder itself, and nothing in ~/Library
/// but ~/Library/Developer (Xcode's builds). A build inside a hidden folder,
/// like SwiftPM's .build/debug/App.app, is fine. `berth gui push` itself is
/// not limited.
fn pushable(path: &str) -> Result<std::path::PathBuf> {
    let full = std::fs::canonicalize(path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).and_then(|h| std::fs::canonicalize(h).ok());
    let dot = |p: &std::path::Path| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'));
    let refused = dot(&full)
        || full.parent().is_none()
        || home.as_ref().is_some_and(|home| {
            full == *home
                || full.strip_prefix(home).ok().and_then(|rest| rest.components().next()).is_some_and(|c| dot(std::path::Path::new(c.as_os_str())))
                || (full.starts_with(home.join("Library")) && !full.starts_with(home.join("Library/Developer")))
                || !full.starts_with(home) && full.starts_with("/Users")
        });
    if refused {
        anyhow::bail!("gui_push won't copy {} (hidden files, the home folder, ~/Library and other users' files stay on this Mac); run `berth gui push` yourself if you mean it", full.display());
    }
    Ok(full)
}

/// A tool call's arguments as the action gui::perform takes.
fn action_for(name: &str, a: &Value) -> Result<Action> {
    let s = |k: &str| a[k].as_str().map(str::to_string);
    let need = |k: &str| s(k).ok_or_else(|| anyhow::anyhow!("{k} is required"));
    let f = |k: &str| a[k].as_f64();
    let point = |k: &str| -> Result<Value> {
        match a[k].as_array().map(|p| p.iter().filter_map(Value::as_f64).collect::<Vec<_>>()) {
            Some(p) if p.len() == 2 => Ok(json!([gui::num(p[0]), gui::num(p[1])])),
            _ => anyhow::bail!("{k} is [x, y]"),
        }
    };
    let guest = |verb, args| Ok(Action::Guest { verb, args });
    match name {
        "gui_session_new" => Ok(Action::SessionNew { clone: a["clone"] == true }),
        "gui_session_end" => Ok(Action::SessionEnd { session: s("session") }),
        "gui_session_list" => Ok(Action::SessionList),
        "gui_push" => Ok(Action::Push { path: pushable(&need("path")?)?, to: s("to") }),
        "gui_open" => {
            let args = a["args"].as_array().cloned().unwrap_or_default();
            guest("open", json!({"app": need("app")?, "args": args, "wait_s": a["wait_s"].as_u64().unwrap_or(20)}))
        }
        "gui_screenshot" => guest("screenshot", json!({"window": s("window"), "format": "png"})),
        "gui_click" => {
            let at = match (f("x"), f("y"), s("label")) {
                (_, _, Some(label)) if !label.trim().is_empty() => At::Label(label),
                (Some(x), Some(y), _) => At::Point(x, y),
                _ => anyhow::bail!("click needs a label, or x and y"),
            };
            let count = a["count"].as_u64().unwrap_or(1).clamp(1, 3) as u32;
            guest("click", gui::click_args(&at, a["button"] == "right", count))
        }
        "gui_type" => guest("type", json!({"text": need("text")?})),
        "gui_key" => guest("key", json!({"combo": need("combo")?})),
        "gui_scroll" => {
            let (Some(x), Some(y)) = (f("x"), f("y")) else { anyhow::bail!("scroll needs x and y") };
            let (dx, dy) = (f("dx").unwrap_or(0.0), f("dy").unwrap_or(0.0));
            guest("scroll", json!({"x": gui::num(x), "y": gui::num(y), "dx": gui::num(dx), "dy": gui::num(dy)}))
        }
        "gui_drag" => guest("drag", json!({"from": point("from")?, "to": point("to")?})),
        "gui_tree" => guest("tree", json!({"app": s("app"), "depth": a["depth"].as_u64().unwrap_or(12)})),
        "gui_wait" => {
            let timeout = a["timeout_s"].as_u64().unwrap_or(20);
            match s("label") {
                Some(label) => guest("wait", json!({"label": label, "timeout_s": timeout})),
                None if a["idle"] == true => guest("wait", json!({"idle": true, "timeout_s": timeout})),
                None => anyhow::bail!("wait needs a label, or idle=true"),
            }
        }
        "gui_windows" => guest("windows", json!({})),
        "gui_focus" => guest("focus", json!({"app": need("app")?})),
        "gui_quit" => guest("quit", json!({"app": need("app")?})),
        "gui_note" => Ok(Action::Note { text: need("text")? }),
        _ => anyhow::bail!("unknown tool {name}"),
    }
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::tests::Fake;

    fn rpc(server: &mut Server, msg: Value) -> Value {
        serde_json::from_str(&server.handle(&msg.to_string()).expect("a reply")).unwrap()
    }

    #[test]
    fn base64_matches_the_rfc() {
        for (raw, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(raw.as_bytes()), enc);
        }
    }

    #[test]
    fn push_refuses_secrets() {
        let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
        let tmp = std::env::temp_dir().join(format!("berth-push-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(".build/debug/App.app")).unwrap();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        assert!(pushable(tmp.join(".build/debug/App.app").to_str().unwrap()).is_ok());
        assert!(pushable(tmp.join(".git").to_str().unwrap()).is_err());
        assert!(pushable(home.to_str().unwrap()).is_err());
        assert!(pushable("/").is_err());
        if home.join("Library/Preferences").exists() {
            assert!(pushable(home.join("Library/Preferences").to_str().unwrap()).is_err());
        }
        if home.join(".ssh").exists() {
            assert!(pushable(home.join(".ssh").to_str().unwrap()).is_err());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn initialize_list_and_errors() {
        let fake = Fake::new(json!({}));
        let mut server = Server::new(&fake.paths, Who { kind: "terminal".into(), ..Who::default() });
        let init = rpc(
            &mut server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "2.1"}}}),
        );
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert_eq!(server.who.kind, "claude-code");
        // An older client gets its own version; an unknown one ours.
        let old = rpc(&mut server, json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}}));
        assert_eq!(old["result"]["protocolVersion"], "2024-11-05");
        let odd = rpc(&mut server, json!({"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}));
        assert_eq!(odd["result"]["protocolVersion"], PROTOCOL_VERSION);

        assert_eq!(server.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
        assert_eq!(server.handle(""), None);
        let list = rpc(&mut server, json!({"jsonrpc": "2.0", "id": "a", "method": "tools/list"}));
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOLS.len());
        let shot = tools.iter().find(|t| t["name"] == "gui_screenshot").unwrap();
        assert_eq!(shot["inputSchema"]["type"], "object");
        assert!(shot["inputSchema"]["properties"]["target"].is_object());
        let push = tools.iter().find(|t| t["name"] == "gui_push").unwrap();
        assert_eq!(push["inputSchema"]["required"], json!(["path"]));

        assert_eq!(rpc(&mut server, json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}))["error"]["code"], -32601);
        assert_eq!(serde_json::from_str::<Value>(&server.handle("{nope").unwrap()).unwrap()["error"]["code"], -32700);
        let unknown = rpc(&mut server, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "rm_rf"}}));
        assert_eq!(unknown["error"]["code"], -32602);
        assert_eq!(rpc(&mut server, json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}))["result"], json!({}));
    }

    #[test]
    fn tools_are_off_without_the_flags() {
        let fake = Fake::new(json!({}));
        let mut server = Server::new(&fake.paths, Who { kind: "terminal".into(), ..Who::default() });
        let reply = rpc(
            &mut server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "gui_windows", "arguments": {"target": "mac:test"}}}),
        );
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains("\"mac\": true"));
        let bad = rpc(&mut server, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "gui_click", "arguments": {}}}));
        assert_eq!(bad["result"]["isError"], true);
    }

    #[test]
    fn a_session_screenshot_and_click_through_mcp() {
        let fake = Fake::new(json!({"mac": true}));
        fake.mac_guest("test");
        let mut server = Server::new(&fake.paths, Who { kind: "terminal".into(), ..Who::default() });
        server.target = None;
        server.session = None;
        rpc(&mut server, json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"clientInfo": {"name": "cursor-vscode"}}}));
        let call = |server: &mut Server, id: u32, name: &str, args: Value| {
            rpc(server, json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}}))
        };
        let new = call(&mut server, 1, "gui_session_new", json!({"target": "mac:test"}));
        assert_eq!(new["result"]["isError"], false, "{new}");
        assert_eq!(server.session.as_deref(), Some("s-1234"));

        let shot = call(&mut server, 2, "gui_screenshot", json!({}));
        let blocks = shot["result"]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["mimeType"], "image/png");
        assert_eq!(blocks[0]["data"], base64(crate::gui::tests::PNG));
        assert!(blocks[1]["text"].as_str().unwrap().contains("/logs/gui/shots/"));

        let click = call(&mut server, 3, "gui_click", json!({"label": "Save"}));
        assert!(click["result"]["content"][0]["text"].as_str().unwrap().contains("AXButton"));
        let seen = fake.requests();
        let last = seen.last().unwrap();
        assert_eq!((last["verb"].as_str(), last["session"].as_str()), (Some("click"), Some("s-1234")));
        assert_eq!(last["args"], json!({"label": "Save", "button": "left", "count": 1}));

        let end = call(&mut server, 4, "gui_session_end", json!({}));
        assert_eq!(end["result"]["isError"], false, "{end}");
        assert_eq!(server.session, None);
        let log = fake.log_lines();
        assert!(log.iter().all(|l| l["who"]["kind"] == "cursor"));
    }
}
