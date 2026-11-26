//! The one thread that owns the model and the index.
//!
//! OpenVINO's handles are not Send, so queries (from the KRunner interface)
//! come here over a channel. Everything else is state the worker brings the
//! index in line with a step at a time: the settings as last read and requests
//! that change the index (`Pending`), and paths to check (`Work`), each once.
//! A step applies what changed in the settings, then takes the most urgent
//! item: a changed path, a root, a walk's finding, the rest of a long document.
//! A file being indexed when settings change stops after its chunk and is
//! checked again under them.
//!
//! The model is loaded on demand and dropped, with the vector cache, after
//! `idle` without a job. `Status` publishes what the worker is doing, so status
//! queries never wait behind it.

use crate::config::{self, Config, Excludes};
use crate::index::encryption::{self, Want};
use crate::index::seal::{self, Sealer};
use crate::index::{self, FileTypes, Hit, Kind, Outcome, Vectors};
use crate::ml::embed::Embed;
use crate::service::sched;
use anyhow::{Result, bail};
use futures_channel::oneshot;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// How long the document stage stays compiled after its last use: it holds
/// ~430 MB and reloads from the cache in ~0.1 s, so it goes well before the
/// model's idle unload.
const DOCS_HOLD: Duration = Duration::from_secs(60);

pub enum Job {
    Match {
        query: String,
        k: usize,
        reply: oneshot::Sender<Vec<Hit>>,
    },
    Wake,
    /// The model may load now, or indexing resumed: run what waited.
    Retry,
}

/// A change to the index outside indexing. Those that drop or convert what a
/// file being indexed may be going into stop it first (`stops_indexing`); the
/// others wait.
#[derive(Debug, PartialEq)]
pub enum Request {
    Forget(PathBuf),
    Encryption(Want),
    Rekey,
    Unload,
    DeleteModel,
    ClearCache,
}

impl Request {
    /// Whether a file being indexed stops for this, or waits for it (pinned
    /// by `tests::unload_waits_for_the_file_being_indexed_and_forget_stops_it`).
    fn stops_indexing(&self) -> bool {
        matches!(
            self,
            Request::Forget(_) | Request::Encryption(_) | Request::Rekey
        )
    }
}

/// The config as last read, and requests that change the index, applied at
/// the worker's next step. The generation moves with every write a file being
/// indexed must not run past (new exclusions, a forgotten root, encryption):
/// the worker stops after its chunk and checks the file again.
#[derive(Default)]
pub struct Pending {
    generation: AtomicU64,
    inner: Mutex<Waiting>,
}

#[derive(Default)]
struct Waiting {
    config: Option<Config>,
    requests: Vec<Request>,
}

struct Arrived {
    generation: u64,
    config: Option<Config>,
    requests: Vec<Request>,
}

impl Pending {
    /// The latest config stands, except where a flag overrides it.
    pub fn config(&self, file: Config) {
        let mut p = self.inner.lock().unwrap();
        p.config = Some(file);
        // Under the lock, so a take's generation is current to what it took.
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn request(&self, r: Request) {
        let mut p = self.inner.lock().unwrap();
        let stops = r.stops_indexing();
        p.requests.push(r);
        if stops {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn take(&self) -> Arrived {
        let mut p = self.inner.lock().unwrap();
        Arrived {
            generation: self.generation.load(Ordering::Relaxed),
            config: p.config.take(),
            requests: std::mem::take(&mut p.requests),
        }
    }

    /// The encryption requests alone: the last Want, and a Rekey; the rest wait
    /// for the index's key.
    fn take_encryption(&self) -> Option<Want> {
        let mut p = self.inner.lock().unwrap();
        let mut want = None;
        p.requests.retain(|r| match r {
            Request::Encryption(w) => {
                want = Some(*w);
                false
            }
            Request::Rekey => false,
            _ => true,
        });
        want
    }
}

/// Paths to check, each once however often asked. The worker takes the most
/// urgent: a root to register and walk (needs no model, so a busy watcher can't
/// hold it back), a path the watcher saw change, then what a walk found. Held
/// work stays here, and queries never wait behind it.
#[derive(Default)]
pub struct Work(Mutex<Set>);

#[derive(Default)]
struct Set {
    changes: BTreeSet<PathBuf>,
    roots: Vec<PathBuf>,
    stale: BTreeSet<PathBuf>,
}

#[derive(Debug, PartialEq)]
pub enum Item {
    Check(PathBuf),
    /// Make the path a root and find what under it needs checking. Needs no model.
    Root(PathBuf),
    Rest,
}

impl Work {
    pub fn changed(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut s = self.0.lock().unwrap();
        for p in paths {
            s.stale.remove(&p);
            s.changes.insert(p);
        }
    }

    pub fn found(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut s = self.0.lock().unwrap();
        for p in paths {
            if !s.changes.contains(&p) {
                s.stale.insert(p);
            }
        }
    }

    pub fn root(&self, root: PathBuf) {
        let mut s = self.0.lock().unwrap();
        if !s.roots.contains(&root) {
            s.roots.push(root);
        }
    }

    /// The most urgent item; with `roots_only`, only a root (whose walk needs no model).
    fn take(&self, roots_only: bool) -> Option<Item> {
        let mut s = self.0.lock().unwrap();
        if !s.roots.is_empty() {
            return Some(Item::Root(s.roots.remove(0)));
        }
        if roots_only {
            return None;
        }
        s.changes
            .pop_first()
            .or_else(|| s.stale.pop_first())
            .map(Item::Check)
    }

    /// Drop a forgotten root: a Reindex of it still waiting would make it a root
    /// again. Paths under it stay if one of `roots` (which can nest) covers them.
    fn forget(&self, root: &Path, roots: &[PathBuf]) {
        let mut s = self.0.lock().unwrap();
        s.roots.retain(|r| r != root);
        let covered = |p: &PathBuf| !p.starts_with(root) || roots.iter().any(|r| p.starts_with(r));
        s.changes.retain(covered);
        s.stale.retain(covered);
    }

    pub fn pending(&self) -> bool {
        let s = self.0.lock().unwrap();
        !(s.changes.is_empty() && s.roots.is_empty() && s.stale.is_empty())
    }
}

/// What the runner is doing, for status queries. The model download runs on its own thread.
#[derive(Default)]
pub struct Status {
    pub loaded: AtomicBool,
    pub working: AtomicBool,
    /// Files embedded by the current run of indexing.
    pub indexed_files: AtomicU64,
    pub indexing_rest: AtomicBool,
    pub fetching: AtomicBool,
    pub fetch_done: AtomicU64,
    pub fetch_total: AtomicU64,
    pub fetch_error: std::sync::Mutex<String>,
    pub device: std::sync::Mutex<String>,
    /// Unix seconds when the watcher last compared every root with the
    /// index, and when indexing last changed it; 0 for not yet.
    pub last_checked: AtomicU64,
    pub last_indexed: AtomicU64,
    pub check_now: AtomicBool,
    /// Set by ApplyEncryption: the watcher reads the config now, not at its next 30 s read.
    pub reload_config: AtomicBool,
    pub pending: Pending,
    pub work: Work,
    /// The model failed to load: what needs it waits for a Retry or a query.
    pub waiting_model: AtomicBool,
    /// Indexing is paused: a file being indexed stops after its chunk.
    pub paused: AtomicBool,
    pub power: crate::service::power::State,
    /// Why this start dropped part of the index to be indexed again, until that is done.
    pub model_changed: std::sync::Mutex<String>,
    pub encryption: std::sync::Mutex<encryption::State>,
    pub tpm: AtomicBool,
    /// The last walk of each root: what it found or why it failed, and a count of walks so far that
    /// tells a new one from the last.
    pub walks: std::sync::Mutex<Vec<Walk>>,
    walked: AtomicU64,
}

/// One root's last walk: paths found to check, or why it failed; `seq` grows with every walk.
#[derive(Clone, Debug, PartialEq)]
pub struct Walk {
    pub root: PathBuf,
    pub found: std::result::Result<u64, String>,
    pub seq: u64,
}

#[derive(Debug)]
struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

fn interrupted(r: &Result<impl Sized>) -> bool {
    r.as_ref()
        .is_err_and(|e| e.downcast_ref::<Interrupted>().is_some())
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Status {
    pub fn new(device: &str) -> Self {
        Self {
            device: std::sync::Mutex::new(device.to_owned()),
            ..Default::default()
        }
    }

    fn walked(&self, root: &Path, found: std::result::Result<u64, String>) {
        let seq = self.walked.fetch_add(1, Ordering::Relaxed) + 1;
        let mut walks = self.walks.lock().unwrap();
        walks.retain(|w| w.root != root);
        walks.push(Walk {
            root: root.to_owned(),
            found,
            seq,
        });
    }

    pub fn held(&self) -> bool {
        self.paused.load(Ordering::Relaxed) || self.power.hold().is_some()
    }

    /// Indexing is running, or will be as soon as the worker gets to it (true from
    /// the moment Reindex returns); false while held or waiting for the model. The
    /// watcher waits this out before comparing every root.
    pub fn indexing(&self) -> bool {
        self.working.load(Ordering::Relaxed)
            || (!self.held() && !self.waiting_model.load(Ordering::Relaxed) && self.work.pending())
    }
}

/// Before the watcher compares the roots with the index: encrypt or decrypt it
/// as the config asks, rebuild it if its key is lost, and drop what must be
/// indexed again (vectors from another model, photos now decodable, scans OCR
/// can read now, files of a format read differently), so that first comparison
/// finds it. Says so in `status`.
///
/// A key that can't be had yet (a locked keyring) isn't prompted for here, so
/// the runner is on the bus at once; the worker asks, waiting for it
/// (`wait_for_key`).
pub fn prepare(status: &Status, file: &Config) -> Result<()> {
    let db = index::default_db()?;
    encryption::hold_shared(&db)?;
    status.tpm.store(seal::tpm_available(), Ordering::Relaxed);
    apply_encryption(
        status,
        &db,
        Want::of(file),
        false,
        &Sealer::default().quiet(),
    );
    match index::open(&db) {
        Ok(conn) => prepare_index(status, &conn),
        Err(e) if seal::is_unavailable(&e) => {
            eprintln!("waiting for the index's key: {e:#}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn prepare_index(status: &Status, conn: &rusqlite::Connection) -> Result<()> {
    let purged = index::purge_other_models(conn)?;
    if let Some(why) = model_changed(&purged) {
        eprintln!("{why}");
        *status.model_changed.lock().unwrap() = why;
    }
    let retry = index::retry_undecodable(conn)?;
    if retry > 0 {
        eprintln!("a HEIF decoder is installed now: {retry} photos will be indexed");
    }
    let ocr = crate::ml::model::ocr_dir().is_ok_and(|d| crate::ml::model::ocr_present(&d));
    let scans = index::retry_scans(conn, ocr)?;
    if scans > 0 {
        eprintln!("{scans} PDFs with no text will be read by OCR");
    }
    let reread = index::reread_formats(conn)?;
    if reread > 0 {
        eprintln!("{reread} files are read differently now and will be indexed again");
    }
    Ok(())
}

fn apply_encryption(status: &Status, db: &Path, want: Want, rekey: bool, sealer: &Sealer) {
    let st = encryption::apply(db, want, rekey, sealer);
    if !st.error.is_empty() {
        eprintln!("encryption: {}", st.error);
    }
    let before = status.encryption.lock().unwrap().waiting.clone();
    if st.waiting != before && !st.waiting.is_empty() {
        eprintln!("encryption: waiting for the index's key: {}", st.waiting);
    }
    let mut kept = status.encryption.lock().unwrap();
    let broken = if st.broken.is_empty() {
        std::mem::take(&mut kept.broken)
    } else {
        st.broken.clone()
    };
    *kept = encryption::State { broken, ..st };
}

const KEY_RETRY: Duration = Duration::from_secs(30);

/// A key that can't be had yet: ask once with a prompt (bounded by
/// `seal::PROMPT_WAIT`), then quietly every `KEY_RETRY`, and with a prompt
/// whenever ApplyEncryption asks. Queries find nothing meanwhile; Status says
/// why (`seal_waiting`).
fn wait_for_key(
    jobs: &mpsc::Receiver<Job>,
    status: &Status,
    db: &Path,
    want: &mut Want,
) -> Result<rusqlite::Connection> {
    let sealer = Sealer::default();
    let mut ask = Some(sealer.clone());
    loop {
        if let Some(w) = status.pending.take_encryption() {
            *want = w;
            ask = Some(sealer.asked());
        }
        let s = ask.take().unwrap_or_else(|| sealer.quiet());
        let w = *want;
        let stopping = std::thread::scope(|scope| {
            let asking = scope.spawn(|| apply_encryption(status, db, w, false, &s));
            while !asking.is_finished() {
                match jobs.recv_timeout(Duration::from_millis(50)) {
                    Ok(Job::Match { reply, .. }) => {
                        let _ = reply.send(Vec::new());
                    }
                    Ok(Job::Wake | Job::Retry) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return true,
                }
            }
            false
        });
        if stopping {
            bail!("the runner is stopping");
        }
        match index::open(db) {
            Ok(conn) => {
                eprintln!("the index's key is had: indexing and search resume");
                prepare_index(status, &conn)?;
                return Ok(conn);
            }
            Err(e) if seal::is_unavailable(&e) => {}
            Err(e) => return Err(e),
        }
        let until = Instant::now() + KEY_RETRY;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match jobs.recv_timeout(left) {
                Ok(Job::Match { reply, .. }) => {
                    let _ = reply.send(Vec::new());
                }
                Ok(Job::Wake | Job::Retry) => {
                    if let Some(w) = status.pending.take_encryption() {
                        *want = w;
                        ask = Some(sealer.asked());
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the runner is stopping"),
            }
        }
    }
}

pub fn model_changed(purged: &[(Kind, usize)]) -> Option<String> {
    let dropped = |kind: Kind| purged.iter().any(|(k, _)| *k == kind);
    let files: usize = purged.iter().map(|(_, n)| n).sum();
    let text = dropped(Kind::Text) || dropped(Kind::Document) || dropped(Kind::Code);
    let what = match (text, dropped(Kind::Image)) {
        (true, true) => "The models changed, so everything",
        (true, false) => "The text model changed, so text, documents and code",
        (false, true) => "The image model changed, so images",
        (false, false) => return None,
    };
    Some(format!("{what} ({files} files) will be indexed again."))
}

/// Settings given as flags or environment variables, which config changes leave alone.
#[derive(Clone, Copy, Default)]
pub struct Pinned {
    pub device: bool,
    pub idle_unload: bool,
}

struct Loaded<E> {
    emb: E,
    vectors: Vectors,
}

struct Rules {
    excludes: Excludes,
    types: FileTypes,
}

pub struct Options {
    pub openvino_dir: Option<PathBuf>,
    pub device: String,
    pub default_device: String,
    pub idle: Option<Duration>,
    pub excludes: Excludes,
    pub types: FileTypes,
    pub config: PathBuf,
    pub file: Config,
    pub pinned: Pinned,
    pub status: Arc<Status>,
    pub db: PathBuf,
}

/// Check one path against the index under the current settings: index it if
/// wanted and changed, drop it if gone, and for a directory find what under it
/// the index is behind on (to `work`). Gone matters there too: a directory
/// renamed away may be recreated at the same path before a debounced batch
/// runs, and its old files must not stay indexed.
///
/// A path excluded now, or under no root, is left alone: indexing it would
/// leave rows nothing removes. Removal still goes ahead.
fn check<E: Embed>(
    conn: &mut rusqlite::Connection,
    emb: &mut E,
    path: &Path,
    rules: &Rules,
    st: &mut index::Stats,
    work: &Work,
    between: &mut dyn FnMut(&rusqlite::Connection, &mut E) -> Result<()>,
) -> Result<()> {
    let Rules {
        excludes: ex,
        types,
    } = rules;
    if ex.contains(path) {
        return Ok(());
    }
    if !path.exists() {
        st.removed += index::remove(conn, path)?;
        return Ok(());
    }
    if !index::roots(conn)?.iter().any(|r| path.starts_with(r)) {
        return Ok(());
    }
    if index::is_real_dir(path) {
        work.found(index::stale(conn, path, ex, types)?);
        return Ok(());
    }
    st.files_seen += 1;
    match index::index_file(conn, emb, path, types, between)? {
        Outcome::Embedded(n) => {
            st.files_embedded += 1;
            st.chunks += n;
        }
        Outcome::Duplicate => st.duplicates += 1,
        Outcome::Unreadable { no_decoder } => st.no_decoder += usize::from(no_decoder),
        Outcome::Unchanged | Outcome::Skipped => {}
    }
    Ok(())
}

fn forget_root(conn: &rusqlite::Connection, db: &Path, work: &Work, root: &Path) {
    match index::forget(conn, root) {
        Ok(n) => eprintln!("forgot {}, {n} files removed", root.display()),
        Err(e) => eprintln!("forgetting {}: {e:#}", root.display()),
    }
    if let Err(e) = encryption::save_roots(db, conn) {
        eprintln!("saving the folder list: {e:#}");
    }
    work.forget(root, &index::roots(conn).unwrap_or_default());
}

/// vinoWhisper's session folders are roots while sessions are on, registered
/// as one added in the settings window is, and forgotten once off. A folder made
/// later is followed from the next start or settings change.
fn follow_sessions(conn: &rusqlite::Connection, db: &Path, types: &FileTypes, work: &Work) {
    let (dirs, on) = types.sessions_dirs();
    for dir in dirs {
        let is_root = index::roots(conn)
            .unwrap_or_default()
            .iter()
            .any(|r| r == dir);
        if on && !is_root && index::is_real_dir(dir) {
            work.root(dir.to_owned());
        } else if !on && is_root {
            forget_root(conn, db, work, dir);
        }
    }
}

/// Make `root` a root and find what under it the index is behind on; what it
/// finds waits in `Work` for a model.
fn register_root(
    conn: &rusqlite::Connection,
    db: &Path,
    root: &Path,
    rules: &Rules,
) -> Result<Vec<PathBuf>> {
    let root = index::resolve_root(root, &rules.excludes)?;
    index::add_root(conn, &root)?;
    if let Err(e) = encryption::save_roots(db, conn) {
        eprintln!("saving the folder list: {e:#}");
    }
    index::stale(conn, &root, &rules.excludes, &rules.types)
}

/// Best `k` chunks of each kind for `query`, text first, or of code alone for
/// one starting `code:`; what a Match job answers and `vinosearch eval` ranks.
pub(crate) fn matches<E: Embed>(
    conn: &rusqlite::Connection,
    emb: &mut E,
    vectors: &Vectors,
    query: &str,
    k: usize,
) -> Result<Vec<Hit>> {
    let (text, kinds) = index::scope(query);
    let q = emb.query_for(text, kinds)?;
    let words = index::Words::find(conn, text);
    let mut hits = Vec::new();
    for kind in kinds {
        hits.extend(index::search(conn, vectors, &q, words.as_ref(), *kind, k)?);
    }
    Ok(hits)
}

fn answer<E: Embed>(
    conn: &rusqlite::Connection,
    emb: &mut E,
    vectors: &Vectors,
    query: &str,
    k: usize,
    reply: oneshot::Sender<Vec<Hit>>,
) {
    let _ = reply.send(matches(conn, emb, vectors, query, k).unwrap_or_else(|e| {
        eprintln!("match {query:?}: {e:#}");
        Vec::new()
    }));
}

fn no_longer_excluded(old: &Excludes, new: &Excludes) -> Vec<PathBuf> {
    old.paths()
        .iter()
        .filter(|p| !new.contains(p) && index::is_real_dir(p))
        .cloned()
        .collect()
}

fn clear_compiled_cache() -> Result<()> {
    match std::fs::remove_dir_all(crate::cache_dir()?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// The most urgent item unless held: roots, changes, walk findings, then the
/// rest of long documents, then media on AC only (`power` sends a Retry). A
/// model that won't load leaves only roots.
fn next_item(
    status: &Status,
    conn: &rusqlite::Connection,
    rest_ready: bool,
    media_models: bool,
) -> Option<Item> {
    if status.held() {
        return None;
    }
    let waiting_model = status.waiting_model.load(Ordering::Relaxed);
    status.work.take(waiting_model).or_else(|| {
        let media = media_models && !status.power.on_battery();
        (rest_ready && !waiting_model && index::has_partial(conn, media).unwrap_or(false))
            .then_some(Item::Rest)
    })
}

/// Log the run; once nothing is left to check, a model change's re-indexing is done.
fn finish_run(status: &Status, run: index::Stats) {
    if run.files_embedded + run.removed + run.duplicates > 0 {
        eprintln!(
            "indexed: {} files embedded ({} chunks), {} duplicates, {} removed",
            run.files_embedded, run.chunks, run.duplicates, run.removed
        );
    }
    if !status.work.pending() {
        status.model_changed.lock().unwrap().clear();
    }
}

fn reencrypt<E: Embed>(
    status: &Status,
    db: &Path,
    conn: &mut rusqlite::Connection,
    loaded: &mut Option<Loaded<E>>,
    want: Want,
    rekey: bool,
) -> Result<()> {
    *conn = rusqlite::Connection::open_in_memory()?;
    apply_encryption(status, db, want, rekey, &Sealer::default().asked());
    *conn = index::open(db)?;
    if let Some(l) = loaded.as_mut() {
        l.vectors = Vectors::load(conn)?;
    }
    Ok(())
}

#[derive(Default)]
struct RestFailures(u32);

impl RestFailures {
    const TAKEN_FOR_THE_DEVICE: u32 = 2;

    fn succeeded(&mut self) {
        self.0 = 0;
    }

    fn failed(&mut self) -> bool {
        self.0 += 1;
        let give_up = self.0 >= Self::TAKEN_FOR_THE_DEVICE;
        if give_up {
            self.0 = 0;
        }
        give_up
    }
}

pub fn run(jobs: mpsc::Receiver<Job>, opts: Options) -> Result<()> {
    let openvino_dir = opts.openvino_dir.clone();
    run_with(jobs, opts, |device| {
        crate::embedder(openvino_dir.clone(), device)
    })
}

/// `run` with the model `load_model` gives: tests use a stand-in, with no OpenVINO.
fn run_with<E: Embed>(
    jobs: mpsc::Receiver<Job>,
    opts: Options,
    load_model: impl FnMut(&str) -> Result<E>,
) -> Result<()> {
    Worker::start(jobs, opts, load_model)?.run()
}

fn index_share(file: &Config) -> Result<f64> {
    config::index_share(
        file.index_busy_percent
            .unwrap_or(config::DEFAULT_INDEX_BUSY_PERCENT),
    )
}

struct Worker<E, L> {
    jobs: mpsc::Receiver<Job>,
    status: Arc<Status>,
    db: PathBuf,
    default_device: String,
    pinned: Pinned,
    load_model: L,
    conn: rusqlite::Connection,
    want: Want,
    rules: Rules,
    device: String,
    idle: Option<Duration>,
    pace: sched::Pace,
    loaded: Option<Loaded<E>>,
    /// False once the rest of long documents failed, until a Retry or a load.
    rest_ready: bool,
    rest_failures: RestFailures,
    /// False once media's models were missing, until a Retry (a fetch sends one).
    media_models: bool,
    run: index::Stats,
    did_work: bool,
    last_job: Instant,
    free_docs: Option<Instant>,
    timed_out: bool,
}

enum Waited {
    Job(Job),
    TimedOut,
    Closed,
}

impl<E: Embed, L: FnMut(&str) -> Result<E>> Worker<E, L> {
    fn start(jobs: mpsc::Receiver<Job>, opts: Options, load_model: L) -> Result<Self> {
        let mut want = Want::of(&opts.file);
        let conn = match index::open(&opts.db) {
            Err(e) if seal::is_unavailable(&e) => {
                wait_for_key(&jobs, &opts.status, &opts.db, &mut want)?
            }
            conn => conn?,
        };
        let rules = Rules {
            excludes: opts.excludes,
            types: opts.types,
        };
        let pace = sched::Pace::new(index_share(&opts.file).unwrap_or_else(|e| {
            eprintln!("{e:#}");
            config::index_share(config::DEFAULT_INDEX_BUSY_PERCENT).unwrap()
        }));
        opts.status
            .power
            .set_index_on_battery(opts.file.index_on_battery());
        let purged =
            index::purge(&conn, &rules.excludes)? + index::purge_types(&conn, &rules.types)?;
        if purged > 0 {
            eprintln!("removed {purged} excluded files from the index");
        }
        follow_sessions(&conn, &opts.db, &rules.types, &opts.status.work);
        Ok(Self {
            jobs,
            status: opts.status,
            db: opts.db,
            default_device: opts.default_device,
            pinned: opts.pinned,
            load_model,
            conn,
            want,
            rules,
            device: opts.device,
            idle: opts.idle,
            pace,
            loaded: None,
            rest_ready: true,
            rest_failures: RestFailures::default(),
            media_models: true,
            run: index::Stats::default(),
            did_work: false,
            last_job: Instant::now(),
            free_docs: None,
            timed_out: false,
        })
    }

    /// Each turn applies what arrived first, so anything taken after it is checked under it.
    fn run(mut self) -> Result<()> {
        loop {
            if !std::mem::take(&mut self.timed_out) {
                self.last_job = Instant::now();
            }
            let arrived = self.status.pending.take();
            if let Some(file) = arrived.config {
                self.apply_config(file)?;
            }
            for r in arrived.requests {
                self.apply_request(r)?;
            }
            let item = next_item(&self.status, &self.conn, self.rest_ready, self.media_models);
            let Some(item) = item else {
                if std::mem::take(&mut self.did_work) {
                    finish_run(&self.status, std::mem::take(&mut self.run));
                }
                match self.wait_for_job() {
                    Waited::Job(job) => self.take_job(job),
                    Waited::TimedOut => {}
                    Waited::Closed => break,
                }
                continue;
            };
            self.work_on(item, arrived.generation);
        }
        Ok(())
    }

    fn apply_config(&mut self, file: Config) -> Result<()> {
        match file.excludes() {
            Ok(ex) if ex != self.rules.excludes => {
                let allowed = no_longer_excluded(&self.rules.excludes, &ex);
                self.rules.excludes = ex;
                match index::purge(&self.conn, &self.rules.excludes) {
                    Ok(n) => eprintln!("exclusions changed, {n} files removed"),
                    Err(e) => eprintln!("applying exclusions: {e:#}"),
                }
                self.status.work.found(allowed);
            }
            Ok(_) => {}
            Err(e) => eprintln!("exclusions not applied: {e:#}"),
        }
        match FileTypes::new(&file.formats) {
            Ok(new) if new != self.rules.types => {
                self.rules.types = new;
                match index::purge_types(&self.conn, &self.rules.types) {
                    Ok(n) => eprintln!("file types changed, {n} files removed"),
                    Err(e) => eprintln!("applying file types: {e:#}"),
                }
                follow_sessions(&self.conn, &self.db, &self.rules.types, &self.status.work);
            }
            Ok(_) => {}
            Err(e) => eprintln!("file types not applied: {e:#}"),
        }
        if !self.pinned.device {
            let new = file.device.as_deref().unwrap_or(&self.default_device);
            if new != self.device {
                eprintln!("device changed to {new}");
                self.device = new.to_owned();
                *self.status.device.lock().unwrap() = self.device.clone();
                self.unload("device changed");
            }
        }
        match index_share(&file) {
            Ok(new) => self.pace.set(new),
            Err(e) => eprintln!("index_busy_percent not applied: {e:#}"),
        }
        self.status
            .power
            .set_index_on_battery(file.index_on_battery());
        if Want::of(&file) != self.want {
            self.want = Want::of(&file);
            self.reencrypt(false)?;
        }
        if !self.pinned.idle_unload {
            let m = file
                .idle_unload_minutes
                .unwrap_or(config::DEFAULT_IDLE_UNLOAD_MINUTES);
            match config::idle_unload(m) {
                Ok(new) if new != self.idle => {
                    eprintln!("idle unload changed to {m} minutes");
                    self.idle = new;
                }
                Ok(_) => {}
                Err(e) => eprintln!("idle_unload_minutes not applied: {e:#}"),
            }
        }
        Ok(())
    }

    fn apply_request(&mut self, r: Request) -> Result<()> {
        match r {
            Request::Forget(root) => forget_root(&self.conn, &self.db, &self.status.work, &root),
            Request::Encryption(w) => {
                self.want = w;
                self.reencrypt(false)?;
            }
            Request::Rekey => self.reencrypt(true)?,
            Request::Unload => self.unload("asked to"),
            Request::DeleteModel => {
                // Again: a FetchModel may have started while this waited.
                if self.status.fetching.load(Ordering::SeqCst) {
                    eprintln!("not deleting the model: a download started since it was asked");
                    return Ok(());
                }
                self.unload("deleting the model");
                match crate::ml::model::delete() {
                    Ok(()) => eprintln!("model deleted"),
                    Err(e) => eprintln!("deleting the model: {e:#}"),
                }
            }
            Request::ClearCache => match clear_compiled_cache() {
                Ok(()) => eprintln!("compiled-model cache cleared"),
                Err(e) => eprintln!("clearing the cache: {e:#}"),
            },
        }
        Ok(())
    }

    fn reencrypt(&mut self, rekey: bool) -> Result<()> {
        reencrypt(
            &self.status,
            &self.db,
            &mut self.conn,
            &mut self.loaded,
            self.want,
            rekey,
        )
    }

    fn load(&mut self) -> Result<()> {
        let emb = (self.load_model)(&self.device)?;
        let vectors = Vectors::load(&self.conn)?;
        eprintln!("serving {} chunks", vectors.len());
        self.loaded = Some(Loaded { emb, vectors });
        self.status.loaded.store(true, Ordering::Relaxed);
        self.rest_ready = true;
        Ok(())
    }

    fn unload(&mut self, why: &str) {
        if self.loaded.take().is_some() {
            self.status.loaded.store(false, Ordering::Relaxed);
            eprintln!("model unloaded ({why})");
        }
    }

    fn wait_for_job(&mut self) -> Waited {
        let unload_at = self
            .idle
            .filter(|_| self.loaded.is_some())
            .map(|d| self.last_job + d);
        let free_at = self.free_docs.filter(|_| self.loaded.is_some());
        let Some(at) = unload_at.into_iter().chain(free_at).min() else {
            return self.jobs.recv().map_or(Waited::Closed, Waited::Job);
        };
        match self
            .jobs
            .recv_timeout(at.saturating_duration_since(Instant::now()))
        {
            Ok(job) => Waited::Job(job),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.timed_out = true;
                self.free_docs = None;
                if Some(at) == unload_at {
                    let after = self.idle.unwrap_or_default().as_secs();
                    self.unload(&format!("idle for {after}s"));
                } else if let Some(l) = self.loaded.as_mut()
                    && l.emb.release_documents()
                {
                    eprintln!("document model freed");
                }
                Waited::TimedOut
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Waited::Closed,
        }
    }

    fn take_job(&mut self, job: Job) {
        match job {
            Job::Match { query, k, reply } => {
                if self.loaded.is_none() {
                    if let Err(e) = self.load() {
                        eprintln!("loading model: {e:#}");
                        let _ = reply.send(Vec::new());
                        return;
                    }
                    self.status.waiting_model.store(false, Ordering::Relaxed);
                }
                let Loaded { emb, vectors } = self.loaded.as_mut().unwrap();
                let had_docs = emb.has_documents();
                if let Err(e) = vectors.refresh(&self.conn) {
                    eprintln!("reloading vectors: {e:#}");
                }
                answer(&self.conn, emb, vectors, &query, k, reply);
                // A query too long for the query stage compiled the document
                // stage: free it as after indexing.
                if emb.has_documents() && !had_docs {
                    self.free_docs = Some(Instant::now() + DOCS_HOLD);
                }
            }
            Job::Wake => {}
            Job::Retry => {
                self.rest_ready = true;
                self.media_models = true;
                self.status.waiting_model.store(false, Ordering::Relaxed);
            }
        }
    }

    fn work_on(&mut self, item: Item, applied: u64) {
        self.status.working.store(true, Ordering::Relaxed);
        self.did_work = true;
        if let Item::Root(root) = item {
            let _idle = sched::Idle::enter();
            match register_root(&self.conn, &self.db, &root, &self.rules) {
                Ok(paths) => {
                    eprintln!("{}: {} paths to check", root.display(), paths.len());
                    self.status.walked(&root, Ok(paths.len() as u64));
                    self.status.work.found(paths);
                }
                Err(e) => {
                    eprintln!("indexing {}: {e:#}", root.display());
                    self.status.walked(&root, Err(format!("{e:#}")));
                }
            }
            self.status.working.store(false, Ordering::Relaxed);
            return;
        }
        self.status
            .indexing_rest
            .store(matches!(item, Item::Rest), Ordering::Relaxed);
        if self.loaded.is_none()
            && let Err(e) = self.load()
        {
            eprintln!("loading model: {e:#}");
            self.status.waiting_model.store(true, Ordering::Relaxed);
            match item {
                Item::Check(path) => self.status.work.changed([path]),
                Item::Rest => self.rest_ready = false,
                Item::Root(_) => unreachable!("needs no model"),
            }
            self.status.indexing_rest.store(false, Ordering::Relaxed);
            self.status.working.store(false, Ordering::Relaxed);
            return;
        }
        self.status.waiting_model.store(false, Ordering::Relaxed);
        self.index_item(item, applied);
        self.status.indexing_rest.store(false, Ordering::Relaxed);
        self.status.working.store(false, Ordering::Relaxed);
    }

    /// Index with the model loaded, at idle priority; stops between chunks for a hold or an
    /// arrival.
    fn index_item(&mut self, item: Item, applied: u64) {
        let Loaded { emb, vectors } = self.loaded.as_mut().unwrap();
        // Compiled before dropping to idle priority: OpenVINO's threads
        // inherit the compiling thread's priority and would stay at idle.
        if let Err(e) = emb.prepare_documents() {
            eprintln!("compiling the document model: {e:#}");
        }
        emb.set_document_share(self.pace.share());
        self.media_models = emb.can_embed_media();
        let idle = sched::Idle::enter();
        let mut retried = false;
        let status = &self.status;
        let pace = &mut self.pace;
        let jobs = &self.jobs;
        // Answers queries from the vectors as they were: a reload per file
        // would cost more than the query.
        let mut between = |conn: &rusqlite::Connection, emb: &mut E| -> Result<()> {
            if status.held() || status.pending.generation() != applied {
                return Err(Interrupted.into());
            }
            emb.set_document_share(pace.share());
            while let Ok(job) = jobs.try_recv() {
                match job {
                    Job::Match { query, k, reply } => {
                        idle.normal(|| answer(conn, emb, vectors, &query, k, reply));
                    }
                    Job::Wake => {}
                    Job::Retry => retried = true,
                }
            }
            Ok(())
        };
        let run = &mut self.run;
        let before = (run.files_embedded, run.removed, run.duplicates);
        match item {
            Item::Check(path) => {
                let r = check(
                    &mut self.conn,
                    emb,
                    &path,
                    &self.rules,
                    run,
                    &status.work,
                    &mut between,
                );
                match r {
                    Ok(()) => {}
                    Err(_) if interrupted(&r) => status.work.changed([path]),
                    Err(e) => eprintln!("{}: {e:#}", path.display()),
                }
            }
            Item::Rest => {
                let r = index::index_rest(&mut self.conn, emb, &self.rules.types, &mut between);
                match r {
                    Ok(Some(rest)) => {
                        match rest {
                            index::Rest::Parts {
                                path, done, total, ..
                            } if done >= total => eprintln!("{path}: all {total} parts in"),
                            index::Rest::Again(path) => {
                                eprintln!("{path} changed while partly indexed, indexed again")
                            }
                            _ => {}
                        }
                        self.rest_failures.succeeded();
                        status.last_indexed.store(unix_now(), Ordering::Relaxed);
                    }
                    Ok(None) => {}
                    Err(_) if interrupted(&r) => {}
                    Err(e) => {
                        eprintln!("indexing the rest of a long document: {e:#}");
                        if self.rest_failures.failed() {
                            self.rest_ready = false;
                        }
                    }
                }
            }
            Item::Root(_) => unreachable!("handled before loading"),
        }
        drop(idle);
        if retried {
            self.rest_ready = true;
            self.media_models = true;
        }
        if before != (run.files_embedded, run.removed, run.duplicates) {
            status.last_indexed.store(unix_now(), Ordering::Relaxed);
        }
        status
            .indexed_files
            .store(run.files_embedded as u64, Ordering::Relaxed);
        if emb.has_documents() {
            self.free_docs = Some(Instant::now() + DOCS_HOLD);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ml::embed::FakeEmbed;
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn one_failed_run_of_the_rest_is_tried_again() {
        let mut failures = RestFailures::default();
        assert!(!failures.failed());
    }

    #[test]
    fn a_second_failed_run_in_a_row_waits() {
        let mut failures = RestFailures::default();
        failures.failed();
        assert!(failures.failed());
    }

    #[test]
    fn a_run_that_worked_starts_the_count_of_failures_again() {
        let mut failures = RestFailures::default();
        failures.failed();
        failures.succeeded();
        assert!(!failures.failed());
    }

    #[test]
    fn work_takes_roots_then_changes_then_what_walks_found_each_once() {
        let work = Work::default();
        work.found([p("/h/b"), p("/h/c")]);
        work.changed([p("/h/b"), p("/h/a")]);
        work.root(p("/h"));
        work.root(p("/h"));
        work.found([p("/h/a")]);
        assert!(work.pending());
        let mut order = Vec::new();
        while let Some(item) = work.take(false) {
            order.push(item);
        }
        assert_eq!(
            order,
            [
                Item::Root(p("/h")),
                Item::Check(p("/h/a")),
                Item::Check(p("/h/b")),
                Item::Check(p("/h/c")),
            ]
        );
        assert!(!work.pending());
    }

    #[test]
    fn a_change_outranks_a_walks_finding_of_the_same_path() {
        let work = Work::default();
        work.found([p("/h/b")]);
        work.changed([p("/h/a")]);
        work.found([p("/h/a")]);
        assert_eq!(work.take(false), Some(Item::Check(p("/h/a"))));
        assert_eq!(work.take(false), Some(Item::Check(p("/h/b"))));
        assert_eq!(work.take(false), None);
    }

    #[test]
    fn a_root_added_while_the_watcher_keeps_finding_changes_is_taken_next() {
        let work = Work::default();
        work.changed([p("/h/a"), p("/h/b")]);
        assert_eq!(work.take(false), Some(Item::Check(p("/h/a"))));
        work.root(p("/h/dev"));
        work.changed([p("/h/c")]);
        assert_eq!(work.take(false), Some(Item::Root(p("/h/dev"))));
    }

    #[test]
    fn without_a_model_only_roots_are_taken() {
        let work = Work::default();
        work.changed([p("/h/a")]);
        work.root(p("/h"));
        work.found([p("/h/c")]);
        assert_eq!(work.take(true), Some(Item::Root(p("/h"))));
        assert_eq!(work.take(true), None);
        assert!(work.pending());
        assert_eq!(work.take(false), Some(Item::Check(p("/h/a"))));
    }

    #[test]
    fn forgetting_a_root_drops_it_and_what_no_other_root_covers() {
        let work = Work::default();
        work.root(p("/h/dev"));
        work.root(p("/h/docs"));
        work.changed([p("/h/dev/proj/x.rs"), p("/h/dev/y.md"), p("/h/docs/z.md")]);
        work.found([p("/h/dev/w.md")]);
        let other_roots = [p("/h/dev/proj"), p("/h/docs")];
        work.forget(&p("/h/dev"), &other_roots);
        let mut left = Vec::new();
        while let Some(item) = work.take(false) {
            left.push(item);
        }
        assert_eq!(
            left,
            [
                Item::Root(p("/h/docs")),
                Item::Check(p("/h/dev/proj/x.rs")),
                Item::Check(p("/h/docs/z.md")),
            ]
        );
    }

    #[test]
    fn pending_keeps_the_latest_config_and_every_request() {
        let pending = Pending::default();
        let first = pending.take();
        assert!(first.config.is_none() && first.requests.is_empty());
        pending.config(Config {
            exclude: vec!["~/a".into()],
            ..Config::default()
        });
        pending.config(Config {
            exclude: vec!["~/b".into()],
            ..Config::default()
        });
        pending.request(Request::Unload);
        pending.request(Request::Forget(p("/h/dev")));
        let arrived = pending.take();
        assert_eq!(arrived.generation, first.generation + 3);
        assert_eq!(arrived.config.unwrap().exclude, ["~/b"]);
        assert_eq!(
            arrived.requests,
            [Request::Unload, Request::Forget(p("/h/dev"))]
        );
        let again = pending.take();
        assert_eq!(again.generation, first.generation + 3);
        assert!(again.config.is_none() && again.requests.is_empty());
    }

    #[test]
    fn unload_waits_for_the_file_being_indexed_and_forget_stops_it() {
        let pending = Pending::default();
        let g0 = pending.generation();
        pending.request(Request::Unload);
        assert_eq!(pending.generation(), g0);
        pending.request(Request::Forget(p("/h/dev")));
        assert_eq!(pending.generation(), g0 + 1);
    }

    struct Harness {
        dir: PathBuf,
        status: Arc<Status>,
        embedded: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("vinosearch-worker-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("root")).unwrap();
            Harness {
                dir: fs::canonicalize(&dir).unwrap(),
                status: Arc::new(Status::new("CPU")),
                embedded: Arc::default(),
            }
        }

        fn root(&self) -> PathBuf {
            self.dir.join("root")
        }

        fn db(&self) -> PathBuf {
            self.dir.join("index.sqlite")
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.root().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }

        fn run(&self, on_document: impl FnMut(&str) + Send + 'static) {
            let (tx, rx) = mpsc::channel();
            let file = Config::default();
            let opts = Options {
                openvino_dir: None,
                device: "CPU".into(),
                default_device: "CPU".into(),
                idle: None,
                excludes: file.excludes().unwrap(),
                types: FileTypes::new(&file.formats).unwrap(),
                config: self.dir.join("config.toml"),
                file,
                pinned: Pinned::default(),
                status: self.status.clone(),
                db: self.db(),
            };
            let embedded = self.embedded.clone();
            let mut hook: Option<crate::ml::embed::OnDocument> = Some(Box::new(on_document));
            let worker = std::thread::spawn(move || {
                run_with(rx, opts, move |_| {
                    let embedded = embedded.clone();
                    let mut hook = hook.take();
                    Ok(FakeEmbed {
                        on_document: Some(Box::new(move |text: &str| {
                            embedded.fetch_add(1, Ordering::Relaxed);
                            if let Some(h) = hook.as_mut() {
                                h(text);
                            }
                        })),
                        ..FakeEmbed::default()
                    })
                })
            });
            drop(tx);
            worker.join().unwrap().unwrap();
        }

        fn indexed(&self) -> Vec<String> {
            let conn = index::open(&self.db()).unwrap();
            let paths = conn
                .prepare("SELECT path FROM files ORDER BY path")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let root = self.root();
            paths
                .iter()
                .map(|p| {
                    Path::new(p)
                        .strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }

        fn roots(&self) -> Vec<PathBuf> {
            index::roots(&index::open(&self.db()).unwrap()).unwrap()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn long(word: &str) -> String {
        format!("{word} ").repeat(600)
    }

    #[test]
    fn vinowhispers_folder_is_a_root_while_sessions_are_on() {
        let h = Harness::new("sessions");
        let dir = h.root().join(".local/share/vinowhisper/transcripts");
        fs::create_dir_all(&dir).unwrap();
        let conn = index::open(&h.db()).unwrap();
        let on = FileTypes::default().with_sessions(true, Some(dir.clone()));
        follow_sessions(&conn, &h.db(), &on, &h.status.work);
        assert!(matches!(h.status.work.take(true), Some(Item::Root(r)) if r == dir));
        index::add_root(&conn, &dir).unwrap();
        follow_sessions(&conn, &h.db(), &on, &h.status.work);
        assert!(!h.status.work.pending(), "already a root");
        let off = FileTypes::default().with_sessions(false, Some(dir.clone()));
        follow_sessions(&conn, &h.db(), &off, &h.status.work);
        assert!(h.roots().is_empty());
    }

    #[test]
    fn a_root_is_registered_walked_and_indexed() {
        let h = Harness::new("root");
        h.write("fruit.md", "apples and pears\n");
        h.write("notes/bread.txt", "banana bread with walnuts\n");
        h.status.work.root(h.root());
        h.run(|_| {});
        assert_eq!(h.roots(), [h.root()]);
        assert_eq!(h.indexed(), ["fruit.md", "notes/bread.txt"]);
        assert_eq!(h.embedded.load(Ordering::Relaxed), 2);
        assert_eq!(h.status.indexed_files.load(Ordering::Relaxed), 2);
        assert!(h.status.last_indexed.load(Ordering::Relaxed) > 0);
        assert!(!h.status.indexing());
        let first = h.status.walks.lock().unwrap().clone();
        assert!(
            matches!(first.as_slice(), [Walk { root, found: Ok(2), .. }] if *root == h.root()),
            "{first:?}"
        );

        h.status.work.root(h.root());
        h.run(|_| {});
        let again = h.status.walks.lock().unwrap().clone();
        assert!(
            matches!(again.as_slice(), [Walk { found: Ok(0), seq, .. }] if *seq > first[0].seq),
            "a walk again that finds nothing new replaces the last, and is told from it: {again:?}"
        );
    }

    #[test]
    fn a_changed_path_is_checked_under_the_settings_of_the_moment() {
        let h = Harness::new("changed");
        let keep = h.write("keep.md", "apples and pears\n");
        let gone = h.write("gone.md", "a file that will be removed\n");
        let dir = h.root().join("new");
        h.write("new/inside.md", "banana bread with walnuts\n");
        let outside = h.dir.join("outside.md");
        fs::write(&outside, "under no root\n").unwrap();
        h.status.work.root(h.root());
        h.run(|_| {});
        assert_eq!(h.indexed(), ["gone.md", "keep.md", "new/inside.md"]);

        fs::remove_file(&gone).unwrap();
        fs::write(&keep, "apples, pears and plums\n").unwrap();
        h.write("new/another.md", "cherry pie\n");
        h.status.work.changed([keep, gone, dir, outside]);
        h.embedded.store(0, Ordering::Relaxed);
        h.run(|_| {});
        assert_eq!(
            h.indexed(),
            ["keep.md", "new/another.md", "new/inside.md"],
            "a directory is checked for what is new under it, a path under no root left alone"
        );
        assert_eq!(h.embedded.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn an_exclusion_arriving_mid_file_stops_it_within_a_chunk() {
        let h = Harness::new("exclude");
        h.write("keep.md", "apples and pears\n");
        h.write("private/a.md", &long("secret"));
        h.write("private/b.md", &long("secret"));
        h.status.work.root(h.root());
        let status = h.status.clone();
        let private = h.root().join("private");
        let mut excluded = false;
        h.run(move |text| {
            if text.contains("secret") && !std::mem::replace(&mut excluded, true) {
                status.pending.config(Config {
                    exclude: vec![private.to_string_lossy().into_owned()],
                    ..Config::default()
                });
            }
        });
        assert_eq!(h.indexed(), ["keep.md"]);
        let keep_and_the_chunk_before_the_stop = 2;
        assert_eq!(
            h.embedded.load(Ordering::Relaxed),
            keep_and_the_chunk_before_the_stop
        );
        assert!(!h.status.work.pending());
    }

    #[test]
    fn a_forget_arriving_mid_file_drops_the_root_and_what_waited_under_it() {
        let h = Harness::new("forget");
        h.write("a.md", &long("alpha"));
        h.write("b.md", &long("beta"));
        h.status.work.root(h.root());
        let status = h.status.clone();
        let root = h.root();
        let mut forgot = false;
        h.run(move |_| {
            if !std::mem::replace(&mut forgot, true) {
                status.pending.request(Request::Forget(root.clone()));
            }
        });
        assert!(h.roots().is_empty());
        assert!(h.indexed().is_empty());
        assert_eq!(h.embedded.load(Ordering::Relaxed), 1);
        assert!(!h.status.work.pending());
    }

    #[test]
    fn held_work_waits_and_does_not_count_as_indexing() {
        let h = Harness::new("held");
        h.write("a.md", "apples\n");
        h.status.work.changed([h.root().join("a.md")]);
        h.status.paused.store(true, Ordering::Relaxed);
        assert!(!h.status.indexing());
        h.status.paused.store(false, Ordering::Relaxed);
        assert!(h.status.indexing());
        h.status.waiting_model.store(true, Ordering::Relaxed);
        assert!(!h.status.indexing());
    }

    #[test]
    fn media_without_its_models_waits_and_the_model_is_loaded_once_to_see() {
        let h = Harness::new("media-no-models");
        let clip = h.write("clip.wav", "not decoded in this test");
        let (mtime, size) = index::stamp_of(&fs::metadata(&clip).unwrap()).unwrap();
        let conn = index::open(&h.db()).unwrap();
        conn.execute(
            "INSERT INTO files (id, path, mtime, size, sha256) VALUES (1, ?1, ?2, ?3, x'aa')",
            rusqlite::params![clip.to_str().unwrap(), mtime, size],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO partial (file_id, done, total, media) VALUES (1, 0, 40, 1)",
            [],
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let file = Config::default();
        let opts = Options {
            openvino_dir: None,
            device: "CPU".into(),
            default_device: "CPU".into(),
            idle: None,
            excludes: file.excludes().unwrap(),
            types: FileTypes::new(&file.formats)
                .unwrap()
                .with_media(true, Some(3600.0)),
            config: h.dir.join("config.toml"),
            file,
            pinned: Pinned::default(),
            status: h.status.clone(),
            db: h.db(),
        };
        let loads = Arc::new(AtomicUsize::new(0));
        let counted = loads.clone();
        let worker = std::thread::spawn(move || {
            run_with(rx, opts, move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(FakeEmbed {
                    no_media_models: true,
                    ..FakeEmbed::default()
                })
            })
        });
        tx.send(Job::Retry).unwrap();
        drop(tx);
        worker.join().unwrap().unwrap();
        assert_eq!(
            loads.load(Ordering::Relaxed),
            1,
            "a Retry has it look once more, and no more than that"
        );
        assert_eq!(index::partial(&conn, 10).unwrap().len(), 1);
    }
}
