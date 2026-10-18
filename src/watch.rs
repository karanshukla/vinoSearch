//! Keep the index current: inotify on every directory `index` would walk.
//!
//! One non-recursive watch per directory, added with the same skip rules as
//! indexing, so build output and hidden trees cost no watches (a recursive
//! watch would add one for every directory under `target/` and
//! `node_modules/`). Events are debounced and handed to the worker as paths.

use crate::index;
use crate::worker::Job;
use anyhow::Result;
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Quiet period before a batch of changes is indexed: editors write a file in
/// several steps, and builds touch many at once.
const DEBOUNCE: Duration = Duration::from_secs(2);
/// How often to look for roots added by `vinosearch index` while running.
const ROOTS_EVERY: Duration = Duration::from_secs(30);

struct State {
    watcher: RecommendedWatcher,
    roots: Vec<PathBuf>,
    watched: usize,
    warned_limit: bool,
}

impl State {
    fn watch_tree(&mut self, dir: &Path) {
        for entry in index::walk(dir).filter(|e| e.file_type().is_dir()) {
            match self
                .watcher
                .watch(entry.path(), RecursiveMode::NonRecursive)
            {
                Ok(()) => self.watched += 1,
                Err(e) => {
                    // ENOSPC: out of inotify watches (fs.inotify.max_user_watches).
                    if !self.warned_limit {
                        eprintln!("watch {}: {e}", entry.path().display());
                        self.warned_limit = true;
                    }
                }
            }
        }
    }

    fn add_new_roots(&mut self, conn: &rusqlite::Connection) {
        let Ok(roots) = index::roots(conn) else {
            return;
        };
        for root in roots {
            if !self.roots.contains(&root) && root.is_dir() {
                let before = self.watched;
                self.watch_tree(&root);
                eprintln!(
                    "watching {} ({} directories)",
                    root.display(),
                    self.watched - before
                );
                self.roots.push(root);
            }
        }
    }

    fn root_of(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .map(PathBuf::as_path)
            .find(|r| path.starts_with(r))
    }
}

/// Whether a change to `path` could alter the index.
fn relevant(root: &Path, path: &Path) -> bool {
    if index::skipped_under(root, path) {
        return false;
    }
    if path.is_dir() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        return !index::skipped_dir(&name);
    }
    // Gone paths may have been indexed files or directories; removal sorts it out.
    !path.exists() || index::wanted(path)
}

fn interesting(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        _ => false,
    }
}

pub fn spawn(jobs: mpsc::Sender<Job>) -> Result<std::thread::JoinHandle<()>> {
    let conn = index::open(&index::default_db()?)?;
    let (tx, events) = mpsc::channel();
    let watcher = notify::recommended_watcher(tx)?;
    Ok(std::thread::spawn(move || {
        let mut st = State {
            watcher,
            roots: Vec::new(),
            watched: 0,
            warned_limit: false,
        };
        st.add_new_roots(&conn);
        let mut pending: HashSet<PathBuf> = HashSet::new();
        let mut last_event = Instant::now();
        let mut last_roots = Instant::now();
        loop {
            match events.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(event)) if interesting(&event.kind) => {
                    for path in event.paths {
                        let Some(root) = st.root_of(&path).map(Path::to_path_buf) else {
                            continue;
                        };
                        if !relevant(&root, &path) {
                            continue;
                        }
                        if path.is_dir()
                            && matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
                        {
                            st.watch_tree(&path);
                        }
                        pending.insert(path);
                        last_event = Instant::now();
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("watch: {e}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if !pending.is_empty()
                && last_event.elapsed() >= DEBOUNCE
                && jobs.send(Job::Changed(pending.drain().collect())).is_err()
            {
                break;
            }
            if last_roots.elapsed() >= ROOTS_EVERY {
                st.add_new_roots(&conn);
                last_roots = Instant::now();
            }
        }
    }))
}
