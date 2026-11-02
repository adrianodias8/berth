//! The `berth gui` activity log (docs/gui-protocol.md, "Activity log"): one
//! JSON line when a command starts and one when it ends, both with the same
//! id, in ~/.berth/logs/gui/<YYYY-MM-DD>.jsonl (UTC dates, like the times in
//! the lines). Berth.app reads these files for its Agents section.
//!
//! Each line says who ran the command. A coding agent leaves traces in the
//! environment it gives its shells: Claude Code sets CLAUDECODE=1 and
//! CLAUDE_CODE_SESSION_ID (and CLAUDE_PID), Codex sets CODEX_* (CODEX_SANDBOX
//! and friends), Cursor's terminals CURSOR_* (CURSOR_TRACE_ID). When the
//! environment says nothing, an ancestor process named `claude`, `codex` or
//! `cursor…` (read with proc_pidinfo) does. Else it was someone in a
//! terminal. `berth mcp` adds what the MCP client says about itself.
//!
//! Chat titles are read from Claude Code's own transcripts
//! (~/.claude/projects/*/<session>.jsonl), only with "gui_titles": true:
//! they stay on this Mac, but they're the user's words.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::Paths;

/// Who ran a command (the log's `who`, and a session's `agent`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Who {
    /// claude-code, cursor, codex, mcp or terminal.
    pub kind: String,
    /// The agent's chat or session id, when it tells us.
    pub chat: Option<String>,
    pub title: Option<String>,
    pub pid: Option<u32>,
    pub cwd: Option<String>,
}

/// Who is running this process, with the chat title if that's turned on.
/// Berth.app's own screenshot refreshes (it sets BERTH_GUI_WHO to this).
pub const APP_KIND: &str = "berth-app";

pub fn who(paths: &Paths) -> Who {
    let env: HashMap<String, String> = std::env::vars().collect();
    let parent = unsafe { libc::getppid() } as u32;
    let mut who = detect(&env, &ancestors(parent), parent);
    who.cwd = std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned());
    if who.kind == "claude-code" && crate::config::flag(paths, "gui_titles") {
        who.title = who.chat.as_deref().and_then(|chat| claude_title(&paths.home, chat));
    }
    who
}

/// Agent variables, not a user's own settings for that tool (CODEX_HOME,
/// CURSOR_API_KEY live in shell profiles and say nothing about who runs us).
fn has_agent_var(env: &HashMap<String, String>, prefix: &str) -> bool {
    env.keys().any(|k| k.starts_with(prefix) && !k.ends_with("_HOME") && !k.ends_with("_API_KEY") && !k.ends_with("_CONFIG"))
}

fn first_var(env: &HashMap<String, String>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| env.get(*n).filter(|v| !v.is_empty()).cloned())
}

/// The kind of agent an ancestor process name points at.
fn kind_of_process(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    // Exactly `claude`: "Claude" is the desktop app, which also runs things
    // that aren't Claude Code.
    if name == "claude" || lower.starts_with("claude-code") {
        Some("claude-code")
    } else if lower == "codex" || lower.starts_with("codex-") {
        Some("codex")
    } else if lower.starts_with("cursor") {
        Some("cursor")
    } else {
        None
    }
}

/// Who, from an environment and the ancestors (pid, name), nearest first.
/// An agent inside an editor's terminal has both sets of variables: the
/// agent's come first.
pub fn detect(env: &HashMap<String, String>, ancestors: &[(u32, String)], parent: u32) -> Who {
    if env.get("BERTH_GUI_WHO").is_some_and(|v| v == APP_KIND) {
        return Who { kind: APP_KIND.into(), pid: Some(parent), ..Who::default() };
    }
    let ancestor = |kind: &str| ancestors.iter().find(|(_, name)| kind_of_process(name) == Some(kind)).map(|(pid, _)| *pid);
    let ai_agent = env.get("AI_AGENT").map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let kind = if env.contains_key("CLAUDECODE") || env.contains_key("CLAUDE_CODE_SESSION_ID") || ai_agent.starts_with("claude") {
        Some("claude-code")
    } else if has_agent_var(env, "CODEX_") || ai_agent.starts_with("codex") {
        Some("codex")
    } else if has_agent_var(env, "CURSOR_") || ai_agent.starts_with("cursor") {
        Some("cursor")
    } else {
        ancestors.iter().find_map(|(_, name)| kind_of_process(name))
    };
    let Some(kind) = kind else {
        return Who { kind: "terminal".into(), pid: Some(parent), ..Who::default() };
    };
    let (chat, pid) = match kind {
        "claude-code" => (
            first_var(env, &["CLAUDE_CODE_SESSION_ID"]),
            env.get("CLAUDE_PID").and_then(|p| p.parse().ok()).or_else(|| ancestor(kind)),
        ),
        "codex" => (first_var(env, &["CODEX_THREAD_ID", "CODEX_SESSION_ID", "CODEX_CONVERSATION_ID"]), ancestor(kind)),
        _ => (first_var(env, &["CURSOR_COMPOSER_ID", "CURSOR_CHAT_ID", "CURSOR_SESSION_ID", "CURSOR_TRACE_ID"]), ancestor(kind)),
    };
    Who { kind: kind.into(), chat, pid: pid.or(Some(parent)), ..Who::default() }
}

/// This process's ancestors from `pid` up, nearest first.
fn ancestors(mut pid: u32) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    while pid > 1 && out.len() < 32 {
        let Some((ppid, name)) = proc_info(pid) else { break };
        out.push((pid, name));
        if ppid == pid {
            break;
        }
        pid = ppid;
    }
    out
}

/// (parent pid, name) of a process; the name is the 32-byte one when set,
/// else the 16-byte command name.
fn proc_info(pid: u32) -> Option<(u32, String)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if n != size {
        return None;
    }
    let text = |chars: &[libc::c_char]| {
        let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let name = text(&info.pbi_name);
    Some((info.pbi_ppid, if name.is_empty() { text(&info.pbi_comm) } else { name }))
}

/// A Claude Code chat's title: the one the user gave it, else the one Claude
/// Code made up, else its first prompt.
pub fn claude_title(home: &Path, session: &str) -> Option<String> {
    if session.is_empty() || !session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    let projects = home.join(".claude/projects");
    let file = std::fs::read_dir(projects).ok()?.flatten().map(|d| d.path().join(format!("{session}.jsonl"))).find(|p| p.is_file())?;
    title_from_transcript(std::io::BufReader::new(std::fs::File::open(file).ok()?))
}

fn title_from_transcript(reader: impl BufRead) -> Option<String> {
    let (mut custom, mut ai, mut prompt) = (None, None, None);
    for line in reader.lines().map_while(Result::ok) {
        // Most lines are tool output: skip them without parsing.
        let interesting = line.contains("\"custom-title\"") || line.contains("\"ai-title\"") || (prompt.is_none() && line.contains("\"user\""));
        if !interesting {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
        match entry["type"].as_str() {
            Some("custom-title") => custom = entry["customTitle"].as_str().map(str::to_string).or(custom),
            Some("ai-title") => ai = entry["aiTitle"].as_str().map(str::to_string).or(ai),
            Some("user") if prompt.is_none() && entry["isMeta"] != Value::Bool(true) => {
                let content = &entry["message"]["content"];
                let text = match content {
                    Value::String(s) => Some(s.clone()),
                    Value::Array(blocks) => blocks.iter().find(|b| b["type"] == "text").and_then(|b| b["text"].as_str()).map(str::to_string),
                    _ => None,
                };
                // Tags are commands, hooks and reminders, not the user typing.
                prompt = text.filter(|t| !t.trim().is_empty() && !t.trim_start().starts_with('<'));
            }
            _ => {}
        }
    }
    let title = custom.or(ai).or(prompt)?;
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(if title.chars().count() > 80 { title.chars().take(79).collect::<String>() + "…" } else { title })
}

/// One line of the log.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Line {
    pub id: String,
    pub phase: String,
    pub time: String,
    pub target: Option<String>,
    pub session: Option<String>,
    pub verb: String,
    pub args: Value,
    pub ok: Option<bool>,
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
    pub screenshot: Option<String>,
    pub hit: Option<Value>,
    pub who: Who,
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.root.join("logs/gui")
}

pub fn shots(paths: &Paths) -> PathBuf {
    dir(paths).join("shots")
}

/// The log holds typed text, prompts and screenshots: only the user may
/// read it (home folders are 0755 by default). Tightens an existing folder.
pub fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Append a line to its day's file in one write, so lines from agents
/// running at once don't interleave.
pub fn write(paths: &Paths, line: &Line) -> Result<()> {
    private_dir(&dir(paths))?;
    let day = line.time.get(..10).unwrap_or("unknown");
    let mut text = serde_json::to_string(line)?;
    text.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir(paths).join(format!("{day}.jsonl")))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// A random id for a command (and its screenshot's file name).
pub fn new_id() -> String {
    let mut bytes = [0u8; 8];
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub fn now() -> String {
    rfc3339(unix_now())
}

/// `2026-11-02T10:00:00Z`.
pub fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The reverse of `rfc3339` (only that exact shape).
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60 + sec)
}

// Howard Hinnant's civil date algorithms (public domain).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn claude_code_from_its_environment() {
        let e = env(&[("CLAUDECODE", "1"), ("CLAUDE_CODE_SESSION_ID", "04506a74-b497"), ("CLAUDE_PID", "25204"), ("CURSOR_TRACE_ID", "x")]);
        let who = detect(&e, &[(100, "zsh".into())], 100);
        assert_eq!((who.kind.as_str(), who.chat.as_deref(), who.pid), ("claude-code", Some("04506a74-b497"), Some(25204)));
    }

    #[test]
    fn codex_and_cursor_from_their_variables() {
        let codex = detect(&env(&[("CODEX_SANDBOX", "seatbelt"), ("CODEX_THREAD_ID", "t1")]), &[], 9);
        assert_eq!((codex.kind.as_str(), codex.chat.as_deref(), codex.pid), ("codex", Some("t1"), Some(9)));
        let cursor = detect(&env(&[("CURSOR_TRACE_ID", "abc")]), &[(50, "Cursor Helper (P".into())], 9);
        assert_eq!((cursor.kind.as_str(), cursor.chat.as_deref(), cursor.pid), ("cursor", Some("abc"), Some(50)));
    }

    #[test]
    fn user_settings_are_not_agents() {
        let who = detect(&env(&[("CODEX_HOME", "/x"), ("CURSOR_API_KEY", "k")]), &[(7, "zsh".into()), (6, "login".into())], 7);
        assert_eq!((who.kind.as_str(), who.chat, who.pid), ("terminal", None, Some(7)));
    }

    #[test]
    fn ancestors_when_the_environment_is_bare() {
        let tree = [(30, "zsh".to_string()), (20, "claude".to_string()), (10, "Claude".to_string())];
        let who = detect(&HashMap::new(), &tree, 30);
        assert_eq!((who.kind.as_str(), who.pid), ("claude-code", Some(20)));
        // The desktop app alone isn't Claude Code.
        let who = detect(&HashMap::new(), &[(10, "Claude".to_string())], 10);
        assert_eq!(who.kind, "terminal");
        let who = detect(&HashMap::new(), &[(5, "codex".to_string())], 5);
        assert_eq!(who.kind, "codex");
    }

    #[test]
    fn our_own_process_tree_reads() {
        let tree = ancestors(std::process::id());
        assert_eq!(tree.first().map(|t| t.0), Some(std::process::id()));
        assert!(!tree[0].1.is_empty());
    }

    #[test]
    fn titles_prefer_the_users_then_claudes_then_the_first_prompt() {
        let lines = [
            r#"{"type":"queue-operation"}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"<command-name>/init</command-name>"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"Fix   the\nlogin bug"}]}}"#,
            r#"{"type":"user","message":{"content":"second prompt"}}"#,
        ];
        let text = |extra: &[&str]| lines.iter().chain(extra).cloned().collect::<Vec<_>>().join("\n");
        assert_eq!(title_from_transcript(text(&[]).as_bytes()).as_deref(), Some("Fix the login bug"));
        let ai = r#"{"type":"ai-title","aiTitle":"Login bug fix"}"#;
        assert_eq!(title_from_transcript(text(&[ai]).as_bytes()).as_deref(), Some("Login bug fix"));
        let custom = r#"{"type":"custom-title","customTitle":"My chat"}"#;
        assert_eq!(title_from_transcript(text(&[custom, ai]).as_bytes()).as_deref(), Some("My chat"));
        assert_eq!(title_from_transcript("".as_bytes()), None);
    }

    #[test]
    fn titles_are_found_by_session_file() {
        let home = std::env::temp_dir().join(format!("berth-title-{}", new_id()));
        std::fs::create_dir_all(home.join(".claude/projects/-a-b")).unwrap();
        std::fs::write(home.join(".claude/projects/-a-b/abc-123.jsonl"), r#"{"type":"user","message":{"content":"hello there"}}"#).unwrap();
        assert_eq!(claude_title(&home, "abc-123").as_deref(), Some("hello there"));
        assert_eq!(claude_title(&home, "../x"), None);
        assert_eq!(claude_title(&home, "nope"), None);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn times_round_trip() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_793_613_600), "2026-11-02T10:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        for t in [0, 951_782_400, 1_793_613_600, 1_793_613_659, 4_102_444_799] {
            assert_eq!(parse_rfc3339(&rfc3339(t)), Some(t));
        }
        assert_eq!(parse_rfc3339("2026-11-02 10:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-13-02T10:00:00Z"), None);
    }

    #[test]
    fn a_line_has_the_documented_shape() {
        let root = std::env::temp_dir().join(format!("berth-log-{}", new_id()));
        let paths = Paths { home: root.clone(), root: root.clone() };
        let line = Line {
            id: "abc".into(),
            phase: "end".into(),
            time: "2026-11-02T10:00:00Z".into(),
            target: Some("mac:test".into()),
            session: Some("s-3f9a".into()),
            verb: "click".into(),
            args: serde_json::json!({"label": "Save"}),
            ok: Some(true),
            error: None,
            duration_ms: Some(120),
            screenshot: None,
            hit: Some(serde_json::json!({"role": "AXButton", "label": "Save", "frame": [1, 2, 3, 4]})),
            who: Who { kind: "claude-code".into(), chat: Some("c".into()), title: None, pid: Some(1), cwd: Some("/x".into()) },
        };
        write(&paths, &line).unwrap();
        write(&paths, &line).unwrap();
        let text = std::fs::read_to_string(dir(&paths).join("2026-11-02.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 2);
        let v: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["args", "duration_ms", "error", "hit", "id", "ok", "phase", "screenshot", "session", "target", "time", "verb", "who"]
        );
        let mut who: Vec<&str> = v["who"].as_object().unwrap().keys().map(String::as_str).collect();
        who.sort();
        assert_eq!(who, ["chat", "cwd", "kind", "pid", "title"]);
        assert_eq!(v["hit"]["role"], "AXButton");
        std::fs::remove_dir_all(root).unwrap();
    }
}
