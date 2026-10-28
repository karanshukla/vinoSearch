//! The one thread that owns the model and the index.
//!
//! OpenVINO's handles are not Send, so queries (from the KRunner interface)
//! come here over a channel. Everything else is state the worker brings the
//! index in line with, a step at a time (#137): the settings as the watcher
//! last read them and requests that change the index (`Pending`), and the
//! paths that need checking (`Work`), each once however often it was asked
//! for. A step applies what changed in the settings, then takes the most
//! urgent item: a path the watcher saw change, a root to index, a path a
//! walk found, the rest of a long document. No item carries a copy of the
//! settings, so each is checked under the current ones, and a file being
//! indexed when they change stops after its chunk and is checked again.
//!
//! The model is loaded on demand and dropped, with the vector cache, after
//! `idle` without a job, as the other vino projects do. What the worker is
//! doing is published in `Status`, so status queries never wait behind it.

use crate::config::{self, Config, Excludes};
use crate::embed::Embed;
use crate::encryption::{self, Want};
use crate::index::{self, FileTypes, Hit, Kind, Outcome, Vectors};
use crate::sched;
use anyhow::Result;
use futures_channel::oneshot;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// How long the document stage stays compiled after its last use. It holds
/// ~430 MB on top of the query stage and reloads from the cache in ~0.1 s,
/// so it goes long before the model's idle unload, which queries keep
/// putting off. Long enough to cover a run of saves a few seconds apart.
const DOCS_HOLD: Duration = Duration::from_secs(60);

pub enum Job {
    /// Best `k` chunks of each kind for a query, text first; of code alone
    /// for one starting `code:` (#110).
    Match {
        query: String,
        k: usize,
        reply: oneshot::Sender<Vec<Hit>>,
    },
    /// Something arrived in `Work` or `Pending`: look, if the worker is
    /// waiting for a job.
    Wake,
    /// The model may load now (a download finished), or indexing was
    /// resumed (a pause ended, power allows it): run what waited for it.
    Retry,
}

/// A change to the index outside indexing, applied at the worker's next
/// step. Those that drop or convert what a file being indexed may be going
/// into stop that file first (`stops_indexing`); the others wait for it.
#[derive(Debug, PartialEq)]
pub enum Request {
    /// Stop indexing and watching a root, and drop what was indexed under it.
    Forget(PathBuf),
    /// Bring the index to the config file's `encryption` now, rather than
    /// at the watcher's next read of it, or try again after a refusal.
    Encryption(Want),
    /// Encrypt the index again under a new key.
    Rekey,
    /// Free the model now instead of waiting for the idle timeout.
    Unload,
    /// Unload and delete the model's files.
    DeleteModel,
    /// Delete OpenVINO's compiled-model cache (the next load recompiles).
    ClearCache,
}

impl Request {
    fn stops_indexing(&self) -> bool {
        matches!(
            self,
            Request::Forget(_) | Request::Encryption(_) | Request::Rekey
        )
    }
}

/// The config file as the watcher last read it, and requests that change
/// the index, for the worker to apply at its next step: nothing waits in a
/// queue behind indexing. The generation moves with every write that a file
/// being indexed must not run past (new exclusions, a forgotten root,
/// encryption): the worker stops after the chunk it is on, applies what is
/// here, and checks the file again under the new settings.
#[derive(Default)]
pub struct Pending {
    generation: AtomicU64,
    inner: Mutex<(Option<Config>, Vec<Request>)>,
}

impl Pending {
    /// The config file changed. The latest one stands; its exclusions and
    /// file types (dropping what is now excluded or turned off), device,
    /// idle timeout and the rest apply, except where a flag overrides them.
    pub fn config(&self, file: Config) {
        let mut p = self.inner.lock().unwrap();
        p.0 = Some(file);
        // Under the lock, so what `take` returns is current to the count it
        // reads: a count moved after a take means another take to come.
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn request(&self, r: Request) {
        let mut p = self.inner.lock().unwrap();
        let stops = r.stops_indexing();
        p.1.push(r);
        if stops {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// What arrived since the last take, with the count it is current to.
    fn take(&self) -> (u64, Option<Config>, Vec<Request>) {
        let mut p = self.inner.lock().unwrap();
        let generation = self.generation.load(Ordering::Relaxed);
        (generation, p.0.take(), std::mem::take(&mut p.1))
    }
}

/// Paths to check against the index, each once however many times it was
/// asked for: the watcher's debounced batches, its check of every root and
/// a Reindex of one can all name the same file. The worker takes the most
/// urgent: a path the watcher saw change (what the user is working on), then
/// a root to register and walk, then what a walk found the index behind on.
/// Held work (paused, for power, or for a model that failed to load) simply
/// stays here, and queries never wait behind any of it.
#[derive(Default)]
pub struct Work(Mutex<Set>);

#[derive(Default)]
struct Set {
    changes: BTreeSet<PathBuf>,
    roots: Vec<PathBuf>,
    stale: BTreeSet<PathBuf>,
}

/// One step of indexing.
#[derive(Debug, PartialEq)]
pub enum Item {
    /// Index the file if it is wanted and changed, drop it if it is gone,
    /// and for a directory find what under it the index is behind on.
    Check(PathBuf),
    /// Make the path a root (as `vinosearch index` does) and find what under
    /// it needs checking. Needs no model.
    Root(PathBuf),
    /// The next run of parts of a long document (#108), once nothing else
    /// waits. The worker makes this itself.
    Rest,
}

impl Work {
    /// Paths created, modified, renamed or removed under a root: first in
    /// line, ahead of any walk's findings.
    pub fn changed(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut s = self.0.lock().unwrap();
        for p in paths {
            s.stale.remove(&p);
            s.changes.insert(p);
        }
    }

    /// Paths a walk found the index behind on (a root's, or the watcher's
    /// check of every root), behind what changed.
    pub fn found(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut s = self.0.lock().unwrap();
        for p in paths {
            if !s.changes.contains(&p) {
                s.stale.insert(p);
            }
        }
    }

    /// A root to index, new or existing, in the order asked.
    pub fn root(&self, root: PathBuf) {
        let mut s = self.0.lock().unwrap();
        if !s.roots.contains(&root) {
            s.roots.push(root);
        }
    }

    /// The most urgent item; with `roots_only`, the next root (whose walk
    /// needs no model) and nothing else.
    fn take(&self, roots_only: bool) -> Option<Item> {
        let mut s = self.0.lock().unwrap();
        if !roots_only && let Some(p) = s.changes.pop_first() {
            return Some(Item::Check(p));
        }
        if !s.roots.is_empty() {
            return Some(Item::Root(s.roots.remove(0)));
        }
        if roots_only {
            return None;
        }
        s.stale.pop_first().map(Item::Check)
    }

    /// Drop a forgotten root: a Reindex of it still waiting would make it a
    /// root again, and the paths under it, except where one of `roots`
    /// (which can nest) still covers them.
    fn forget(&self, root: &Path, roots: &[PathBuf]) {
        let mut s = self.0.lock().unwrap();
        s.roots.retain(|r| r != root);
        let covered = |p: &PathBuf| !p.starts_with(root) || roots.iter().any(|r| p.starts_with(r));
        s.changes.retain(covered);
        s.stale.retain(covered);
    }

    /// Anything waits to be checked.
    pub fn pending(&self) -> bool {
        let s = self.0.lock().unwrap();
        !(s.changes.is_empty() && s.roots.is_empty() && s.stale.is_empty())
    }
}

/// What the runner is doing, for status queries. The worker owns the model
/// and indexing fields; the model download runs on its own thread.
#[derive(Default)]
pub struct Status {
    pub loaded: AtomicBool,
    /// The worker is on an item of work, model load included.
    pub working: AtomicBool,
    /// Files embedded so far by the current run of indexing: since the
    /// worker last found nothing to do.
    pub indexed_files: AtomicU64,
    /// The item is the rest of a long document (#108).
    pub indexing_rest: AtomicBool,
    pub fetching: AtomicBool,
    pub fetch_done: AtomicU64,
    pub fetch_total: AtomicU64,
    /// Why the last model download failed, cleared when one starts.
    pub fetch_error: std::sync::Mutex<String>,
    /// The device the model runs, or will next load, on.
    pub device: std::sync::Mutex<String>,
    /// Unix seconds when the watcher last compared every root with the
    /// index, and when indexing last changed it; 0 for not yet.
    pub last_checked: AtomicU64,
    pub last_indexed: AtomicU64,
    /// Set by CheckNow; the watcher takes it.
    pub check_now: AtomicBool,
    /// Set by ApplyEncryption: the watcher takes it and reads the config
    /// file now, rather than at its next 30-second read.
    pub reload_config: AtomicBool,
    /// Settings and requests for the worker's next step.
    pub pending: Pending,
    /// Paths for the worker to check.
    pub work: Work,
    /// The model failed to load (not fetched yet, or the device failed):
    /// what needs it waits for a Retry, or for a query to load it.
    pub waiting_model: AtomicBool,
    /// Indexing is paused: a file being indexed stops after its current
    /// chunk, and the rest waits until Retry.
    pub paused: AtomicBool,
    /// What may hold indexing for power (#109), as `paused` does.
    pub power: crate::power::State,
    /// Why this start dropped part of the index to be indexed again (a model
    /// changed, #80), until that indexing is done; empty otherwise.
    pub model_changed: std::sync::Mutex<String>,
    /// Whether the index is encrypted, and why a change to that failed (#4).
    pub encryption: std::sync::Mutex<encryption::State>,
    /// systemd can seal a key to a TPM here, as of this start.
    pub tpm: AtomicBool,
}

/// An indexing item stopped part way, to be run again.
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

    /// Indexing waits: paused, or held for power.
    pub fn held(&self) -> bool {
        self.paused.load(Ordering::Relaxed) || self.power.hold().is_some()
    }

    /// Indexing is running, or will as soon as the worker gets to it: what
    /// Control1's Status reports (true from the moment Reindex returns), and
    /// what the watcher waits out before comparing every root with the
    /// index. False while the work is held, or waits for the model.
    pub fn indexing(&self) -> bool {
        self.working.load(Ordering::Relaxed)
            || (!self.held() && !self.waiting_model.load(Ordering::Relaxed) && self.work.pending())
    }
}

/// Before the watcher compares the roots with the index: encrypt or
/// decrypt it as the config file asks, or rebuild it if its key is lost
/// (#4); drop what must be indexed again (vectors from another model, #80;
/// photos skipped for want of a decoder that is installed now, #79; scans
/// OCR can read now, #106; files of a format read differently now, #97), so
/// that first comparison finds it, and say so in `status` for the Index page.
pub fn prepare(status: &Status, file: &Config) -> Result<()> {
    let db = index::default_db()?;
    encryption::hold_shared(&db)?;
    status
        .tpm
        .store(crate::seal::tpm_available(), Ordering::Relaxed);
    apply_encryption(status, &db, Want::of(file), false);
    let conn = index::open(&db)?;
    let purged = index::purge_other_models(&conn)?;
    if let Some(why) = model_changed(&purged) {
        eprintln!("{why}");
        *status.model_changed.lock().unwrap() = why;
    }
    let retry = index::retry_undecodable(&conn)?;
    if retry > 0 {
        eprintln!("a HEIF decoder is installed now: {retry} photos will be indexed");
    }
    let ocr = crate::model::ocr_dir().is_ok_and(|d| crate::model::ocr_present(&d));
    let scans = index::retry_scans(&conn, ocr)?;
    if scans > 0 {
        eprintln!("{scans} PDFs with no text will be read by OCR");
    }
    let reread = index::reread_formats(&conn)?;
    if reread > 0 {
        eprintln!("{reread} files are read differently now and will be indexed again");
    }
    Ok(())
}

/// `encryption::apply`, its outcome logged and kept for Status. A rebuild
/// reported earlier stays until a later one replaces it.
fn apply_encryption(status: &Status, db: &std::path::Path, want: Want, rekey: bool) {
    let st = encryption::apply(db, want, rekey, &crate::seal::Sealer::default());
    if !st.error.is_empty() {
        eprintln!("encryption: {}", st.error);
    }
    let mut kept = status.encryption.lock().unwrap();
    let broken = if st.broken.is_empty() {
        std::mem::take(&mut kept.broken)
    } else {
        st.broken.clone()
    };
    *kept = encryption::State { broken, ..st };
}

/// One sentence on what `purge_other_models` dropped, for the Index page;
/// None when nothing was.
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

/// Settings given as flags or environment variables, which config file
/// changes leave alone.
#[derive(Clone, Copy, Default)]
pub struct Pinned {
    pub device: bool,
    pub idle_unload: bool,
}

struct Loaded<E> {
    emb: E,
    vectors: Vectors,
}

/// What a check goes by: the exclusions and file types as they are now.
struct Rules {
    excludes: Excludes,
    types: FileTypes,
}

pub struct Options {
    pub openvino_dir: Option<PathBuf>,
    pub device: String,
    pub idle: Option<Duration>,
    pub excludes: Excludes,
    pub types: FileTypes,
    /// The config file the settings came from, which the watcher re-reads.
    pub config: PathBuf,
    /// Its contents at startup.
    pub file: Config,
    pub pinned: Pinned,
    pub status: Arc<Status>,
    /// The index.
    pub db: PathBuf,
}

/// Check one path against the index under the settings as they are now:
/// index it if it is a wanted file that changed, drop it if it is gone,
/// and for a directory (new, or renamed in) find what under it the index
/// is behind on, which goes to `work`. Gone matters there too: by the time
/// a debounced batch runs, a directory renamed away may already have been
/// recreated at the same path, and its old files must not stay indexed.
///
/// A path excluded now, or under no root (forgotten since the path was
/// queued), is left alone: indexing it would leave rows nothing ever
/// removes. Removal still goes ahead.
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

/// vinoWhisper's folder of sessions is a root while sessions are on
/// (#143), registered as one added in the settings window is, and
/// forgotten once they're off. A folder vinoWhisper makes later is
/// followed from the next start or change of settings.
fn follow_sessions(conn: &rusqlite::Connection, db: &Path, types: &FileTypes, work: &Work) {
    let Some((dir, on)) = types.sessions_dir() else {
        return;
    };
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

/// Make `root` a root, as `vinosearch index` does, and find what under it
/// the index is behind on, as the watcher's check of a root does. Needs no
/// model: what it finds waits in `Work` for one.
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

/// Best `k` chunks of each kind for `query`, text first, or of code alone
/// for one starting `code:` (#110): what a Match job answers, and what
/// `vinosearch eval` ranks (#107).
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

pub fn run(jobs: mpsc::Receiver<Job>, opts: Options) -> Result<()> {
    let openvino_dir = opts.openvino_dir.clone();
    run_with(jobs, opts, |device| {
        crate::embedder(openvino_dir.clone(), device)
    })
}

/// `run` with the model `load_model` gives for a device: tests run the
/// worker through a stand-in, with no OpenVINO.
fn run_with<E: Embed>(
    jobs: mpsc::Receiver<Job>,
    opts: Options,
    mut load_model: impl FnMut(&str) -> Result<E>,
) -> Result<()> {
    let db = opts.db.clone();
    let mut conn = index::open(&db)?;
    let mut want = Want::of(&opts.file);
    let mut rules = Rules {
        excludes: opts.excludes.clone(),
        types: opts.types.clone(),
    };
    let mut device = opts.device.clone();
    let mut idle = opts.idle;
    let share = |file: &Config| {
        config::index_share(
            file.index_busy_percent
                .unwrap_or(config::DEFAULT_INDEX_BUSY_PERCENT),
        )
    };
    let mut pace = sched::Pace::new(share(&opts.file).unwrap_or_else(|e| {
        eprintln!("{e:#}");
        config::index_share(config::DEFAULT_INDEX_BUSY_PERCENT).unwrap()
    }));
    opts.status
        .power
        .set_index_on_battery(opts.file.index_on_battery());
    let purged = index::purge(&conn, &rules.excludes)? + index::purge_types(&conn, &rules.types)?;
    if purged > 0 {
        eprintln!("removed {purged} excluded files from the index");
    }
    follow_sessions(&conn, &db, &rules.types, &opts.status.work);
    let mut load = |conn: &rusqlite::Connection, device: &str| -> Result<Loaded<E>> {
        let emb = load_model(device)?;
        let vectors = Vectors::load(conn)?;
        eprintln!("serving {} chunks", vectors.len());
        Ok(Loaded { emb, vectors })
    };
    let status = &opts.status;
    // Encrypt, decrypt or re-key (#4): the worker's connection closed while
    // the file is replaced, and the vector cache read again from the new one.
    let reencrypt = |conn: &mut rusqlite::Connection,
                     loaded: &mut Option<Loaded<E>>,
                     want: Want,
                     rekey: bool|
     -> Result<()> {
        *conn = rusqlite::Connection::open_in_memory()?;
        apply_encryption(status, &db, want, rekey);
        *conn = index::open(&db)?;
        if let Some(l) = loaded.as_mut() {
            l.vectors = Vectors::load(conn)?;
        }
        Ok(())
    };
    let mut loaded: Option<Loaded<E>> = None;
    let unload = |loaded: &mut Option<Loaded<E>>, why: &str| {
        if loaded.take().is_some() {
            status.loaded.store(false, Ordering::Relaxed);
            eprintln!("model unloaded ({why})");
        }
    };
    // Whether the rest of long documents may run when nothing else waits:
    // not after it failed to, until a Retry or a load succeeds.
    let mut rest_ready = true;
    // What this run of indexing (since the worker last had nothing to do)
    // has done, for Status and one line in the log; and whether it did
    // anything at all.
    let mut run = index::Stats::default();
    let mut did_work = false;
    // When the last job finished, and when to free the document stage.
    let mut last_job = Instant::now();
    let mut free_docs: Option<Instant> = None;
    let mut timed_out = false;
    loop {
        if !std::mem::take(&mut timed_out) {
            last_job = Instant::now();
        }
        // The settings and requests first: anything taken below is checked
        // under them. `applied` is the count they are current to.
        let (applied, file, requests) = status.pending.take();
        if let Some(file) = file {
            match file.excludes() {
                Ok(ex) if ex != rules.excludes => {
                    // What is no longer excluded: index it (its files are
                    // new to the index, and a check of each finds them).
                    let allowed: Vec<PathBuf> = rules
                        .excludes
                        .paths()
                        .iter()
                        .filter(|p| !ex.contains(p) && index::is_real_dir(p))
                        .cloned()
                        .collect();
                    rules.excludes = ex;
                    match index::purge(&conn, &rules.excludes) {
                        Ok(n) => eprintln!("exclusions changed, {n} files removed"),
                        Err(e) => eprintln!("applying exclusions: {e:#}"),
                    }
                    status.work.found(allowed);
                }
                Ok(_) => {}
                Err(e) => eprintln!("exclusions not applied: {e:#}"),
            }
            // Turned on, the watcher checks every root for what is wanted now.
            match FileTypes::new(&file.formats) {
                Ok(new) if new != rules.types => {
                    rules.types = new;
                    match index::purge_types(&conn, &rules.types) {
                        Ok(n) => eprintln!("file types changed, {n} files removed"),
                        Err(e) => eprintln!("applying file types: {e:#}"),
                    }
                    follow_sessions(&conn, &db, &rules.types, &status.work);
                }
                Ok(_) => {}
                Err(e) => eprintln!("file types not applied: {e:#}"),
            }
            if !opts.pinned.device {
                let new = file.device.as_deref().unwrap_or(config::DEFAULT_DEVICE);
                if new != device {
                    eprintln!("device changed to {new}");
                    device = new.to_owned();
                    *status.device.lock().unwrap() = device.clone();
                    // Loaded again, on the new device, by the next item that needs it.
                    unload(&mut loaded, "device changed");
                }
            }
            match share(&file) {
                Ok(new) => pace.set(new),
                Err(e) => eprintln!("index_busy_percent not applied: {e:#}"),
            }
            // Released, the work waiting for a charger is simply taken below.
            status.power.set_index_on_battery(file.index_on_battery());
            if Want::of(&file) != want {
                want = Want::of(&file);
                reencrypt(&mut conn, &mut loaded, want, false)?;
            }
            if !opts.pinned.idle_unload {
                let m = file
                    .idle_unload_minutes
                    .unwrap_or(config::DEFAULT_IDLE_UNLOAD_MINUTES);
                match config::idle_unload(m) {
                    Ok(new) if new != idle => {
                        eprintln!("idle unload changed to {m} minutes");
                        idle = new;
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("idle_unload_minutes not applied: {e:#}"),
                }
            }
        }
        for r in requests {
            match r {
                Request::Forget(root) => forget_root(&conn, &db, &status.work, &root),
                Request::Encryption(w) => {
                    want = w;
                    reencrypt(&mut conn, &mut loaded, want, false)?;
                }
                Request::Rekey => reencrypt(&mut conn, &mut loaded, want, true)?,
                Request::Unload => unload(&mut loaded, "asked to"),
                Request::DeleteModel => {
                    // Checked again here: this may have waited for a file
                    // while a FetchModel started.
                    if status.fetching.load(Ordering::SeqCst) {
                        eprintln!("not deleting the model: a download started since it was asked");
                        continue;
                    }
                    unload(&mut loaded, "deleting the model");
                    match crate::model::delete() {
                        Ok(()) => eprintln!("model deleted"),
                        Err(e) => eprintln!("deleting the model: {e:#}"),
                    }
                }
                Request::ClearCache => {
                    // Loaded stages are compiled already; only the next load reads the cache.
                    match crate::cache_dir().and_then(|d| match std::fs::remove_dir_all(&d) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                        _ => Ok(()),
                    }) {
                        Ok(()) => eprintln!("compiled-model cache cleared"),
                        Err(e) => eprintln!("clearing the cache: {e:#}"),
                    }
                }
            }
        }
        // Then the most urgent item, unless the work is held. A model that
        // won't load leaves roots to register, whose walk needs none.
        let waiting_model = status.waiting_model.load(Ordering::Relaxed);
        let item = if status.held() {
            None
        } else {
            status.work.take(waiting_model).or_else(|| {
                // The first pass over everything is done; the rest of long
                // documents is what's left, a run of parts at a time.
                (rest_ready && !waiting_model && index::has_partial(&conn).unwrap_or(false))
                    .then_some(Item::Rest)
            })
        };
        let Some(item) = item else {
            if std::mem::take(&mut did_work) {
                if run.files_embedded + run.removed + run.duplicates > 0 {
                    eprintln!(
                        "indexed: {} files embedded ({} chunks), {} duplicates, {} removed",
                        run.files_embedded, run.chunks, run.duplicates, run.removed
                    );
                }
                run = index::Stats::default();
                // The re-indexing a model change asked for is done once
                // nothing is left to check.
                if !status.work.pending() {
                    status.model_changed.lock().unwrap().clear();
                }
            }
            let unload_at = idle.filter(|_| loaded.is_some()).map(|d| last_job + d);
            let free_at = free_docs.filter(|_| loaded.is_some());
            let job = match unload_at.into_iter().chain(free_at).min() {
                Some(at) => match jobs.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        timed_out = true;
                        free_docs = None;
                        if Some(at) == unload_at {
                            let after = idle.unwrap_or_default().as_secs();
                            unload(&mut loaded, &format!("idle for {after}s"));
                        } else if let Some(l) = loaded.as_mut()
                            && l.emb.release_documents()
                        {
                            eprintln!("document model freed");
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match jobs.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                },
            };
            match job {
                Job::Match { query, k, reply } => {
                    if loaded.is_none() {
                        match load(&conn, &device) {
                            Ok(l) => {
                                loaded = Some(l);
                                status.loaded.store(true, Ordering::Relaxed);
                                status.waiting_model.store(false, Ordering::Relaxed);
                                rest_ready = true;
                            }
                            Err(e) => {
                                eprintln!("loading model: {e:#}");
                                let _ = reply.send(Vec::new());
                                continue;
                            }
                        }
                    }
                    let Loaded { emb, vectors } = loaded.as_mut().unwrap();
                    let had_docs = emb.has_documents();
                    if let Err(e) = vectors.refresh(&conn) {
                        eprintln!("reloading vectors: {e:#}");
                    }
                    answer(&conn, emb, vectors, &query, k, reply);
                    // A query too long for the query stage compiled the
                    // document stage: free it as after indexing. Short
                    // queries leave the timer alone.
                    if emb.has_documents() && !had_docs {
                        free_docs = Some(Instant::now() + DOCS_HOLD);
                    }
                }
                Job::Wake => {}
                Job::Retry => {
                    rest_ready = true;
                    status.waiting_model.store(false, Ordering::Relaxed);
                }
            }
            continue;
        };
        status.working.store(true, Ordering::Relaxed);
        did_work = true;
        if let Item::Root(root) = item {
            let _idle = sched::Idle::enter();
            match register_root(&conn, &db, &root, &rules) {
                Ok(paths) => {
                    eprintln!("{}: {} paths to check", root.display(), paths.len());
                    status.work.found(paths);
                }
                Err(e) => eprintln!("indexing {}: {e:#}", root.display()),
            }
            status.working.store(false, Ordering::Relaxed);
            continue;
        }
        status
            .indexing_rest
            .store(matches!(item, Item::Rest), Ordering::Relaxed);
        if loaded.is_none() {
            match load(&conn, &device) {
                Ok(l) => {
                    loaded = Some(l);
                    status.loaded.store(true, Ordering::Relaxed);
                    rest_ready = true;
                }
                Err(e) => {
                    eprintln!("loading model: {e:#}");
                    status.waiting_model.store(true, Ordering::Relaxed);
                    match item {
                        Item::Check(path) => status.work.changed([path]),
                        // Tried again once a load succeeds.
                        Item::Rest => rest_ready = false,
                        Item::Root(_) => unreachable!("needs no model"),
                    }
                    status.indexing_rest.store(false, Ordering::Relaxed);
                    status.working.store(false, Ordering::Relaxed);
                    continue;
                }
            }
        }
        status.waiting_model.store(false, Ordering::Relaxed);
        let Loaded { emb, vectors } = loaded.as_mut().unwrap();
        // Indexing runs at idle priority and stops between chunks for
        // queries, which are answered at normal priority from the vectors
        // as they were (a reload per file would cost more than the query).
        // Compile the document stage first: OpenVINO's threads created by
        // the compile inherit the caller's priority, and at idle they would
        // stay there.
        if let Err(e) = emb.prepare_documents() {
            eprintln!("compiling the document model: {e:#}");
        }
        emb.set_document_share(pace.share());
        let idle = sched::Idle::enter();
        let mut retried = false;
        // Between chunks: stop for a pause or a hold, or for settings and
        // requests that arrived (the item goes back first in line, and is
        // checked again under them); answer queries.
        let mut between = |conn: &rusqlite::Connection, emb: &mut E| -> Result<()> {
            if status.held() || status.pending.generation() != applied {
                return Err(Interrupted.into());
            }
            // Plugged in or unplugged mid-file, or the setting changed.
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
        let before = (run.files_embedded, run.removed, run.duplicates);
        match item {
            Item::Check(path) => {
                let r = check(
                    &mut conn,
                    emb,
                    &path,
                    &rules,
                    &mut run,
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
                let r = index::index_rest(&mut conn, emb, &rules.types, &mut between);
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
                        status.last_indexed.store(unix_now(), Ordering::Relaxed);
                    }
                    Ok(None) => {}
                    // Held or stopped: the run goes again from where the
                    // index has it.
                    Err(_) if interrupted(&r) => {}
                    Err(e) => {
                        eprintln!("indexing the rest of a long document: {e:#}");
                        rest_ready = false;
                    }
                }
            }
            Item::Root(_) => unreachable!("handled before loading"),
        }
        drop(idle);
        if retried {
            rest_ready = true;
        }
        if before != (run.files_embedded, run.removed, run.duplicates) {
            status.last_indexed.store(unix_now(), Ordering::Relaxed);
        }
        status
            .indexed_files
            .store(run.files_embedded as u64, Ordering::Relaxed);
        if emb.has_documents() {
            free_docs = Some(Instant::now() + DOCS_HOLD);
        }
        status.indexing_rest.store(false, Ordering::Relaxed);
        status.working.store(false, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::FakeEmbed;
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn work_takes_changes_then_roots_then_what_walks_found_each_once() {
        let work = Work::default();
        work.found([p("/h/b"), p("/h/c")]);
        work.changed([p("/h/b"), p("/h/a")]);
        work.root(p("/h"));
        work.root(p("/h"));
        // A change outranks a walk's finding of the same path.
        work.found([p("/h/a")]);
        assert!(work.pending());
        let mut order = Vec::new();
        while let Some(item) = work.take(false) {
            order.push(item);
        }
        assert_eq!(
            order,
            [
                Item::Check(p("/h/a")),
                Item::Check(p("/h/b")),
                Item::Root(p("/h")),
                Item::Check(p("/h/c")),
            ]
        );
        assert!(!work.pending());
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
        // ~/dev/proj is a root of its own, and stays.
        work.forget(&p("/h/dev"), &[p("/h/dev/proj"), p("/h/docs")]);
        let mut left = Vec::new();
        while let Some(item) = work.take(false) {
            left.push(item);
        }
        assert_eq!(
            left,
            [
                Item::Check(p("/h/dev/proj/x.rs")),
                Item::Check(p("/h/docs/z.md")),
                Item::Root(p("/h/docs")),
            ]
        );
    }

    #[test]
    fn pending_keeps_the_latest_config_and_every_request() {
        let pending = Pending::default();
        let (g0, file, requests) = pending.take();
        assert!(file.is_none() && requests.is_empty());
        pending.config(Config {
            exclude: vec!["~/a".into()],
            ..Config::default()
        });
        pending.config(Config {
            exclude: vec!["~/b".into()],
            ..Config::default()
        });
        // Unload waits for the file being indexed; Forget stops it.
        pending.request(Request::Unload);
        assert_eq!(pending.generation(), g0 + 2);
        pending.request(Request::Forget(p("/h/dev")));
        assert_eq!(pending.generation(), g0 + 3);
        let (g, file, requests) = pending.take();
        assert_eq!(g, g0 + 3);
        assert_eq!(file.unwrap().exclude, ["~/b"]);
        assert_eq!(requests, [Request::Unload, Request::Forget(p("/h/dev"))]);
        let (g, file, requests) = pending.take();
        assert_eq!(g, g0 + 3);
        assert!(file.is_none() && requests.is_empty());
    }

    /// A temporary tree and index of their own, removed when dropped.
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

        /// Run the worker over what `status` holds until the channel closes
        /// (it is dropped here, so the worker stops once nothing is left),
        /// with `on_document` called before each chunk is embedded.
        fn run(&self, on_document: impl FnMut(&str) + Send + 'static) {
            let (tx, rx) = mpsc::channel();
            let file = Config::default();
            let opts = Options {
                openvino_dir: None,
                device: "CPU".into(),
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
            let mut hook: Option<crate::embed::OnDocument> = Some(Box::new(on_document));
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

    /// Enough text for several chunks, so a file can be stopped part way.
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
        // Already a root: nothing more to do.
        follow_sessions(&conn, &h.db(), &on, &h.status.work);
        assert!(!h.status.work.pending());
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
        // A directory is checked for what under it is new; a path under no
        // root is left alone.
        h.status.work.changed([keep, gone, dir, outside]);
        h.embedded.store(0, Ordering::Relaxed);
        h.run(|_| {});
        assert_eq!(h.indexed(), ["keep.md", "new/another.md", "new/inside.md"]);
        assert_eq!(h.embedded.load(Ordering::Relaxed), 2);
    }

    /// The check #136 added: a file being indexed when the exclusions
    /// change stops after its chunk, and the exclusions apply before any
    /// more of it, or anything else, is indexed.
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
        // keep.md, and the one chunk of private/a.md before the stop.
        assert_eq!(h.embedded.load(Ordering::Relaxed), 2);
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
}
