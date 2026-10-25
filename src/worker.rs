//! The one thread that owns the model and the index.
//!
//! OpenVINO's handles are not Send, so queries (from the KRunner interface),
//! changed paths (from the watcher) and control requests come here over a
//! channel. The model is loaded on demand and dropped, with the vector cache,
//! after `idle` without a job, as the other vino projects do. What the worker
//! is doing is published in `Status`, so status queries never wait behind it.

use crate::config::{self, Config, Excludes};
use crate::embed::Embedder;
use crate::index::{self, FileTypes, Hit, Kind, Outcome, Vectors};
use crate::sched;
use anyhow::Result;
use futures_channel::oneshot;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the document stage stays compiled after its last use. It holds
/// ~430 MB on top of the query stage and reloads from the cache in ~0.1 s,
/// so it goes long before the model's idle unload, which queries keep
/// putting off. Long enough to cover a run of saves a few seconds apart.
const DOCS_HOLD: Duration = Duration::from_secs(60);

pub enum Job {
    /// Best `k` chunks of each kind for a query, text first.
    Match {
        query: String,
        k: usize,
        reply: oneshot::Sender<Vec<Hit>>,
    },
    /// Paths created, modified, renamed or removed under a root.
    Changed(Vec<PathBuf>),
    /// The config file changed: apply its exclusions and file types (dropping
    /// what is now excluded or turned off), device and idle timeout, except
    /// where a flag overrides them.
    Config(Config),
    /// Index a root (new or existing), as `vinosearch index` does.
    Reindex(PathBuf),
    /// Stop indexing and watching a root, and drop what was indexed under it.
    Forget(PathBuf),
    /// Free the model now instead of waiting for the idle timeout.
    Unload,
    /// Unload and delete the model's files.
    DeleteModel,
    /// Delete OpenVINO's compiled-model cache (the next load recompiles).
    ClearCache,
    /// The model may load now (a download finished), or indexing was
    /// resumed: run the indexing that waited.
    Retry,
}

/// What the runner is doing, for status queries. The worker owns the model
/// and indexing fields; the model download runs on its own thread.
#[derive(Default)]
pub struct Status {
    pub loaded: AtomicBool,
    /// The worker is on an indexing job, model load included.
    pub indexing: AtomicBool,
    /// Reindex jobs queued and not yet finished; Control counts them in as it
    /// sends, so Status says indexing from the moment Reindex returns.
    pub queued_reindex: AtomicUsize,
    /// Files embedded so far by the current indexing job.
    pub indexed_files: AtomicU64,
    pub fetching: AtomicBool,
    pub fetch_done: AtomicU64,
    pub fetch_total: AtomicU64,
    /// Why the last model download failed, cleared when one starts.
    pub fetch_error: std::sync::Mutex<String>,
    /// The device the model runs, or will next load, on.
    pub device: std::sync::Mutex<String>,
    /// Unix seconds when the watcher last compared every root with the
    /// index, and when an indexing job last changed it; 0 for not yet.
    pub last_checked: AtomicU64,
    pub last_indexed: AtomicU64,
    /// Set by CheckNow; the watcher takes it.
    pub check_now: AtomicBool,
    /// Indexing is paused: a running job stops after its current chunk, and
    /// new ones wait until Retry.
    pub paused: AtomicBool,
    /// What may hold indexing for power (#109), as `paused` does.
    pub power: crate::power::State,
    /// Why this start dropped part of the index to be indexed again (a model
    /// changed, #80), until that indexing is done; empty otherwise.
    pub model_changed: std::sync::Mutex<String>,
}

/// An indexing job stopped part way, to be run again.
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
}

/// Before the watcher compares the roots with the index: drop what must be
/// indexed again (vectors from another model, #80; photos skipped for want
/// of a decoder that is installed now, #79; scans OCR can read now, #106;
/// files of a format read differently now, #97), so that first comparison
/// finds it, and say so in
/// `status` for the Index page.
pub fn prepare(status: &Status) -> Result<()> {
    let conn = index::open(&index::default_db()?)?;
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

/// One sentence on what `purge_other_models` dropped, for the Index page;
/// None when nothing was.
pub fn model_changed(purged: &[(Kind, usize)]) -> Option<String> {
    let dropped = |kind: Kind| purged.iter().any(|(k, _)| *k == kind);
    let files: usize = purged.iter().map(|(_, n)| n).sum();
    let text = dropped(Kind::Text) || dropped(Kind::Document);
    let what = match (text, dropped(Kind::Image)) {
        (true, true) => "The models changed, so everything",
        (true, false) => "The text model changed, so text and documents",
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

struct Loaded {
    emb: Embedder,
    vectors: Vectors,
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
}

fn changed(
    conn: &mut rusqlite::Connection,
    emb: &mut Embedder,
    paths: Vec<PathBuf>,
    ex: &Excludes,
    types: &FileTypes,
    after: &mut index::AfterFile,
) -> Result<index::Stats> {
    let roots = index::roots(conn)?;
    let mut files = Vec::new();
    for path in paths {
        if ex.contains(&path) {
            continue;
        }
        // Under a root no longer followed (Forget, with this batch already
        // queued): indexing it would leave rows nothing ever removes. Removal
        // still goes ahead.
        if path.exists() && !roots.iter().any(|r| path.starts_with(r)) {
            continue;
        }
        if index::is_real_dir(&path) {
            // A new or renamed-in directory: what under it is new, changed or
            // gone. Gone matters too: by the time a debounced batch runs, a
            // directory renamed away may already have been recreated at the
            // same path, and its old files must not stay indexed.
            files.extend(index::stale(conn, &path, ex, types)?);
        } else {
            files.push(path);
        }
    }
    let mut st = index::Stats::default();
    for path in files {
        if path.exists() {
            st.files_seen += 1;
            match index::index_file(conn, emb, &path, types, &mut |c, e| after(&st, c, e))? {
                Outcome::Embedded(n) => {
                    st.files_embedded += 1;
                    st.chunks += n;
                }
                Outcome::Duplicate => st.duplicates += 1,
                Outcome::Unreadable { no_decoder } => st.no_decoder += usize::from(no_decoder),
                Outcome::Unchanged | Outcome::Skipped => {}
            }
        } else {
            st.removed += index::remove(conn, &path)?;
        }
        after(&st, conn, emb)?;
    }
    if st.files_embedded + st.removed + st.duplicates > 0 {
        eprintln!(
            "updated: {} files embedded ({} chunks), {} duplicates, {} removed",
            st.files_embedded, st.chunks, st.duplicates, st.removed
        );
    }
    Ok(st)
}

/// Record when the index last changed, for Status.
fn touched(status: &Status, st: &index::Stats) {
    if st.files_embedded + st.removed + st.duplicates > 0 {
        status.last_indexed.store(unix_now(), Ordering::Relaxed);
    }
}

fn answer(
    conn: &rusqlite::Connection,
    emb: &mut Embedder,
    vectors: &Vectors,
    query: &str,
    k: usize,
    reply: oneshot::Sender<Vec<Hit>>,
) {
    let hits = emb.query(query).and_then(|q| {
        let mut hits = Vec::new();
        for kind in [Kind::Text, Kind::Document, Kind::Image] {
            hits.extend(index::search(conn, vectors, &q, kind, k)?);
        }
        Ok(hits)
    });
    let _ = reply.send(hits.unwrap_or_else(|e| {
        eprintln!("match {query:?}: {e:#}");
        Vec::new()
    }));
}

pub fn run(jobs: mpsc::Receiver<Job>, opts: Options) -> Result<()> {
    let mut conn = index::open(&index::default_db()?)?;
    let mut excludes = opts.excludes.clone();
    let mut types = opts.types.clone();
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
    let purged = index::purge(&conn, &excludes)? + index::purge_types(&conn, &types)?;
    if purged > 0 {
        eprintln!("removed {purged} excluded files from the index");
    }
    let load = |conn: &rusqlite::Connection, device: &str| -> Result<Loaded> {
        let emb = crate::embedder(opts.openvino_dir.clone(), device)?;
        let vectors = Vectors::load(conn)?;
        eprintln!("serving {} chunks", vectors.len());
        Ok(Loaded { emb, vectors })
    };
    let status = &opts.status;
    let mut loaded: Option<Loaded> = None;
    // Jobs that arrived while indexing and were not queries, run after it.
    let mut deferred: VecDeque<Job> = VecDeque::new();
    // Indexing that could not run because the model failed to load (not
    // fetched yet, or the device failed), run after the next load succeeds.
    let mut waiting: HashSet<PathBuf> = HashSet::new();
    let mut waiting_roots: Vec<PathBuf> = Vec::new();
    let replay = |waiting: &mut HashSet<PathBuf>,
                  waiting_roots: &mut Vec<PathBuf>,
                  deferred: &mut VecDeque<Job>| {
        if !waiting.is_empty() {
            deferred.push_back(Job::Changed(waiting.drain().collect()));
        }
        for root in waiting_roots.drain(..) {
            status.queued_reindex.fetch_add(1, Ordering::Relaxed);
            deferred.push_back(Job::Reindex(root));
        }
    };
    let unload = |loaded: &mut Option<Loaded>, why: &str| {
        if loaded.take().is_some() {
            status.loaded.store(false, Ordering::Relaxed);
            eprintln!("model unloaded ({why})");
        }
    };
    // When the last job finished, and when to free the document stage.
    let mut last_job = Instant::now();
    let mut free_docs: Option<Instant> = None;
    let mut timed_out = false;
    loop {
        if !std::mem::take(&mut timed_out) {
            last_job = Instant::now();
        }
        let job = if let Some(job) = deferred.pop_front() {
            job
        } else {
            let unload_at = idle.filter(|_| loaded.is_some()).map(|d| last_job + d);
            let free_at = free_docs.filter(|_| loaded.is_some());
            match unload_at.into_iter().chain(free_at).min() {
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
            }
        };
        // Jobs that need no model.
        let job = match job {
            Job::Retry => {
                replay(&mut waiting, &mut waiting_roots, &mut deferred);
                continue;
            }
            Job::Config(file) => {
                match file.excludes() {
                    Ok(ex) if ex != excludes => {
                        // What is no longer excluded: index it (its files are
                        // new to the index, and changed() finds them).
                        let allowed: Vec<PathBuf> = excludes
                            .paths()
                            .iter()
                            .filter(|p| !ex.contains(p) && index::is_real_dir(p))
                            .cloned()
                            .collect();
                        excludes = ex;
                        match index::purge(&conn, &excludes) {
                            Ok(n) => eprintln!("exclusions changed, {n} files removed"),
                            Err(e) => eprintln!("applying exclusions: {e:#}"),
                        }
                        if !allowed.is_empty() {
                            deferred.push_back(Job::Changed(allowed));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("exclusions not applied: {e:#}"),
                }
                // Turned on, the watcher checks every root for what is wanted now.
                match FileTypes::new(&file.formats) {
                    Ok(new) if new != types => {
                        types = new;
                        match index::purge_types(&conn, &types) {
                            Ok(n) => eprintln!("file types changed, {n} files removed"),
                            Err(e) => eprintln!("applying file types: {e:#}"),
                        }
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
                        // Loaded again, on the new device, by the next job that needs it.
                        unload(&mut loaded, "device changed");
                    }
                }
                match share(&file) {
                    Ok(new) => pace.set(new),
                    Err(e) => eprintln!("index_busy_percent not applied: {e:#}"),
                }
                if status.power.set_index_on_battery(file.index_on_battery()) {
                    replay(&mut waiting, &mut waiting_roots, &mut deferred);
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
                continue;
            }
            // Held while paused or held for power, with the work that waits
            // for the model.
            Job::Changed(paths) if status.held() => {
                waiting.extend(paths);
                continue;
            }
            Job::Reindex(root) if status.held() => {
                status.queued_reindex.fetch_sub(1, Ordering::Relaxed);
                waiting_roots.push(root);
                continue;
            }
            Job::Forget(root) => {
                // A Reindex of it still queued (one this Forget interrupted,
                // say) would make it a root again.
                let before = deferred.len();
                deferred.retain(|j| !matches!(j, Job::Reindex(r) if *r == root));
                status
                    .queued_reindex
                    .fetch_sub(before - deferred.len(), Ordering::Relaxed);
                waiting_roots.retain(|r| *r != root);
                match index::forget(&conn, &root) {
                    Ok(n) => eprintln!("forgot {}, {n} files removed", root.display()),
                    Err(e) => eprintln!("forgetting {}: {e:#}", root.display()),
                }
                continue;
            }
            Job::Unload => {
                unload(&mut loaded, "asked to");
                continue;
            }
            Job::DeleteModel => {
                // Checked again here: this may have waited behind indexing
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
                continue;
            }
            Job::ClearCache => {
                // Loaded stages are compiled already; only the next load reads the cache.
                match crate::cache_dir().and_then(|d| match std::fs::remove_dir_all(&d) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                    _ => Ok(()),
                }) {
                    Ok(()) => eprintln!("compiled-model cache cleared"),
                    Err(e) => eprintln!("clearing the cache: {e:#}"),
                }
                continue;
            }
            job => job,
        };
        let indexes = matches!(job, Job::Changed(_) | Job::Reindex(_));
        if indexes {
            status.indexed_files.store(0, Ordering::Relaxed);
            status.indexing.store(true, Ordering::Relaxed);
        }
        if loaded.is_none() {
            match load(&conn, &device) {
                Ok(l) => {
                    loaded = Some(l);
                    status.loaded.store(true, Ordering::Relaxed);
                    replay(&mut waiting, &mut waiting_roots, &mut deferred);
                }
                Err(e) => {
                    eprintln!("loading model: {e:#}");
                    match job {
                        Job::Match { reply, .. } => {
                            let _ = reply.send(Vec::new());
                        }
                        Job::Reindex(root) => {
                            status.queued_reindex.fetch_sub(1, Ordering::Relaxed);
                            waiting_roots.push(root);
                        }
                        Job::Changed(paths) => waiting.extend(paths),
                        _ => {}
                    }
                    status.indexing.store(false, Ordering::Relaxed);
                    continue;
                }
            }
        }
        let Loaded { emb, vectors } = loaded.as_mut().unwrap();
        // Indexing runs at idle priority and stops between files for queries,
        // which are answered at normal priority from the vectors as they were
        // (a reload per file would cost more than the query). Anything else
        // waits for the job to finish.
        // Compile the document stage first: OpenVINO's threads created by the
        // compile inherit the caller's priority, and at idle they would stay
        // there.
        let had_docs = emb.has_documents();
        if indexes && let Err(e) = emb.prepare_documents() {
            eprintln!("compiling the document model: {e:#}");
        }
        emb.set_document_share(pace.share());
        let idle = indexes.then(sched::Idle::enter);
        // Set when a change drops files this job may be indexing (new
        // exclusions, a forgotten root): the job stops, the change applies,
        // and the job runs again after it.
        let mut restart = false;
        let mut after = |st: &index::Stats, conn: &rusqlite::Connection, emb: &mut Embedder| {
            if status.held() || restart {
                return Err(Interrupted.into());
            }
            // Plugged in or unplugged mid-job, or the setting changed.
            emb.set_document_share(pace.share());
            status
                .indexed_files
                .store(st.files_embedded as u64, Ordering::Relaxed);
            while let Ok(job) = jobs.try_recv() {
                match job {
                    Job::Match { query, k, reply } => {
                        let serve = || answer(conn, emb, vectors, &query, k, reply);
                        match &idle {
                            Some(idle) => idle.normal(serve),
                            None => serve(),
                        }
                    }
                    // The pace and index_on_battery apply now; new
                    // exclusions stop the job so they apply before it runs
                    // again.
                    Job::Config(file) => {
                        if let Ok(new) = share(&file) {
                            pace.set(new);
                        }
                        if status.power.set_index_on_battery(file.index_on_battery()) {
                            deferred.push_back(Job::Retry);
                        }
                        restart |= file.excludes().is_ok_and(|ex| ex != excludes)
                            || FileTypes::new(&file.formats).is_ok_and(|t| t != types);
                        deferred.push_back(Job::Config(file));
                    }
                    Job::Forget(root) => {
                        restart = true;
                        deferred.push_back(Job::Forget(root));
                    }
                    other => deferred.push_back(other),
                }
            }
            Ok(())
        };
        match job {
            Job::Match { query, k, reply } => {
                if let Err(e) = vectors.refresh(&conn) {
                    eprintln!("reloading vectors: {e:#}");
                }
                answer(&conn, emb, vectors, &query, k, reply);
            }
            Job::Changed(paths) => {
                let r = changed(&mut conn, emb, paths.clone(), &excludes, &types, &mut after);
                match r {
                    Ok(s) => touched(status, &s),
                    // Jobs held while paused go back through the main loop.
                    Err(_) if interrupted(&r) => deferred.push_back(Job::Changed(paths)),
                    Err(e) => eprintln!("updating index: {e:#}"),
                }
                status.indexing.store(false, Ordering::Relaxed);
            }
            Job::Reindex(root) => {
                let r = index::index(&mut conn, emb, &root, &excludes, &types, &mut after);
                if interrupted(&r) {
                    eprintln!("indexing {} stopped", root.display());
                    status.queued_reindex.fetch_add(1, Ordering::Relaxed);
                    deferred.push_back(Job::Reindex(root.clone()));
                }
                match r {
                    Err(_) if interrupted(&r) => {}
                    Ok(s) => {
                        eprintln!(
                            "{}: {} files seen, {} embedded ({} chunks), {} removed",
                            root.display(),
                            s.files_seen,
                            s.files_embedded,
                            s.chunks,
                            s.removed
                        );
                        touched(status, &s);
                    }
                    Err(e) => eprintln!("indexing {}: {e:#}", root.display()),
                }
                status.queued_reindex.fetch_sub(1, Ordering::Relaxed);
                status.indexing.store(false, Ordering::Relaxed);
            }
            Job::Config(_)
            | Job::Forget(_)
            | Job::Unload
            | Job::DeleteModel
            | Job::ClearCache
            | Job::Retry => {
                unreachable!("handled before loading")
            }
        }
        // Used by this job: indexing, or a query too long for the query stage
        // that compiled it. Short queries leave the timer alone.
        if emb.has_documents() && (indexes || !had_docs) {
            free_docs = Some(Instant::now() + DOCS_HOLD);
        }
        // The re-indexing a model change asked for is done once no indexing
        // is left queued.
        if indexes
            && status.queued_reindex.load(Ordering::Relaxed) == 0
            && !deferred
                .iter()
                .any(|j| matches!(j, Job::Changed(_) | Job::Reindex(_)))
        {
            status.model_changed.lock().unwrap().clear();
        }
    }
    Ok(())
}
