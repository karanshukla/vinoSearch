//! Audio and video files (#187): recorded when a check finds them, then
//! embedded from `index_rest` once everything else is in, SLICE seconds at a
//! time from the second the last slice ended, so a film interrupted halfway
//! carries on from there (#192). Their sound in 10 s windows (quiet ones
//! skipped), their picture a frame per shot. Nothing is read whole: the hash
//! streams, and ffmpeg decodes through pipes.

use super::{Kind, Outcome, Rest};
use crate::extract::media::{self, FRAME_LINES};
use crate::ml::embed::Embed;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Seconds of media a call to `index_rest` embeds: 30 sound windows and a
/// few frames, under a minute of the NPU's time. A slice starts its frames
/// afresh, so longer slices waste fewer.
pub const SLICE: u32 = 300;

/// The file's SHA-256, read a MiB at a time.
pub fn hash(path: &Path) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_vec())
}

/// Record a new or changed media file for `index_rest`: its row, no chunks
/// yet, and a partial row of its duration in seconds. A file over the
/// longest allowed, or that ffprobe can't read, keeps a row with no chunks,
/// so it isn't probed again until it changes.
pub fn start(
    conn: &mut Connection,
    path: &Path,
    row: (&str, i64, i64, &[u8]),
    max_seconds: f64,
) -> Result<Outcome> {
    let (key, mtime, size, hash) = row;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
    tx.execute(
        "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, ?4)",
        params![key, mtime, size, hash],
    )?;
    let id = tx.last_insert_rowid();
    let probe = match media::probe(path) {
        Ok(p) if p.seconds > max_seconds => {
            eprintln!(
                "skip {}: {} long, over the longest indexed ({})",
                path.display(),
                media::stamp(p.seconds as i64),
                media::stamp(max_seconds as i64)
            );
            None
        }
        Ok(p) if !p.audio && !p.video => {
            eprintln!("skip {}: no sound or picture", path.display());
            None
        }
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("skip {}: {e:#}", path.display());
            None
        }
    };
    let Some(probe) = probe else {
        tx.commit()?;
        return Ok(Outcome::Unreadable { no_decoder: false });
    };
    tx.execute(
        "INSERT INTO partial (file_id, done, total, media) VALUES (?1, 0, ?2, 1)",
        params![id, probe.seconds.ceil() as i64],
    )?;
    tx.commit()?;
    Ok(Outcome::Embedded(0))
}

/// Embed seconds `from..from + SLICE` of file `id`, and move its partial
/// row on. A track that won't decode is left out of the slice; when neither
/// will, the file stops where it is.
pub fn rest<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    id: i64,
    key: String,
    from: usize,
    total: usize,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Rest> {
    let path = Path::new(&key);
    let from = u32::try_from(from).context("start second")?;
    let end = from.saturating_add(SLICE).min(total as u32);
    let stopped = |conn: &Connection, why: anyhow::Error| -> Result<Rest> {
        eprintln!(
            "{}: stopped at {}: {why:#}",
            path.display(),
            media::stamp(i64::from(from))
        );
        conn.execute("DELETE FROM partial WHERE file_id = ?1", [id])?;
        Ok(Rest::Parts {
            path: key.clone(),
            done: from as usize,
            total,
            chunks: 0,
        })
    };
    let probe = match media::probe(path) {
        Ok(p) => p,
        Err(e) => return stopped(conn, e),
    };
    let mut chunks = Vec::new();
    let mut vectors = Vec::new();
    // Each track's decoding error, if it had one. Errors from embedding or
    // from `between` (a stop, a query's turn) end the call instead.
    let mut failed = Vec::new();
    if probe.audio {
        match media::audio(path, from, &probe) {
            Err(e) => failed.push(e.context("sound")),
            Ok(windows) => {
                for w in windows {
                    let w = match w.and_then(|w| Ok((crate::ml::audio::log_mel(&w.samples)?, w))) {
                        Ok(w) => w,
                        Err(e) => {
                            failed.push(e.context("sound"));
                            break;
                        }
                    };
                    let (features, w) = w;
                    if w.start >= end {
                        break;
                    }
                    if w.quiet() {
                        continue;
                    }
                    vectors.push(emb.audio(&features)?);
                    chunks.push((w.start as usize, String::new()));
                    between(conn, emb)?;
                }
            }
        }
    }
    if probe.video {
        match media::frames(path, from, &probe) {
            Err(e) => failed.push(e.context("picture")),
            Ok(frames) => {
                for f in frames {
                    let f = match f
                        .and_then(|f| Ok((crate::ml::vision::prepare(&f.image)?, f.second)))
                    {
                        Ok(f) => f,
                        Err(e) => {
                            failed.push(e.context("picture"));
                            break;
                        }
                    };
                    let (pixels, second) = f;
                    if second >= f64::from(end) {
                        break;
                    }
                    vectors.push(emb.image(&pixels)?);
                    // Up, so a seek to it lands in the shot, not the one
                    // before a cut a fraction of a second earlier.
                    chunks.push((FRAME_LINES + second.ceil() as usize, String::new()));
                    between(conn, emb)?;
                }
            }
        }
    }
    let tracks = usize::from(probe.audio) + usize::from(probe.video);
    if failed.len() == tracks {
        return stopped(conn, failed.remove(0));
    }
    for e in failed {
        eprintln!(
            "{} at {}: {e:#}",
            path.display(),
            media::stamp(i64::from(from))
        );
    }
    super::store_rest(
        conn,
        id,
        Kind::Media,
        end as usize,
        total,
        &chunks,
        &vectors,
    )?;
    Ok(Rest::Parts {
        path: key,
        done: end as usize,
        total,
        chunks: chunks.len(),
    })
}
