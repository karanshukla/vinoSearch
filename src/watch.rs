//! Keep the index current: inotify on every directory `index` would walk.
//!
//! One non-recursive watch per directory, added with the same skip rules as
//! indexing, so build output and hidden trees cost no watches (a recursive
//! watch would add one for every directory under `target/` and
//! `node_modules/`). Events are debounced and handed to the worker as paths.

use crate::config::{Config, Excludes};
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
/// How often to look for roots added by `vinosearch index`, and re-read the
/// config, while running.
const ROOTS_EVERY: Duration = Duration::from_secs(30);

struct State {
    watcher: RecommendedWatcher,
    roots: Vec<PathBuf>,
    watched: usize,
    warned_limit: bool,
    excludes: Excludes,
}

impl State {
    fn watch_tree(&mut self, dir: &Path) {
        let ex = self.excludes.clone();
        for entry in index::walk(dir, &ex).filter(|e| e.file_type().is_dir()) {
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

    /// Follow roots added since the last look, and drop forgotten ones. A
    /// forgotten root's inotify watches stay until restart, but its events no
    /// longer match a root, so they are ignored.
    fn sync_roots(&mut self, conn: &rusqlite::Connection, jobs: &mpsc::Sender<Job>) {
        let Ok(roots) = index::roots(conn) else {
            return;
        };
        self.roots.retain(|r| roots.contains(r));
        for root in roots {
            if !self.roots.contains(&root) && root.is_dir() && !self.excludes.contains(&root) {
                let before = self.watched;
                self.watch_tree(&root);
                eprintln!(
                    "watching {} ({} directories)",
                    root.display(),
                    self.watched - before
                );
                // Catch up on what changed while nothing was watching (before
                // login, or with the runner stopped). Watches are already in
                // place, so nothing falls between this scan and them.
                let t0 = Instant::now();
                match index::stale(conn, &root, &self.excludes) {
                    Ok(paths) if !paths.is_empty() => {
                        eprintln!(
                            "{}: {} paths changed while unwatched (checked in {:.1} s)",
                            root.display(),
                            paths.len(),
                            t0.elapsed().as_secs_f64()
                        );
                        let _ = jobs.send(Job::Changed(paths));
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("checking {}: {e:#}", root.display()),
                }
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
fn relevant(root: &Path, path: &Path, ex: &Excludes) -> bool {
    if ex.contains(path) || index::skipped_under(root, path) {
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

/// `config` is re-read every ROOTS_EVERY; changed exclusions go to the worker.
pub fn spawn(
    jobs: mpsc::Sender<Job>,
    config: PathBuf,
    excludes: Excludes,
) -> Result<std::thread::JoinHandle<()>> {
    let conn = index::open(&index::default_db()?)?;
    let (tx, events) = mpsc::channel();
    let watcher = notify::recommended_watcher(tx)?;
    Ok(std::thread::spawn(move || {
        let mut st = State {
            watcher,
            roots: Vec::new(),
            watched: 0,
            warned_limit: false,
            excludes,
        };
        st.sync_roots(&conn, &jobs);
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
                        if !relevant(&root, &path, &st.excludes) {
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
                match Config::load(&config).and_then(|c| c.excludes()) {
                    Ok(ex) if ex != st.excludes => {
                        eprintln!("{}: exclusions changed", config.display());
                        st.excludes = ex.clone();
                        if jobs.send(Job::Excludes(ex)).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    // Keep the last good exclusions rather than index what a typo un-excluded.
                    Err(e) => eprintln!("reading config: {e:#}"),
                }
                st.sync_roots(&conn, &jobs);
                last_roots = Instant::now();
            }
        }
    }))
}
