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
    gui_log::config_value(paths, "gui_idle_minutes").and_then(|v| v.as_u64()).unwrap_or(30)
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
