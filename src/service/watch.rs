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
use crate::service::worker::{Job, Status, unix_now};
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
/// How long to wait for an event before looking at the timers.
const POLL: Duration = Duration::from_millis(500);

struct State {
    watcher: RecommendedWatcher,
    status: Arc<Status>,
    roots: Vec<PathBuf>,
    /// Directories with a watch. Dropped when the directory is removed or
    /// renamed away, so one recreated at the same path is watched again.
    watched: HashSet<PathBuf>,
    /// A watch was refused, most likely for want of inotify watches
    /// (fs.inotify.max_user_watches): warned once, retried at the next catch-up.
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
                Err(e) if !self.warned_limit => {
                    eprintln!("watch {}: {e}", entry.path().display());
                    self.warned_limit = true;
                }
                Err(_) => {}
            }
        }
    }

    /// Drop the watches on `dir` and everything under it that `keep` rejects.
    /// Returns how many were dropped. The kernel already dropped those on
    /// deleted directories, so unwatching them fails harmlessly.
    fn unwatch_tree(&mut self, dir: &Path, keep: impl Fn(&Self, &Path) -> bool) -> usize {
        let drop: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|d| d.starts_with(dir) && !keep(self, d))
            .cloned()
            .collect();
        for d in &drop {
            let _ = self.watcher.unwatch(d);
            self.watched.remove(d);
        }
        drop.len()
    }

    /// Watch directories `old` excluded that are excluded no longer.
    fn watch_unexcluded(&mut self, old: &Excludes) {
        for dir in old.paths() {
            if !self.excludes.contains(dir)
                && index::is_real_dir(dir)
                && self.root_of(dir).is_some()
            {
                self.watch_tree(dir);
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
            let n = self.unwatch_tree(&root, |st, d| st.root_of(d).is_some());
            eprintln!("stopped watching {} ({n} directories)", root.display());
        }
        for root in roots {
            if !self.roots.contains(&root) && root.is_dir() && !self.excludes.contains(&root) {
                self.follow(conn, jobs, root);
            }
        }
    }

    /// Watch a new root, then catch up on what changed while nothing watched
    /// it (before login, or with the runner stopped). The watches go in
    /// first, so no change falls between the check and them.
    fn follow(&mut self, conn: &rusqlite::Connection, jobs: &mpsc::Sender<Job>, root: PathBuf) {
        let before = self.watched.len();
        self.watch_tree(&root);
        eprintln!(
            "watching {} ({} directories)",
            root.display(),
            self.watched.len() - before
        );
        self.check(conn, jobs, &root);
        self.roots.push(root);
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
    /// under another (vinoWhisper's sessions), which the outer one's rules
    /// would skip (pinned by `tests::a_path_belongs_to_its_innermost_root`).
    fn root_of(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .map(PathBuf::as_path)
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.components().count())
    }
}

/// Whether a change to `path` could alter the index. A path that is gone
/// counts whatever its name: it may have been an indexed file or directory
/// (pinned by `tests::a_path_that_is_gone_is_relevant_whatever_its_type`
/// and `tests::an_existing_file_of_an_unindexed_type_is_not_relevant`).
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
    !path.exists() || index::wanted(path, types)
}

/// Paths an event says are no longer there: removed, or renamed away. A
/// rename with both names lists them as [from, to].
fn gone(event: &Event) -> &[PathBuf] {
    match event.kind {
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            &event.paths
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            &event.paths[..event.paths.len().min(1)]
        }
        _ => &[],
    }
}

/// Whether an event can change what is indexed (pinned by
/// `tests::chmod_and_access_are_not_interesting`).
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

    /// How much further wall time moved than monotonic time since `before`
    /// (pinned by `tests::a_wall_clock_stepped_back_is_not_sleep`).
    fn ahead_of(&self, before: &Clock) -> Duration {
        let wall = self.wall.duration_since(before.wall).unwrap_or_default();
        wall.saturating_sub(self.mono - before.mono)
    }
}

/// The worker hung up: the service is stopping.
struct WorkerGone;

/// The watch thread: `State` plus the config, the timers and what is waiting.
struct Watch {
    st: State,
    jobs: mpsc::Sender<Job>,
    config: PathBuf,
    /// What `config` held when last applied.
    file: Config,
    status: Arc<Status>,
    /// Closed and opened again under it when encryption changes.
    db: Arc<crate::index::encryption::Live>,
    pending: HashSet<PathBuf>,
    last_event: Instant,
    last_roots: Instant,
    last_check: Instant,
    clock: Clock,
    /// Why the next catch-up is due, once indexing allows it.
    due: Option<&'static str>,
}

impl Watch {
    /// Follow the roots already indexed, then start the timers.
    fn start(
        mut st: State,
        jobs: mpsc::Sender<Job>,
        config: PathBuf,
        file: Config,
        status: Arc<Status>,
        db: Arc<crate::index::encryption::Live>,
    ) -> Watch {
        if let Ok(conn) = db.lock() {
            st.sync_roots(&conn, &jobs);
        }
        status.last_checked.store(unix_now(), Ordering::Relaxed);
        Watch {
            st,
            jobs,
            config,
            file,
            status,
            db,
            pending: HashSet::new(),
            last_event: Instant::now(),
            last_roots: Instant::now(),
            last_check: Instant::now(),
            clock: Clock::now(),
            due: None,
        }
    }

    fn run(mut self, events: mpsc::Receiver<notify::Result<Event>>) {
        loop {
            match events.recv_timeout(POLL) {
                Ok(Ok(event)) => self.on_event(event),
                Ok(Err(e)) => eprintln!("watch: {e}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if self.tick().is_err() {
                return;
            }
        }
    }

    fn tick(&mut self) -> Result<(), WorkerGone> {
        self.flush_settled()?;
        if self.last_roots.elapsed() >= ROOTS_EVERY
            || self.status.reload_config.swap(false, Ordering::Relaxed)
        {
            self.reload_config()?;
            self.resync_roots();
            self.last_roots = Instant::now();
        }
        self.note_due_checks();
        self.catch_up_once_settled();
        Ok(())
    }

    fn wake(&self) -> Result<(), WorkerGone> {
        self.jobs.send(Job::Wake).map_err(|_| WorkerGone)
    }

    fn on_event(&mut self, event: Event) {
        if event.need_rescan() {
            self.due.get_or_insert("events were dropped");
            self.last_event = Instant::now();
        } else if interesting(&event.kind) {
            self.on_change(event);
        }
    }

    /// Watches under paths that went are dropped before any are added, so a
    /// rename's destination is watched again.
    fn on_change(&mut self, event: Event) {
        for path in gone(&event) {
            self.st.unwatch_tree(path, |_, _| false);
        }
        let may_add_dirs = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
        for path in event.paths {
            let Some(root) = self.st.root_of(&path).map(Path::to_path_buf) else {
                continue;
            };
            if !relevant(&root, &path, &self.st.excludes, &self.st.types) {
                continue;
            }
            if index::is_real_dir(&path) && may_add_dirs {
                self.st.watch_tree(&path);
            }
            self.pending.insert(path);
            self.last_event = Instant::now();
        }
    }

    fn flush_settled(&mut self) -> Result<(), WorkerGone> {
        if self.pending.is_empty() || self.last_event.elapsed() < DEBOUNCE {
            return Ok(());
        }
        self.status.work.changed(self.pending.drain());
        self.wake()
    }

    /// A config that fails to parse, or has a bad exclusion or file type,
    /// changes nothing: better the last good exclusions than index what a
    /// typo un-excluded. The file types also follow what is installed (media
    /// once ffmpeg is there), so they can change with the file as it was.
    fn reload_config(&mut self) -> Result<(), WorkerGone> {
        let loaded = Config::load(&self.config)
            .and_then(|c| Ok((c.excludes()?, FileTypes::new(&c.formats)?, c)));
        let (ex, types, c) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => {
                eprintln!("reading config: {e:#}");
                return Ok(());
            }
        };
        if c == self.file && types == self.st.types {
            return Ok(());
        }
        if c != self.file {
            eprintln!("{}: changed", self.config.display());
        }
        let old = std::mem::replace(&mut self.st.excludes, ex);
        self.st.watch_unexcluded(&old);
        if types != self.st.types {
            self.st.types = types;
            self.due.get_or_insert("file types changed");
        }
        self.file = c.clone();
        self.status.pending.config(c);
        self.wake()
    }

    fn resync_roots(&mut self) {
        match self.db.lock() {
            Ok(conn) => self.st.sync_roots(&conn, &self.jobs),
            Err(e) => eprintln!("watch: {e:#}"),
        }
    }

    fn note_due_checks(&mut self) {
        if self.clock.slept() {
            self.due.get_or_insert("woke from sleep");
        }
        if self.last_check.elapsed() >= CATCH_UP_EVERY {
            self.due.get_or_insert("hourly");
        }
        if self.status.check_now.swap(false, Ordering::Relaxed) {
            self.due.get_or_insert("asked to");
        }
    }

    /// Only after a burst settles, and not while indexing: the walk would
    /// find what the worker hasn't reached yet, for nothing.
    fn catch_up_once_settled(&mut self) {
        let Some(why) = self.due else {
            return;
        };
        if self.last_event.elapsed() < DEBOUNCE || self.status.indexing() {
            return;
        }
        match self.db.lock() {
            Ok(conn) => self.st.catch_up(&conn, &self.jobs, why),
            Err(e) => eprintln!("watch: {e:#}"),
        }
        self.status
            .last_checked
            .store(unix_now(), Ordering::Relaxed);
        self.last_check = Instant::now();
        self.due = None;
    }
}

/// `config` is re-read every ROOTS_EVERY; when it differs from `file` (what
/// it held at startup), the new contents go to the worker's `Pending`.
/// `status` carries CheckNow requests in, and when the last check ran out.
pub fn spawn(
    jobs: mpsc::Sender<Job>,
    config: PathBuf,
    file: Config,
    status: Arc<Status>,
) -> Result<std::thread::JoinHandle<()>> {
    let excludes = file.excludes()?;
    let types = FileTypes::new(&file.formats)?;
    let db = crate::index::encryption::Live::open(&index::default_db()?)?;
    let (tx, events) = mpsc::channel();
    let watcher = notify::recommended_watcher(tx)?;
    let st = State {
        watcher,
        status: status.clone(),
        roots: Vec::new(),
        watched: HashSet::new(),
        warned_limit: false,
        excludes,
        types,
    };
    Ok(std::thread::spawn(move || {
        Watch::start(st, jobs, config, file, status, db).run(events)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, MetadataKind, RemoveKind};

    fn clock_after(before: &Clock, mono: Duration, wall: SystemTime) -> Clock {
        Clock {
            mono: before.mono + mono,
            wall,
        }
    }

    #[test]
    fn a_wall_clock_jump_reads_as_sleep() {
        let before = Clock::now();
        let five = Duration::from_secs(5);
        let awake = clock_after(&before, five, before.wall + five);
        assert!(awake.ahead_of(&before) <= SLEPT);
        let resumed = clock_after(&before, five, before.wall + Duration::from_secs(8 * 3600));
        assert!(resumed.ahead_of(&before) > SLEPT);
    }

    #[test]
    fn a_wall_clock_stepped_back_is_not_sleep() {
        let before = Clock::now();
        let wall = before.wall - Duration::from_secs(3600);
        let stepped_back = clock_after(&before, Duration::from_secs(5), wall);
        assert_eq!(stepped_back.ahead_of(&before), Duration::ZERO);
    }

    /// A root with `src`, `target`, `.git` and an excluded `private`, in a
    /// temp dir of its own, removed on drop.
    struct Tree {
        dir: PathBuf,
        root: PathBuf,
        ex: Excludes,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir()
                .join(format!("vinosearch-watch-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for d in ["root/src", "root/target", "root/.git", "root/private"] {
                std::fs::create_dir_all(dir.join(d)).unwrap();
            }
            let root = std::fs::canonicalize(dir.join("root")).unwrap();
            let config: Config =
                toml::from_str(&format!("exclude = [{:?}]", root.join("private"))).unwrap();
            let ex = config.excludes().unwrap();
            Tree { dir, root, ex }
        }

        fn relevant(&self, p: &str) -> bool {
            relevant(
                &self.root,
                &self.root.join(p),
                &self.ex,
                &FileTypes::default(),
            )
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn relevant_follows_the_indexing_rules() {
        let tree = Tree::new("rules");
        assert!(tree.relevant("notes.md"));
        assert!(tree.relevant("src"));
        assert!(!tree.relevant("target"));
        assert!(!tree.relevant(".git"));
        assert!(!tree.relevant("target/debug/out.rs"));
        assert!(!tree.relevant(".git/HEAD"));
        assert!(!tree.relevant("private"));
        assert!(!tree.relevant("private/diary.md"));
    }

    #[test]
    fn a_path_that_is_gone_is_relevant_whatever_its_type() {
        let tree = Tree::new("gone");
        assert!(tree.relevant("gone.xcf"));
    }

    #[test]
    fn an_existing_file_of_an_unindexed_type_is_not_relevant() {
        let tree = Tree::new("unindexed");
        std::fs::write(tree.root.join("photo.xcf"), b"").unwrap();
        assert!(!tree.relevant("photo.xcf"));
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

    fn event(kind: EventKind, paths: &[&str]) -> Event {
        Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        }
    }

    fn rename(mode: RenameMode) -> EventKind {
        EventKind::Modify(ModifyKind::Name(mode))
    }

    #[test]
    fn gone_reads_the_event_kind() {
        let removed = event(EventKind::Remove(RemoveKind::File), &["/a"]);
        assert_eq!(gone(&removed), [PathBuf::from("/a")]);
        assert_eq!(
            gone(&event(rename(RenameMode::From), &["/a"])),
            [PathBuf::from("/a")]
        );
        let both = event(rename(RenameMode::Both), &["/a", "/b"]);
        assert_eq!(
            gone(&both),
            [PathBuf::from("/a")],
            "a rename lists [from, to]: only the old name went"
        );
        assert!(gone(&event(rename(RenameMode::Both), &[])).is_empty());
        assert!(gone(&event(rename(RenameMode::To), &["/b"])).is_empty());
        assert!(gone(&event(EventKind::Create(CreateKind::File), &["/c"])).is_empty());
    }

    #[test]
    fn creates_removes_and_modifications_are_interesting() {
        assert!(interesting(&EventKind::Create(CreateKind::File)));
        assert!(interesting(&EventKind::Remove(RemoveKind::Any)));
        assert!(interesting(&rename(RenameMode::Both)));
        assert!(interesting(&EventKind::Modify(ModifyKind::Any)));
    }

    #[test]
    fn chmod_and_access_are_not_interesting() {
        let chmod = ModifyKind::Metadata(MetadataKind::Permissions);
        assert!(!interesting(&EventKind::Modify(chmod)));
        assert!(!interesting(&EventKind::Access(AccessKind::Any)));
        assert!(!interesting(&EventKind::Other));
    }
}
