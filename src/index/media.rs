//! Audio and video files (#187): recorded when a check finds them, then
//! embedded from `index_rest` once everything else is in, SLICE seconds at a
//! time from the second the last slice ended, so a film interrupted halfway
//! carries on from there (#192). Their sound in 10 s windows (quiet ones
//! skipped), their picture a frame per shot. Nothing is read whole: the hash
//! streams, and ffmpeg decodes through pipes.

use super::{Kind, Outcome, Rest};
use crate::extract::media::{self, FRAME_LINES, Probe};
use crate::ml::embed::Embed;
use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Seconds of media a call to `index_rest` embeds: 30 sound windows and a
/// few frames, under a minute of the NPU's time. A slice starts its frames
/// afresh, so longer slices waste fewer.
pub const SLICE: u32 = 300;

/// A file is hashed this much at a time, with `between` after each: tens of
/// milliseconds of reading, so a query waits no longer than behind a chunk.
const HASH_RUN: usize = 16 << 20;

/// The file's SHA-256, read a MiB at a time, or None if it can't be read.
/// `between` runs every HASH_RUN, and an error from it ends the hashing.
pub fn hash(path: &Path, between: &mut dyn FnMut() -> Result<()>) -> Result<Option<Vec<u8>>> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Ok(None);
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut run = 0;
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(None),
        };
        hasher.update(&buf[..n]);
        run += n;
        if run >= HASH_RUN {
            run = 0;
            between()?;
        }
    }
    Ok(Some(hasher.finalize().to_vec()))
}

/// What ffprobe makes of a file, against the longest indexed.
pub enum Probed {
    /// Sound, picture or both, and not too long.
    Ok(Probe),
    /// Over the longest indexed, by its seconds.
    TooLong(f64),
    /// ffprobe can't read it, or it has neither sound nor picture.
    Unreadable,
}

/// Probe `path`, saying why on stderr when it won't be indexed. Up to 30 s
/// of ffprobe, so never inside a transaction.
pub fn probe(path: &Path, max_seconds: f64) -> Probed {
    match media::probe(path) {
        Ok(p) if p.seconds > max_seconds => {
            eprintln!(
                "skip {}: {} long, over the longest indexed ({})",
                path.display(),
                media::stamp(p.seconds as i64),
                media::stamp(max_seconds as i64)
            );
            Probed::TooLong(p.seconds)
        }
        Ok(p) if !p.audio && !p.video => {
            eprintln!("skip {}: no sound or picture", path.display());
            Probed::Unreadable
        }
        Ok(p) => Probed::Ok(p),
        Err(e) => {
            eprintln!("skip {}: {e:#}", path.display());
            Probed::Unreadable
        }
    }
}

/// Record a file over the longest indexed: its row and its length, no
/// chunks, and no hash, so it is never read whole. `fit` takes it up if the
/// limit is raised past it.
pub fn leave_out(conn: &mut Connection, row: (&str, i64, i64), seconds: f64) -> Result<()> {
    let (key, mtime, size) = row;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
    tx.execute(
        "INSERT INTO files (path, mtime, size) VALUES (?1, ?2, ?3)",
        params![key, mtime, size],
    )?;
    tx.execute(
        "INSERT INTO media (file_id, seconds, too_long) VALUES (?1, ?2, 1)",
        params![tx.last_insert_rowid(), seconds],
    )?;
    Ok(tx.commit()?)
}

/// File `id` was left out as too long, and at `max_seconds` still is.
pub fn left_out(conn: &Connection, id: i64, max_seconds: f64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM media WHERE file_id = ?1 AND too_long = 1 AND seconds > ?2",
            params![id, max_seconds],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Record how long the file at `key` runs: a copy's, which is never
/// started itself.
pub fn note(conn: &Connection, key: &str, probe: &Probe) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO media (file_id, seconds)
         SELECT id, ?2 FROM files WHERE path = ?1",
        params![key, probe.seconds],
    )?;
    Ok(())
}

/// Record a new or changed media file for `index_rest`: its row, no chunks
/// yet, how long it runs, and a partial row of that in seconds. A file
/// ffprobe couldn't read keeps a row with no chunks, so it isn't probed
/// again until it changes.
pub fn start(
    conn: &mut Connection,
    row: (&str, i64, i64, &[u8]),
    probed: Option<Probe>,
) -> Result<Outcome> {
    let (key, mtime, size, hash) = row;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
    tx.execute(
        "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, ?4)",
        params![key, mtime, size, hash],
    )?;
    let id = tx.last_insert_rowid();
    let Some(probe) = probed else {
        tx.commit()?;
        return Ok(Outcome::Unreadable { no_decoder: false });
    };
    tx.execute(
        "INSERT INTO media (file_id, seconds) VALUES (?1, ?2)",
        params![id, probe.seconds],
    )?;
    tx.execute(
        "INSERT INTO partial (file_id, done, total, media) VALUES (?1, 0, ?2, 1)",
        params![id, probe.seconds.ceil() as i64],
    )?;
    tx.commit()?;
    Ok(Outcome::Embedded(0))
}

/// Hold the index to `max_seconds` of media per file after the setting
/// changed: files now over it lose their chunks and wait, and files that
/// waited and now fit are queued for `index_rest`. Nothing is probed or
/// hashed for it. Returns (files left out, files taken up).
pub fn fit(conn: &Connection, max_seconds: f64) -> Result<(usize, usize)> {
    let tx = conn.unchecked_transaction()?;
    let over = "SELECT file_id FROM media WHERE too_long = 0 AND seconds > ?1";
    tx.execute(
        &format!("DELETE FROM chunks WHERE file_id IN ({over})"),
        [max_seconds],
    )?;
    tx.execute(
        &format!("DELETE FROM partial WHERE file_id IN ({over})"),
        [max_seconds],
    )?;
    let out = tx.execute(
        "UPDATE media SET too_long = 1 WHERE too_long = 0 AND seconds > ?1",
        [max_seconds],
    )?;
    let fits: Vec<(i64, String, f64, Option<Vec<u8>>)> = tx
        .prepare(
            "SELECT f.id, f.path, m.seconds, f.sha256 FROM media m JOIN files f ON f.id = m.file_id
             WHERE m.too_long = 1 AND m.seconds <= ?1 ORDER BY f.id",
        )?
        .query_map([max_seconds], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, path, seconds, hash) in &fits {
        tx.execute("UPDATE media SET too_long = 0 WHERE file_id = ?1", [id])?;
        // A copy of one taken up before it shares that one's chunks.
        if let Some(hash) = hash
            && super::holder_of(&tx, hash, path)?.is_some()
        {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO partial (file_id, done, total, media) VALUES (?1, 0, ?2, 1)",
            params![id, seconds.ceil() as i64],
        )?;
        // Left out before it was hashed: a stamp that can't match has
        // `index_rest` index it afresh, hash and all, when its turn comes.
        if hash.is_none() {
            tx.execute("UPDATE files SET mtime = -1 WHERE id = ?1", [id])?;
        }
    }
    tx.commit()?;
    if out + fits.len() > 0 {
        eprintln!(
            "longest media indexed is now {}: {out} files left out, {} taken up",
            media::stamp(max_seconds as i64),
            fits.len()
        );
    }
    Ok((out, fits.len()))
}

/// Embed seconds `from..from + SLICE` of file `id`, and move its partial
/// row on. A track that won't decode is left out of the slice; when neither
/// will, the file stops where it is. An error from the model ends the call
/// with the file put behind the others waiting, so one file the model
/// can't take doesn't hold up the rest.
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
    // The model's error on this file, which goes to the back of the queue.
    let put_back = |conn: &Connection, why: anyhow::Error| -> anyhow::Error {
        match conn.execute(
            "UPDATE partial SET tries = tries + 1 WHERE file_id = ?1",
            [id],
        ) {
            Ok(_) => why.context(format!("embedding {}", path.display())),
            Err(e) => e.into(),
        }
    };
    let probe = match media::probe(path) {
        Ok(p) => p,
        Err(e) => return stopped(conn, e),
    };
    let mut chunks = Vec::new();
    let mut vectors = Vec::new();
    // Each track's decoding error, if it had one. Errors from `between` (a
    // stop, a query's turn) end the call instead, as the model's do.
    let mut failed = Vec::new();
    if probe.audio {
        match media::audio(path, from, &probe) {
            Err(e) => failed.push(e.context("sound")),
            Ok(windows) => {
                for w in windows {
                    // Samples `log_mel` refuses (NaN in a float file) are a
                    // track that won't decode, not the model failing.
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
                    match emb.audio(&features) {
                        Ok(v) => vectors.push(v),
                        Err(e) => return Err(put_back(conn, e)),
                    }
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
                    match emb.image(&pixels) {
                        Ok(v) => vectors.push(v),
                        Err(e) => return Err(put_back(conn, e)),
                    }
                    // Up, so a seek to it lands in the shot, not the one
                    // before a cut a fraction of a second earlier.
                    chunks.push((FRAME_LINES + second.ceil() as usize, String::new()));
                    between(conn, emb)?;
                }
            }
        }
    }
    let tracks = usize::from(probe.audio) + usize::from(probe.video);
    if tracks == 0 {
        // It had one when it was recorded: replaced since, with the same
        // stamp, by something with subtitles or data only.
        return stopped(conn, anyhow!("no sound or picture"));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    #[test]
    fn hashing_a_big_file_lets_queries_in_and_stops_when_told() {
        let path = std::env::temp_dir().join(format!("vinosearch-hash-{}", std::process::id()));
        // Sparse: 40 MiB of zeros that take no disk.
        std::fs::File::create(&path)
            .unwrap()
            .set_len(40 << 20)
            .unwrap();
        let mut turns = 0;
        let whole = hash(&path, &mut || {
            turns += 1;
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(turns, 2);
        let zeros = Sha256::digest(vec![0u8; 40 << 20]).to_vec();
        assert_eq!(whole, zeros);

        let stopped = hash(&path, &mut || bail!("stop"));
        assert_eq!(stopped.unwrap_err().to_string(), "stop");
        std::fs::remove_file(&path).unwrap();
        // Gone: unreadable, which is no error.
        assert_eq!(hash(&path, &mut || Ok(())).unwrap(), None);
    }
}
