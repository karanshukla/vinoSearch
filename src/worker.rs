//! The one thread that owns the model and the index.
//!
//! OpenVINO's handles are not Send, so queries (from the KRunner interface),
//! changed paths (from the watcher) and control requests come here over a
//! channel. The model is loaded on demand and dropped, with the vector cache,
//! after `idle` without a job, as the other vino projects do. What the worker
//! is doing is published in `Status`, so status queries never wait behind it.

use crate::config::Excludes;
use crate::embed::Embedder;
use crate::index::{self, Hit, Outcome, Vectors};
use anyhow::Result;
use futures_channel::oneshot;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The config's exclusions changed: drop what is now excluded.
    Excludes(Excludes),
    /// Index a root (new or existing), as `vinosearch index` does.
    Reindex(PathBuf),
    /// Stop indexing and watching a root, and drop what was indexed under it.
    Forget(PathBuf),
    /// Free the model now instead of waiting for the idle timeout.
    Unload,
}

/// What the worker is doing, for status queries.
#[derive(Default)]
pub struct Status {
    pub loaded: AtomicBool,
    pub indexing: AtomicBool,
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
    /// The config file the exclusions came from, which the watcher re-reads.
    pub config: PathBuf,
    pub status: Arc<Status>,
}

fn changed(
    conn: &mut rusqlite::Connection,
    emb: &mut Embedder,
    paths: Vec<PathBuf>,
    ex: &Excludes,
) -> Result<()> {
    let (mut files, mut chunks, mut removed) = (0, 0, 0);
    for path in paths {
        if ex.contains(&path) {
            continue;
        }
        if path.is_dir() {
            // A new or renamed-in directory: index what is under it.
            for entry in index::walk(&path, ex).filter(|e| e.file_type().is_file()) {
                if let Outcome::Embedded(n) = index::index_file(conn, emb, entry.path())? {
                    files += 1;
                    chunks += n;
                }
            }
        } else if path.exists() {
            match index::index_file(conn, emb, &path)? {
                Outcome::Embedded(n) => {
                    files += 1;
                    chunks += n;
                }
                Outcome::Unchanged => {}
                Outcome::Skipped => {}
            }
        } else {
            removed += index::remove(conn, &path)?;
        }
    }
    if files + removed > 0 {
        eprintln!("updated: {files} files embedded ({chunks} chunks), {removed} removed");
    }
    Ok(())
}

pub fn run(jobs: mpsc::Receiver<Job>, opts: Options) -> Result<()> {
    let mut conn = index::open(&index::default_db()?)?;
    let mut excludes = opts.excludes.clone();
    let purged = index::purge(&conn, &excludes)?;
    if purged > 0 {
        eprintln!("removed {purged} excluded files from the index");
    }
    let load = |conn: &rusqlite::Connection| -> Result<Loaded> {
        let emb = crate::embedder(opts.openvino_dir.clone(), &opts.device)?;
        let vectors = Vectors::load(conn)?;
        eprintln!("serving {} chunks", vectors.len());
        Ok(Loaded { emb, vectors })
    };
    let status = &opts.status;
    let mut loaded: Option<Loaded> = None;
    let unload = |loaded: &mut Option<Loaded>, why: &str| {
        if loaded.take().is_some() {
            status.loaded.store(false, Ordering::Relaxed);
            eprintln!("model unloaded ({why})");
        }
    };
    loop {
        let job = match (opts.idle, loaded.is_some()) {
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
        };
        // Jobs that need no model.
        let job = match job {
            Job::Excludes(ex) => {
                excludes = ex;
                match index::purge(&conn, &excludes) {
                    Ok(n) => eprintln!("exclusions changed, {n} files removed"),
                    Err(e) => eprintln!("applying exclusions: {e:#}"),
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
            job => job,
        };
        if loaded.is_none() {
            match load(&conn) {
                Ok(l) => {
                    loaded = Some(l);
                    status.loaded.store(true, Ordering::Relaxed);
                }
                Err(e) => {
                    eprintln!("loading model: {e:#}");
                    if let Job::Match { reply, .. } = job {
                        let _ = reply.send(Vec::new());
                    }
                    continue;
                }
            }
        }
        let l = loaded.as_mut().unwrap();
        match job {
            Job::Match { query, k, reply } => {
                let hits = (|| -> Result<Vec<Hit>> {
                    l.vectors.refresh(&conn)?;
                    let q = l.emb.query(&query)?;
                    index::search(&conn, &l.vectors, &q, k)
                })();
                let _ = reply.send(hits.unwrap_or_else(|e| {
                    eprintln!("match {query:?}: {e:#}");
                    Vec::new()
                }));
            }
            Job::Changed(paths) => {
                status.indexing.store(true, Ordering::Relaxed);
                if let Err(e) = changed(&mut conn, &mut l.emb, paths, &excludes) {
                    eprintln!("updating index: {e:#}");
                }
                status.indexing.store(false, Ordering::Relaxed);
            }
            Job::Reindex(root) => {
                status.indexing.store(true, Ordering::Relaxed);
                match index::index(&mut conn, &mut l.emb, &root, &excludes) {
                    Ok(s) => eprintln!(
                        "{}: {} files seen, {} embedded ({} chunks), {} removed",
                        root.display(),
                        s.files_seen,
                        s.files_embedded,
                        s.chunks,
                        s.removed
                    ),
                    Err(e) => eprintln!("indexing {}: {e:#}", root.display()),
                }
                status.indexing.store(false, Ordering::Relaxed);
            }
            Job::Excludes(_) | Job::Forget(_) | Job::Unload => {
                unreachable!("handled before loading")
            }
        }
    }
    Ok(())
}
