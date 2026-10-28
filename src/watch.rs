//! Keep the index current: inotify on every directory `index` would walk.
//!
//! One non-recursive watch per directory, added with the same skip rules as
//! indexing, so build output and hidden trees cost no watches (a recursive
//! watch would add one for every directory under `target/` and
//! `node_modules/`). Events are debounced and put to the worker's `Work` as
//! paths to check, each once however often it comes up.
//!
//! inotify can still miss changes: the kernel drops events when its queue
//! overflows, directories past the watch limit go unwatched, and nothing is
//! watched while the machine sleeps. So every root is also compared with the
//! index by mtime and size, as at startup: after an overflow, every
//! CATCH_UP_EVERY, after a resume, and when asked (`Control1.CheckNow`).

use crate::config::{Config, Excludes};
use crate::index::{self, FileTypes};
use crate::worker::{Job, Status, unix_now};
use anyhow::Result;
use notify::event::{Event, EventKind, ModifyKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};

/// Quiet period before a batch of changes is indexed: editors write a file in
/// several steps, and builds touch many at once.
const DEBOUNCE: Duration = Duration::from_secs(2);
/// How often to look for roots added by `vinosearch index`, and re-read the
/// config, while running.
const ROOTS_EVERY: Duration = Duration::from_secs(30);
/// How often every root is compared with the index. It reads no files and
/// loads no model: 0.5 s for 45,418 paths under a home directory (2026-10-18).
const CATCH_UP_EVERY: Duration = Duration::from_secs(60 * 60);
/// Wall time running this far ahead of monotonic time means the machine slept.
const SLEPT: Duration = Duration::from_secs(60);

struct State {
    watcher: RecommendedWatcher,
    status: Arc<Status>,
    roots: Vec<PathBuf>,
    /// Directories with a watch. Dropped when the directory is removed or
    /// renamed away, so one recreated at the same path is watched again.
    watched: HashSet<PathBuf>,
    warned_limit: bool,
    excludes: Excludes,
    types: FileTypes,
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

    /// Drop the watches on `dir` and everything under it that `keep` rejects.
    /// Returns how many were dropped.
    fn unwatch_tree(&mut self, dir: &Path, keep: impl Fn(&Self, &Path) -> bool) -> usize {
        let drop: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|d| d.starts_with(dir) && !keep(self, d))
            .cloned()
            .collect();
        for d in &drop {
            // Fails for directories already deleted; the kernel dropped those.
            let _ = self.watcher.unwatch(d);
            self.watched.remove(d);
        }
        drop.len()
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
            let n = self.unwatch_tree(&root, |st, d| st.root_of(d).is_some());
            eprintln!("stopped watching {} ({n} directories)", root.display());
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
                self.check(conn, jobs, &root);
                self.roots.push(root);
            }
        }
    }

    /// Put whatever under `root` differs from the index to the worker.
    fn check(&self, conn: &rusqlite::Connection, jobs: &mpsc::Sender<Job>, root: &Path) {
        let t0 = Instant::now();
        match index::stale(conn, root, &self.excludes, &self.types) {
            Ok(paths) if !paths.is_empty() => {
                eprintln!(
                    "{}: {} paths changed while unwatched (checked in {:.1} s)",
                    root.display(),
                    paths.len(),
                    t0.elapsed().as_secs_f64()
                );
                self.status.work.found(paths);
                let _ = jobs.send(Job::Wake);
            }
            Ok(_) => {}
            Err(e) => eprintln!("checking {}: {e:#}", root.display()),
        }
    }

    /// Check every root, and retry watches the limit refused last time, in
    /// case it was raised.
    fn catch_up(&mut self, conn: &rusqlite::Connection, jobs: &mpsc::Sender<Job>, why: &str) {
        eprintln!("checking for missed changes ({why})");
        for root in self.roots.clone() {
            if self.warned_limit {
                self.warned_limit = false;
                self.watch_tree(&root);
            }
            self.check(conn, jobs, &root);
        }
    }

    /// The innermost root holding `path`: a root can sit in a hidden folder
    /// under another (vinoWhisper's sessions, #143), which the outer one's
    /// rules would skip.
    fn root_of(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .map(PathBuf::as_path)
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.components().count())
    }
}

/// Whether a change to `path` could alter the index.
fn relevant(root: &Path, path: &Path, ex: &Excludes, types: &FileTypes) -> bool {
    if ex.contains(path) || index::skipped_under(root, path) {
        return false;
    }
    if index::is_real_dir(path) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        return !index::skipped_dir(&name);
    }
    // Gone paths may have been indexed files or directories; removal sorts it out.
    !path.exists() || index::wanted(path, types)
}

/// Paths an event says are no longer there: removed, or renamed away.
fn gone(event: &Event) -> &[PathBuf] {
    match event.kind {
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            &event.paths
        }
        // [from, to]
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            &event.paths[..event.paths.len().min(1)]
        }
        _ => &[],
    }
}

fn interesting(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        _ => false,
    }
}

/// Monotonic time stops while the machine is suspended (Rust's Instant is
/// CLOCK_MONOTONIC on Linux); wall time does not.
struct Clock {
    mono: Instant,
    wall: SystemTime,
}

impl Clock {
    fn now() -> Self {
        Clock {
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// Whether the machine slept since the last call. A wall clock stepped
    /// forward reads the same, and costs one needless check.
    fn slept(&mut self) -> bool {
        let now = Clock::now();
        let slept = now.ahead_of(self) > SLEPT;
        *self = now;
        slept
    }

    /// How much further wall time moved than monotonic time since `before`.
    fn ahead_of(&self, before: &Clock) -> Duration {
        let wall = self.wall.duration_since(before.wall).unwrap_or_default();
        wall.saturating_sub(self.mono - before.mono)
    }
}

/// `config` is re-read every ROOTS_EVERY; when it differs from `file` (what
/// it held at startup), the new contents go to the worker's `Pending`.
/// `status` carries CheckNow requests in, and when the last check ran out.
pub fn spawn(
    jobs: mpsc::Sender<Job>,
    config: PathBuf,
    mut file: Config,
    status: Arc<Status>,
) -> Result<std::thread::JoinHandle<()>> {
    let excludes = file.excludes()?;
    let types = FileTypes::new(&file.formats)?;
    // Closed and opened again under it when encryption changes (#4).
    let db = crate::encryption::Live::open(&index::default_db()?)?;
    let (tx, events) = mpsc::channel();
    let watcher = notify::recommended_watcher(tx)?;
    Ok(std::thread::spawn(move || {
        let mut st = State {
            watcher,
            status: status.clone(),
            roots: Vec::new(),
            watched: HashSet::new(),
            warned_limit: false,
            excludes,
            types,
        };
        if let Ok(conn) = db.lock() {
            st.sync_roots(&conn, &jobs);
        }
        status.last_checked.store(unix_now(), Ordering::Relaxed);
        let mut pending: HashSet<PathBuf> = HashSet::new();
        let mut last_event = Instant::now();
        let mut last_roots = Instant::now();
        let mut last_check = Instant::now();
        let mut clock = Clock::now();
        // Why the next check is due, once indexing allows it.
        let mut due: Option<&str> = None;
        loop {
            match events.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(event)) if event.need_rescan() => {
                    // The kernel's queue overflowed: some changes were dropped.
                    due.get_or_insert("events were dropped");
                    last_event = Instant::now();
                }
                Ok(Ok(event)) if interesting(&event.kind) => {
                    // Before anything below re-adds the destination of a rename.
                    for path in gone(&event) {
                        st.unwatch_tree(path, |_, _| false);
                    }
                    for path in event.paths {
                        let Some(root) = st.root_of(&path).map(Path::to_path_buf) else {
                            continue;
                        };
                        if !relevant(&root, &path, &st.excludes, &st.types) {
                            continue;
                        }
                        if index::is_real_dir(&path)
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
            if !pending.is_empty() && last_event.elapsed() >= DEBOUNCE {
                status.work.changed(pending.drain());
                if jobs.send(Job::Wake).is_err() {
                    break;
                }
            }
            if last_roots.elapsed() >= ROOTS_EVERY
                || status.reload_config.swap(false, Ordering::Relaxed)
            {
                // A file that fails to parse, or has a bad exclusion or file
                // type, changes nothing: better the last good exclusions than
                // index what a typo un-excluded.
                match Config::load(&config)
                    .and_then(|c| Ok((c.excludes()?, FileTypes::new(&c.formats)?, c)))
                {
                    Ok((ex, types, c)) if c != file => {
                        eprintln!("{}: changed", config.display());
                        let old = std::mem::replace(&mut st.excludes, ex);
                        // Directories no longer excluded need watches now.
                        for dir in old.paths() {
                            if !st.excludes.contains(dir)
                                && index::is_real_dir(dir)
                                && st.root_of(dir).is_some()
                            {
                                st.watch_tree(dir);
                            }
                        }
                        // A type turned on: what of it is already there.
                        if types != st.types {
                            st.types = types;
                            due.get_or_insert("file types changed");
                        }
                        file = c.clone();
                        status.pending.config(c);
                        if jobs.send(Job::Wake).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("reading config: {e:#}"),
                }
                match db.lock() {
                    Ok(conn) => st.sync_roots(&conn, &jobs),
                    Err(e) => eprintln!("watch: {e:#}"),
                }
                last_roots = Instant::now();
            }
            if clock.slept() {
                due.get_or_insert("woke from sleep");
            }
            if last_check.elapsed() >= CATCH_UP_EVERY {
                due.get_or_insert("hourly");
            }
            if status.check_now.swap(false, Ordering::Relaxed) {
                due.get_or_insert("asked to");
            }
            // After a burst settles, and not while indexing: the walk would
            // find what the worker hasn't reached yet, for nothing.
            if let Some(why) = due
                && last_event.elapsed() >= DEBOUNCE
                && !status.indexing()
            {
                match db.lock() {
                    Ok(conn) => st.catch_up(&conn, &jobs, why),
                    Err(e) => eprintln!("watch: {e:#}"),
                }
                status.last_checked.store(unix_now(), Ordering::Relaxed);
                last_check = Instant::now();
                due = None;
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_clock_jump_reads_as_sleep() {
        let before = Clock::now();
        let awake = Clock {
            mono: before.mono + Duration::from_secs(5),
            wall: before.wall + Duration::from_secs(5),
        };
        assert!(awake.ahead_of(&before) <= SLEPT);
        let resumed = Clock {
            mono: before.mono + Duration::from_secs(5),
            wall: before.wall + Duration::from_secs(8 * 3600),
        };
        assert!(resumed.ahead_of(&before) > SLEPT);
        // A clock stepped back is not sleep.
        let stepped_back = Clock {
            mono: before.mono + Duration::from_secs(5),
            wall: before.wall - Duration::from_secs(3600),
        };
        assert_eq!(stepped_back.ahead_of(&before), Duration::ZERO);
    }

    #[test]
    fn relevant_follows_the_indexing_rules() {
        let dir = std::env::temp_dir().join(format!("vinosearch-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["root/src", "root/target", "root/.git", "root/private"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let root = std::fs::canonicalize(dir.join("root")).unwrap();
        let config: Config =
            toml::from_str(&format!("exclude = [{:?}]", root.join("private"))).unwrap();
        let ex = config.excludes().unwrap();
        let types = FileTypes::default();
        let yes = |p: &str| relevant(&root, &root.join(p), &ex, &types);

        assert!(yes("notes.md"));
        assert!(yes("src"));
        // Gone: it may have been indexed, so removal sorts it out.
        assert!(yes("gone.xcf"));
        std::fs::write(root.join("photo.xcf"), b"").unwrap();
        assert!(!yes("photo.xcf"));
        assert!(!yes("target"));
        assert!(!yes(".git"));
        assert!(!yes("target/debug/out.rs"));
        assert!(!yes(".git/HEAD"));
        assert!(!yes("private"));
        assert!(!yes("private/diary.md"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_belongs_to_its_innermost_root() {
        let (tx, _rx) = mpsc::channel();
        let mut st = State {
            watcher: notify::recommended_watcher(tx).unwrap(),
            status: Arc::default(),
            roots: vec![
                PathBuf::from("/h"),
                PathBuf::from("/h/.local/share/vinowhisper/transcripts"),
            ],
            watched: HashSet::new(),
            warned_limit: false,
            excludes: Excludes::default(),
            types: FileTypes::default(),
        };
        let s = Path::new("/h/.local/share/vinowhisper/transcripts/a.vwsession");
        assert_eq!(
            st.root_of(s),
            Some(Path::new("/h/.local/share/vinowhisper/transcripts"))
        );
        assert_eq!(st.root_of(Path::new("/h/notes.md")), Some(Path::new("/h")));
        st.roots.reverse();
        assert_eq!(
            st.root_of(s),
            Some(Path::new("/h/.local/share/vinowhisper/transcripts"))
        );
        assert_eq!(st.root_of(Path::new("/elsewhere")), None);
    }

    #[test]
    fn gone_and_interesting_read_the_event_kind() {
        use notify::event::{CreateKind, MetadataKind, RemoveKind};
        let event = |kind, paths: &[&str]| Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        };
        let rename = |mode| EventKind::Modify(ModifyKind::Name(mode));

        let removed = event(EventKind::Remove(RemoveKind::File), &["/a"]);
        assert_eq!(gone(&removed), [PathBuf::from("/a")]);
        assert_eq!(
            gone(&event(rename(RenameMode::From), &["/a"])),
            [PathBuf::from("/a")]
        );
        // [from, to]: only the old name went.
        let both = event(rename(RenameMode::Both), &["/a", "/b"]);
        assert_eq!(gone(&both), [PathBuf::from("/a")]);
        assert!(gone(&event(rename(RenameMode::Both), &[])).is_empty());
        assert!(gone(&event(rename(RenameMode::To), &["/b"])).is_empty());
        assert!(gone(&event(EventKind::Create(CreateKind::File), &["/c"])).is_empty());

        assert!(interesting(&EventKind::Create(CreateKind::File)));
        assert!(interesting(&EventKind::Remove(RemoveKind::Any)));
        assert!(interesting(&rename(RenameMode::Both)));
        assert!(interesting(&EventKind::Modify(ModifyKind::Any)));
        // chmod and atime don't change what is indexed.
        assert!(!interesting(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Permissions
        ))));
        assert!(!interesting(&EventKind::Access(
            notify::event::AccessKind::Any
        )));
        assert!(!interesting(&EventKind::Other));
    }
}
