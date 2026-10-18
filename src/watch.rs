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
    /// Directories with a watch. Ones deleted since lose theirs in the kernel
    /// and stay listed until their root is forgotten, which is harmless.
    watched: HashSet<PathBuf>,
    warned_limit: bool,
    excludes: Excludes,
}

impl State {
    fn watch_tree(&mut self, dir: &Path) {
        let ex = self.excludes.clone();
        for entry in index::walk(dir, &ex).filter(|e| e.file_type().is_dir()) {
            if self.watched.contains(entry.path()) {
                continue;
            }
            match self
                .watcher
                .watch(entry.path(), RecursiveMode::NonRecursive)
            {
                Ok(()) => {
                    self.watched.insert(entry.into_path());
                }
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

    /// Follow roots added since the last look, and drop forgotten ones with
    /// their watches (except under a root still followed: roots can nest).
    fn sync_roots(&mut self, conn: &rusqlite::Connection, jobs: &mpsc::Sender<Job>) {
        let Ok(roots) = index::roots(conn) else {
            return;
        };
        let (kept, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.roots)
            .into_iter()
            .partition(|r| roots.contains(r));
        self.roots = kept;
        for root in gone {
            let drop: Vec<PathBuf> = self
                .watched
                .iter()
                .filter(|d| d.starts_with(&root) && self.root_of(d).is_none())
                .cloned()
                .collect();
            for dir in &drop {
                // Fails for directories deleted since; their watch is gone already.
                let _ = self.watcher.unwatch(dir);
                self.watched.remove(dir);
            }
            eprintln!(
                "stopped watching {} ({} directories)",
                root.display(),
                drop.len()
            );
        }
        for root in roots {
            if !self.roots.contains(&root) && root.is_dir() && !self.excludes.contains(&root) {
                let before = self.watched.len();
                self.watch_tree(&root);
                eprintln!(
                    "watching {} ({} directories)",
                    root.display(),
                    self.watched.len() - before
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

/// `config` is re-read every ROOTS_EVERY; when it differs from `file` (what
/// it held at startup), the new contents go to the worker.
pub fn spawn(
    jobs: mpsc::Sender<Job>,
    config: PathBuf,
    mut file: Config,
) -> Result<std::thread::JoinHandle<()>> {
    let excludes = file.excludes()?;
    let conn = index::open(&index::default_db()?)?;
    let (tx, events) = mpsc::channel();
    let watcher = notify::recommended_watcher(tx)?;
    Ok(std::thread::spawn(move || {
        let mut st = State {
            watcher,
            roots: Vec::new(),
            watched: HashSet::new(),
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
                // A file that fails to parse, or has a bad exclusion, changes
                // nothing: better the last good exclusions than index what a
                // typo un-excluded.
                match Config::load(&config).and_then(|c| Ok((c.excludes()?, c))) {
                    Ok((ex, c)) if c != file => {
                        eprintln!("{}: changed", config.display());
                        st.excludes = ex;
                        file = c.clone();
                        if jobs.send(Job::Config(c)).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("reading config: {e:#}"),
                }
                st.sync_roots(&conn, &jobs);
                last_roots = Instant::now();
            }
        }
    }))
}
