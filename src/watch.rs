//! File change notifications for bind mounts.
//!
//! virtiofs doesn't carry macOS file events into the guest, so watchers in
//! containers (Vite, webpack, nodemon, ...) never see edits made on the Mac.
//! We watch the Mac-side sources of running containers' bind mounts with
//! FSEvents and have the guest agent "touch" each changed path: a chmod to its
//! current mode, which raises inotify IN_ATTRIB in the guest and refreshes the
//! cached attributes. Our chmod comes back from macOS as another event (FSEvents
//! coalesces flags, so it isn't reliably "metadata only"). To avoid a loop we
//! forward a path only when its state changed: existence, mtime or size. A
//! chmod changes none of them; real edits, creations and deletions do.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::daemon::guest_stream;

const BATCH: Duration = Duration::from_millis(100);
/// Per batch; a huge burst (git checkout, npm install) is truncated, and the
/// directory touches still prompt watchers to rescan.
const MAX_PATHS: usize = 2000;

struct State {
    watcher: RecommendedWatcher,
    roots: BTreeSet<PathBuf>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Start the event pump. Call once from the daemon.
pub fn start() {
    let (tx, rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && let Some(paths) = relevant(&event)
        {
            let _ = tx.send(paths);
        }
    });
    match watcher {
        Ok(watcher) => {
            *STATE.lock().unwrap() = Some(State { watcher, roots: BTreeSet::new() });
            tokio::spawn(forward(rx));
        }
        Err(err) => eprintln!("watch: file events unavailable: {err}"),
    }
}

/// Paths worth telling the guest about, or None to ignore the event. Pure
/// metadata changes are skipped: that is what our own touch looks like.
fn relevant(event: &notify::Event) -> Option<Vec<PathBuf>> {
    let structural = match event.kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => true,
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any) => false,
        _ => return None,
    };
    let mut paths = event.paths.clone();
    if structural {
        // Directory watchers learn about new/removed entries from their dir.
        paths.extend(event.paths.iter().filter_map(|p| p.parent().map(Path::to_path_buf)));
    }
    Some(paths)
}

/// What a path looked like when we last forwarded it: None if absent.
type Signature = Option<(std::time::SystemTime, u64)>;

fn signature(path: &Path) -> Signature {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some((meta.modified().ok()?, if meta.is_dir() { 0 } else { meta.len() }))
}

/// Keep paths whose signature changed since we last sent them, recording the
/// new ones. Bounded so a long session can't grow it without limit.
fn changed(seen: &mut HashMap<PathBuf, Signature>, paths: BTreeSet<PathBuf>) -> Vec<PathBuf> {
    if seen.len() > 200_000 {
        seen.clear();
    }
    paths
        .into_iter()
        .filter(|p| {
            let sig = signature(p);
            seen.insert(p.clone(), sig) != Some(sig)
        })
        .collect()
}

/// Batch events and hand them to the agent.
async fn forward(mut rx: mpsc::UnboundedReceiver<Vec<PathBuf>>) {
    let mut seen = HashMap::new();
    while let Some(first) = rx.recv().await {
        let mut batch: BTreeSet<PathBuf> = first.into_iter().collect();
        tokio::time::sleep(BATCH).await;
        while let Ok(more) = rx.try_recv() {
            batch.extend(more);
        }
        let paths: Vec<String> = changed(&mut seen, batch)
            .into_iter()
            .take(MAX_PATHS)
            .filter_map(|p| p.to_str().map(str::to_string))
            .collect();
        if paths.is_empty() {
            continue;
        }
        if let Err(err) = touch(&paths).await {
            eprintln!("watch: {err:#}");
        }
    }
}

async fn touch(paths: &[String]) -> anyhow::Result<()> {
    let mut guest = guest_stream("touch").await?;
    guest.write_all(paths.join("\n").as_bytes()).await?;
    guest.shutdown().await?;
    Ok(())
}

/// Watch exactly these roots (bind-mount sources under the shared home).
pub fn set_roots(wanted: BTreeSet<PathBuf>) {
    let wanted = outermost(wanted);
    let mut guard = STATE.lock().unwrap();
    let Some(state) = guard.as_mut() else { return };
    for gone in state.roots.difference(&wanted).cloned().collect::<Vec<_>>() {
        let _ = state.watcher.unwatch(&gone);
        state.roots.remove(&gone);
    }
    for root in wanted {
        if state.roots.contains(&root) {
            continue;
        }
        match state.watcher.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => {
                eprintln!("watch: {}", root.display());
                state.roots.insert(root);
            }
            Err(err) => eprintln!("watch: {}: {err}", root.display()),
        }
    }
}

/// Drop roots nested inside other roots; one recursive watch covers them.
fn outermost(roots: BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    let mut kept: BTreeSet<PathBuf> = BTreeSet::new();
    for root in roots {
        // BTreeSet order puts a parent before its children.
        if !kept.iter().any(|k| root.starts_with(k)) {
            kept.insert(root);
        }
    }
    kept
}

/// Bind-mount sources worth watching: directories inside the shared home.
pub fn bind_roots<'a>(sources: impl Iterator<Item = &'a str>, home: &Path) -> BTreeSet<PathBuf> {
    sources
        .map(PathBuf::from)
        // A file mount is watched through its directory.
        .map(|p| if p.is_dir() { p } else { p.parent().map_or(p.clone(), Path::to_path_buf) })
        // Never the whole home directory (a file mounted straight from ~).
        .filter(|p| p.starts_with(home) && p != home)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, MetadataKind};

    fn event(kind: EventKind, path: &str) -> notify::Event {
        notify::Event::new(kind).add_path(PathBuf::from(path))
    }

    #[test]
    fn ignores_metadata_only_events() {
        // Our own chmod in the guest shows up like this: must not loop.
        assert!(relevant(&event(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)), "/u/p/a.js")).is_none());
        assert!(relevant(&event(EventKind::Access(notify::event::AccessKind::Any), "/u/p/a.js")).is_none());
    }

    #[test]
    fn content_edits_touch_the_file_and_creations_the_parent_too() {
        let edit = relevant(&event(EventKind::Modify(ModifyKind::Data(DataChange::Content)), "/u/p/a.js")).unwrap();
        assert_eq!(edit, vec![PathBuf::from("/u/p/a.js")]);
        let new = relevant(&event(EventKind::Create(CreateKind::File), "/u/p/b.js")).unwrap();
        assert_eq!(new, vec![PathBuf::from("/u/p/b.js"), PathBuf::from("/u/p")]);
    }

    #[test]
    fn echoes_of_our_own_chmod_are_dropped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("berth-watch-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.js");
        std::fs::write(&file, "v1").unwrap();
        let mut seen = HashMap::new();
        let one = |p: &Path| [p.to_path_buf()].into_iter().collect::<BTreeSet<_>>();
        assert_eq!(changed(&mut seen, one(&file)).len(), 1, "first sighting is forwarded");
        // What the agent's touch does; it must not be forwarded again.
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(changed(&mut seen, one(&file)).is_empty(), "chmod echo is dropped");
        // A real edit (size changes) is forwarded.
        std::fs::write(&file, "v2 longer").unwrap();
        assert_eq!(changed(&mut seen, one(&file)).len(), 1);
        // Deletion is a change too.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(changed(&mut seen, one(&file)).len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn nested_roots_collapse() {
        let roots: BTreeSet<PathBuf> = ["/u/app", "/u/app/src", "/u/other"].iter().map(PathBuf::from).collect();
        let kept: Vec<_> = outermost(roots).into_iter().collect();
        assert_eq!(kept, vec![PathBuf::from("/u/app"), PathBuf::from("/u/other")]);
    }

    #[test]
    fn only_paths_under_home() {
        let home = std::env::temp_dir().join(format!("berth-watch-test-{}", std::process::id()));
        std::fs::create_dir_all(home.join("app")).unwrap();
        std::fs::write(home.join(".env"), "").unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_string();
        let sources = [s(&home.join("app")), "/var/run/docker.sock".into(), s(&home), s(&home.join(".env"))];
        let roots = bind_roots(sources.iter().map(String::as_str), &home);
        // Only the app dir: not /var/run, not ~ itself, not ~ via ~/.env.
        assert_eq!(roots.into_iter().collect::<Vec<_>>(), vec![home.join("app")]);
        std::fs::remove_dir_all(home).ok();
    }
}
