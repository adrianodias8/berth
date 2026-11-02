//! Host-side `berth gui` sessions (docs/gui-protocol.md, "Sessions"):
//! ~/.berth/gui/sessions.json, and the queue for macOS VM slots in
//! ~/.berth/gui/queue.json. Several agents run `berth gui` at once, so every
//! read-change-write of either file happens under ~/.berth/gui/lock.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::gui_log::{self, Who};
use crate::paths::Paths;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Record {
    pub session: String,
    pub target: String,
    pub agent: Who,
    /// A throwaway macOS VM made for this session, deleted when it ends.
    pub clone: bool,
    pub started: String,
    pub last_used: String,
    /// The latest `berth gui note` in this session.
    pub note: Option<String>,
    /// Linux: what the guest started (agent, app or desktop), how its
    /// compositor renders (gles2, pixman), and for a desktop the VM scanout
    /// that shows it (Berth.app's Display window reads these).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renderer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanout: Option<u32>,
}

/// A `session new --clone` waiting for a macOS VM slot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Waiting {
    pub ticket: String,
    pub base: String,
    pub agent: Who,
    pub since: String,
    /// The waiting `berth gui` process: if it's gone, so is its place.
    pub pid: u32,
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.root.join("gui")
}

/// Held while reading and writing sessions.json or queue.json.
pub struct Lock(#[allow(dead_code)] std::fs::File);

pub fn lock(paths: &Paths) -> Result<Lock> {
    std::fs::create_dir_all(dir(paths))?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir(paths).join("lock"))?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("locking ~/.berth/gui");
    }
    Ok(Lock(file))
}

fn load<T: serde::de::DeserializeOwned>(paths: &Paths, name: &str) -> Vec<T> {
    // A file that won't parse is treated as empty rather than wedging every call.
    std::fs::read(dir(paths).join(name)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save<T: Serialize>(paths: &Paths, name: &str, items: &[T]) -> Result<()> {
    let path = dir(paths).join(name);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(items)? + "\n")?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_sessions(paths: &Paths, _lock: &Lock) -> Vec<Record> {
    load(paths, "sessions.json")
}

pub fn save_sessions(paths: &Paths, _lock: &Lock, sessions: &[Record]) -> Result<()> {
    save(paths, "sessions.json", sessions)
}

pub fn load_queue(paths: &Paths, _lock: &Lock) -> Vec<Waiting> {
    load(paths, "queue.json")
}

pub fn save_queue(paths: &Paths, _lock: &Lock, queue: &[Waiting]) -> Result<()> {
    save(paths, "queue.json", queue)
}

/// The record of a session, if the host knows it.
pub fn find(paths: &Paths, session: &str) -> Result<Option<Record>> {
    let lock = lock(paths)?;
    Ok(load_sessions(paths, &lock).into_iter().find(|r| r.session == session))
}

/// The desktop session showing on `machine`'s `scanout`, newest first.
pub fn desktop_on(paths: &Paths, machine: &str, scanout: u32) -> Option<String> {
    let lock = lock(paths).ok()?;
    let target = format!("linux:{machine}");
    let mut on: Vec<Record> = load_sessions(paths, &lock).into_iter().filter(|r| r.target == target && r.scanout == Some(scanout)).collect();
    on.sort_by(|a, b| b.started.cmp(&a.started));
    on.into_iter().next().map(|r| r.session)
}

/// Mark a session used now (and remember its note).
pub fn touch(paths: &Paths, session: &str, note: Option<&str>) -> Result<()> {
    let lock = lock(paths)?;
    let mut sessions = load_sessions(paths, &lock);
    let Some(record) = sessions.iter_mut().find(|r| r.session == session) else { return Ok(()) };
    record.last_used = gui_log::now();
    if let Some(note) = note {
        record.note = Some(note.to_string());
    }
    save_sessions(paths, &lock, &sessions)
}

/// "gui_idle_minutes" in config.json, 30 by default; 0 never expires.
pub fn idle_minutes(paths: &Paths) -> u64 {
    crate::config::value(paths, "gui_idle_minutes").and_then(|v| v.as_u64()).unwrap_or(30)
}

/// Split sessions into those still in use and those idle longer than
/// `idle_secs` at `now`. A record whose time won't parse counts as fresh,
/// so a hand-edited file can't end everything.
pub fn split_idle(sessions: Vec<Record>, now: i64, idle_secs: i64) -> (Vec<Record>, Vec<Record>) {
    if idle_secs <= 0 {
        return (sessions, Vec::new());
    }
    sessions.into_iter().partition(|r| gui_log::parse_rfc3339(&r.last_used).is_none_or(|t| now - t <= idle_secs))
}

/// Take the idle sessions out of sessions.json; the caller ends them.
pub fn take_idle(paths: &Paths) -> Result<Vec<Record>> {
    let lock = lock(paths)?;
    let all = load_sessions(paths, &lock);
    let (keep, idle) = split_idle(all, gui_log::unix_now(), idle_minutes(paths) as i64 * 60);
    if !idle.is_empty() {
        save_sessions(paths, &lock, &keep)?;
    }
    Ok(idle)
}

/// Put back records take_idle took whose end failed (a clone VM that
/// couldn't be deleted), so they stay tracked and are tried again.
pub fn restore(paths: &Paths, records: &[Record]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let lock = lock(paths)?;
    let mut sessions = load_sessions(paths, &lock);
    for record in records {
        if !sessions.iter().any(|r| r.session == record.session) {
            sessions.push(record.clone());
        }
    }
    save_sessions(paths, &lock, &sessions)
}

// ---- Sessions that ended without `session end` ----
//
// A session dies with what holds it: every Linux session with berth's VM
// (they're compositors in machines inside it) or with its machine, a macOS
// session with its VM. Their records are dropped where berth knows that
// happened (the daemon on boot and exit and after machine stop|delete|
// restart|rename, `berth mac stop|delete`), by `session list`, which looks
// at each target once, and by a call the guest answers "no session".

/// What one look at a target found.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    /// The guest's own list of its live sessions (Linux).
    Live(Vec<String>),
    /// The VM or machine is gone or stopped: none of its sessions survive.
    Down,
    /// Up, or no telling (berth isn't running, a timeout): keep its records.
    /// berth-gui-mac doesn't hold verbs to a session, so a running macOS VM
    /// is as far as a look goes.
    Unsure,
}

pub fn is_gone(record: &Record, seen: &Seen) -> bool {
    match seen {
        Seen::Live(ids) => !ids.contains(&record.session),
        Seen::Down => true,
        Seen::Unsure => false,
    }
}

/// A macOS VM is down if it's gone or its process isn't running.
pub fn mac_seen(exists: bool, running: bool) -> Seen {
    if exists && running { Seen::Unsure } else { Seen::Down }
}

/// A Linux machine from its guest's `session.list`: the result, or the
/// refusal's text. A reply without a list tells nothing.
pub fn linux_seen(machine: &str, listed: std::result::Result<&serde_json::Value, &str>) -> Seen {
    match listed {
        Ok(result) => match result["sessions"].as_array() {
            Some(list) => Seen::Live(list.iter().filter_map(|s| s["session"].as_str().map(str::to_string)).collect()),
            None => Seen::Unsure,
        },
        Err(error) if machine_down(machine, error) => Seen::Down,
        Err(_) => Seen::Unsure,
    }
}

/// The Linux agent's refusals for a machine that isn't there or isn't
/// running (agent/src/gui/exec.rs, Machine::open).
fn machine_down(machine: &str, error: &str) -> bool {
    error == format!("no machine {machine}") || error.starts_with(&format!("machine {machine} isn't running"))
}

/// Whether a failed call's error shows its session is gone: the Linux agent
/// no longer knows it (agent/src/gui/mod.rs, resolve), or its machine is
/// down. A macOS guest never refuses a session; its VM is looked at instead.
pub fn gone_by_error(record: &Record, error: &str) -> bool {
    match record.target.split_once(':') {
        Some(("linux", machine)) => error == format!("no session {} in {machine}", record.session) || machine_down(machine, error),
        _ => false,
    }
}

/// Take the records `gone` picks out of sessions.json, under the lock;
/// returns them (a clone among them is the caller's to delete). Without a
/// sessions.json there's nothing to drop, and ~/.berth/gui isn't made.
pub fn drop_where(paths: &Paths, gone: impl Fn(&Record) -> bool) -> Result<Vec<Record>> {
    if !dir(paths).join("sessions.json").exists() {
        return Ok(Vec::new());
    }
    let lock = lock(paths)?;
    let (dropped, keep): (Vec<Record>, Vec<Record>) = load_sessions(paths, &lock).into_iter().partition(|r| gone(r));
    if !dropped.is_empty() {
        save_sessions(paths, &lock, &keep)?;
    }
    Ok(dropped)
}

/// Whether a record is a session on Linux `machine`, or on any machine.
pub fn on_linux(record: &Record, machine: Option<&str>) -> bool {
    record.target.strip_prefix("linux:").is_some_and(|m| machine.is_none_or(|want| m == want))
}

/// Drop the Linux sessions on `machine` (None: every machine), which died
/// with it. Linux sessions have no clone to delete.
pub fn forget_linux(paths: &Paths, machine: Option<&str>) -> Result<Vec<Record>> {
    drop_where(paths, |r| on_linux(r, machine))
}

/// Drop the sessions on macOS VM `vm`; its clone sessions only with
/// `clones` (the VM is deleted), else a stopped clone would be forgotten
/// and never deleted.
pub fn forget_mac(paths: &Paths, vm: &str, clones: bool) -> Result<Vec<Record>> {
    let target = format!("mac:{vm}");
    drop_where(paths, |r| r.target == target && (clones || !r.clone))
}

pub fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// Drop waiters whose process is gone; `ahead` is how many wait before
/// `ticket` (None if it isn't queued).
pub fn prune_and_place(queue: &mut Vec<Waiting>, ticket: &str, alive: impl Fn(u32) -> bool) -> Option<usize> {
    queue.retain(|w| w.ticket == ticket || alive(w.pid));
    queue.iter().position(|w| w.ticket == ticket)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(session: &str, last_used: &str) -> Record {
        Record {
            session: session.into(),
            target: "mac:test".into(),
            agent: Who::default(),
            clone: false,
            started: last_used.into(),
            last_used: last_used.into(),
            note: None,
            kind: None,
            renderer: None,
            scanout: None,
        }
    }

    #[test]
    fn idle_sessions_expire() {
        let now = gui_log::parse_rfc3339("2026-11-02T10:00:00Z").unwrap();
        let all = vec![record("fresh", "2026-11-02T09:45:00Z"), record("idle", "2026-11-02T09:29:59Z"), record("odd", "yesterday")];
        let (keep, idle) = split_idle(all.clone(), now, 30 * 60);
        assert_eq!(keep.iter().map(|r| r.session.as_str()).collect::<Vec<_>>(), ["fresh", "odd"]);
        assert_eq!(idle.iter().map(|r| r.session.as_str()).collect::<Vec<_>>(), ["idle"]);
        let (keep, idle) = split_idle(all, now, 0);
        assert_eq!((keep.len(), idle.len()), (3, 0));
    }

    #[test]
    fn the_queue_is_first_come_first_served_and_forgets_the_dead() {
        let w = |t: &str, pid| Waiting { ticket: t.into(), base: "base".into(), agent: Who::default(), since: String::new(), pid };
        let mut queue = vec![w("a", 1), w("b", 2), w("me", 3), w("c", 4)];
        assert_eq!(prune_and_place(&mut queue, "me", |_| true), Some(2));
        // b's process died: one fewer ahead.
        assert_eq!(prune_and_place(&mut queue, "me", |pid| pid != 2), Some(1));
        assert_eq!(queue.len(), 3);
        assert_eq!(prune_and_place(&mut queue, "gone", |_| true), None);
    }

    fn on(session: &str, target: &str, clone: bool) -> Record {
        Record { target: target.into(), clone, ..record(session, "2026-11-02T09:00:00Z") }
    }

    #[test]
    fn a_look_at_a_target_tells_which_sessions_are_gone() {
        let r = on("s-1", "linux:desk", false);
        assert!(!is_gone(&r, &Seen::Live(vec!["s-0".into(), "s-1".into()])));
        assert!(is_gone(&r, &Seen::Live(vec!["s-0".into()])));
        assert!(is_gone(&r, &Seen::Down));
        assert!(!is_gone(&r, &Seen::Unsure));

        assert_eq!(mac_seen(true, true), Seen::Unsure);
        assert_eq!(mac_seen(true, false), Seen::Down);
        assert_eq!(mac_seen(false, false), Seen::Down);

        let listed = serde_json::json!({"sessions": [{"session": "s-1", "agent": "claude-code"}, {"session": "s-2"}]});
        assert_eq!(linux_seen("desk", Ok(&listed)), Seen::Live(vec!["s-1".into(), "s-2".into()]));
        assert_eq!(linux_seen("desk", Ok(&serde_json::json!({}))), Seen::Unsure);
        assert_eq!(linux_seen("desk", Err("no machine desk")), Seen::Down);
        assert_eq!(linux_seen("desk", Err("machine desk isn't running: berth machine start desk")), Seen::Down);
        assert_eq!(linux_seen("desk", Err("no machine desktop")), Seen::Unsure);
        assert_eq!(linux_seen("desk", Err("berth is not running (berth start)")), Seen::Unsure);
        assert_eq!(linux_seen("desk", Err("Linux GUI sessions are off: …")), Seen::Unsure);
    }

    #[test]
    fn a_refusal_naming_the_session_means_its_gone() {
        // As seen live: the VM restarted under linux:desk's session s-cabf81.
        let r = on("s-cabf81", "linux:desk", false);
        assert!(gone_by_error(&r, "no session s-cabf81 in desk"));
        assert!(gone_by_error(&r, "machine desk isn't running: berth machine start desk"));
        assert!(!gone_by_error(&r, "no session s-other in desk"));
        assert!(!gone_by_error(&r, "no element labelled \"Save\""));
        assert!(!gone_by_error(&on("s-1", "mac:test", false), "no session s-1 in test"));
    }

    #[test]
    fn dead_sessions_are_dropped_under_the_lock() {
        let root = std::env::temp_dir().join(format!("berth-sessions-{}", gui_log::new_id()));
        let paths = Paths { home: root.clone(), root: root.clone() };
        // No sessions.json: nothing to do, and nothing made under ~/.berth.
        assert!(forget_linux(&paths, None).unwrap().is_empty());
        assert!(!root.exists());
        {
            let lock = lock(&paths).unwrap();
            let all = [
                on("s-a", "linux:desk", false),
                on("s-b", "linux:desk-2", false),
                on("s-c", "linux:web", false),
                on("s-d", "mac:test", false),
                on("s-e", "mac:test", true),
                on("s-f", "mac:other", false),
            ];
            save_sessions(&paths, &lock, &all).unwrap();
        }
        let names = |rs: Vec<Record>| rs.into_iter().map(|r| r.session).collect::<Vec<_>>();
        let left = || names(load_sessions(&paths, &lock(&paths).unwrap()));
        assert_eq!(names(forget_linux(&paths, Some("desk")).unwrap()), ["s-a"]);
        // A stopped VM's clone sessions stay, to be deleted along with their VM.
        assert_eq!(names(forget_mac(&paths, "test", false).unwrap()), ["s-d"]);
        assert_eq!(left(), ["s-b", "s-c", "s-e", "s-f"]);
        assert_eq!(names(forget_linux(&paths, None).unwrap()), ["s-b", "s-c"]);
        assert_eq!(names(forget_mac(&paths, "test", true).unwrap()), ["s-e"]);
        assert!(forget_mac(&paths, "test", true).unwrap().is_empty());
        assert_eq!(left(), ["s-f"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sessions_persist_under_the_lock() {
        let root = std::env::temp_dir().join(format!("berth-sessions-{}", gui_log::new_id()));
        let paths = Paths { home: root.clone(), root: root.clone() };
        {
            let lock = lock(&paths).unwrap();
            save_sessions(&paths, &lock, &[record("s-1", "2026-11-02T09:00:00Z")]).unwrap();
        }
        touch(&paths, "s-1", Some("checking Save")).unwrap();
        let r = find(&paths, "s-1").unwrap().unwrap();
        assert_eq!(r.note.as_deref(), Some("checking Save"));
        assert_ne!(r.last_used, "2026-11-02T09:00:00Z");
        assert!(find(&paths, "s-2").unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
