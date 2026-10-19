//! The one thread that owns the model and the index.
//!
//! OpenVINO's handles are not Send, so queries (from the KRunner interface),
//! changed paths (from the watcher) and control requests come here over a
//! channel. The model is loaded on demand and dropped, with the vector cache,
//! after `idle` without a job, as the other vino projects do. What the worker
//! is doing is published in `Status`, so status queries never wait behind it.

use crate::config::{self, Config, Excludes};
use crate::embed::Embedder;
use crate::index::{self, Hit, Outcome, Vectors};
use crate::sched;
use anyhow::Result;
use futures_channel::oneshot;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

pub enum Job {
    /// Best `k` chunks for a query.
    Match {
        query: String,
        k: usize,
        reply: oneshot::Sender<Vec<Hit>>,
    },
    /// Paths created, modified, renamed or removed under a root.
    Changed(Vec<PathBuf>),
    /// The config file changed: apply its exclusions (dropping what is now
    /// excluded), device and idle timeout, except where a flag overrides them.
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
    /// The model may load now (a download finished): run the indexing that
    /// waited for it.
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
            files.extend(index::stale(conn, &path, ex)?);
        } else {
            files.push(path);
        }
    }
    let mut st = index::Stats {
        files_seen: 0,
        files_embedded: 0,
        chunks: 0,
        removed: 0,
        duplicates: 0,
    };
    for path in files {
        if path.exists() {
            st.files_seen += 1;
            match index::index_file(conn, emb, &path, &mut |c, e| after(&st, c, e))? {
                Outcome::Embedded(n) => {
                    st.files_embedded += 1;
                    st.chunks += n;
                }
                Outcome::Duplicate => st.duplicates += 1,
                Outcome::Unchanged | Outcome::Skipped => {}
            }
        } else {
            st.removed += index::remove(conn, &path)?;
        }
        after(&st, conn, emb);
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
    let hits = emb
        .query(query)
        .and_then(|q| index::search(conn, vectors, &q, k));
    let _ = reply.send(hits.unwrap_or_else(|e| {
        eprintln!("match {query:?}: {e:#}");
        Vec::new()
    }));
}

pub fn run(jobs: mpsc::Receiver<Job>, opts: Options) -> Result<()> {
    let mut conn = index::open(&index::default_db()?)?;
    let mut excludes = opts.excludes.clone();
    let mut device = opts.device.clone();
    let mut idle = opts.idle;
    let purged = index::purge(&conn, &excludes)?;
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
    loop {
        let job = if let Some(job) = deferred.pop_front() {
            job
        } else {
            match (idle, loaded.is_some()) {
                (Some(after), true) => match jobs.recv_timeout(after) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        unload(&mut loaded, &format!("idle for {}s", after.as_secs()));
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                _ => match jobs.recv() {
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
            Job::Forget(root) => {
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
                match crate::model::default_dir().and_then(|d| crate::model::delete(&d)) {
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
        if indexes && let Err(e) = emb.prepare_documents() {
            eprintln!("compiling the document model: {e:#}");
        }
        let idle = indexes.then(sched::Idle::enter);
        let mut after = |st: &index::Stats, conn: &rusqlite::Connection, emb: &mut Embedder| {
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
                    other => deferred.push_back(other),
                }
            }
        };
        match job {
            Job::Match { query, k, reply } => {
                if let Err(e) = vectors.refresh(&conn) {
                    eprintln!("reloading vectors: {e:#}");
                }
                answer(&conn, emb, vectors, &query, k, reply);
            }
            Job::Changed(paths) => {
                match changed(&mut conn, emb, paths, &excludes, &mut after) {
                    Ok(s) => touched(status, &s),
                    Err(e) => eprintln!("updating index: {e:#}"),
                }
                status.indexing.store(false, Ordering::Relaxed);
            }
            Job::Reindex(root) => {
                match index::index(&mut conn, emb, &root, &excludes, &mut after) {
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
    }
    Ok(())
}
