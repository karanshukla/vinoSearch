//! Audio and video files: recorded when a check finds them, then embedded
//! from `index_rest` once everything else is in, SLICE seconds at a time from
//! the second the last slice ended, so a film interrupted halfway carries on
//! from there. Their sound in 10 s windows (quiet ones
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
pub fn probe(path: &Path, max_seconds: Option<f64>) -> Probed {
    match media::probe(path) {
        Ok(p) => {
            if let Some(max) = max_seconds
                && p.seconds > max
            {
                eprintln!(
                    "skip {}: {} long, over the longest indexed ({})",
                    path.display(),
                    media::stamp(p.seconds as i64),
                    media::stamp(max as i64)
                );
                return Probed::TooLong(p.seconds);
            }
            if !p.audio && !p.video {
                eprintln!("skip {}: no sound or picture", path.display());
                Probed::Unreadable
            } else {
                Probed::Ok(p)
            }
        }
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
pub fn left_out(conn: &Connection, id: i64, max_seconds: Option<f64>) -> Result<bool> {
    let Some(max_seconds) = max_seconds else {
        return Ok(false);
    };
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

/// How many files `fit` moved.
#[derive(Debug, PartialEq, Eq)]
pub struct Fitted {
    pub left_out: usize,
    pub taken_up: usize,
}

/// An mtime no file has: a file left out before it was hashed gets it when
/// taken up, so `index_rest` indexes it afresh, hash and all.
const NO_STAMP: i64 = -1;

struct Waiting {
    id: i64,
    path: String,
    seconds: f64,
    hash: Option<Vec<u8>>,
}

/// Hold the index to `max_seconds` of media per file after the setting
/// changed: files now over it lose their chunks and wait, and files that
/// waited and now fit (or all of them when `max_seconds` is `None`) are
/// queued for `index_rest`. Nothing is probed or hashed for it.
pub fn fit(conn: &Connection, max_seconds: Option<f64>) -> Result<Fitted> {
    let tx = conn.unchecked_transaction()?;
    let read_waiting = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Waiting> {
        Ok(Waiting {
            id: r.get(0)?,
            path: r.get(1)?,
            seconds: r.get(2)?,
            hash: r.get(3)?,
        })
    };
    let (out, fits) = match max_seconds {
        Some(max_seconds) => {
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
            let fits: Vec<Waiting> = tx
                .prepare(
                    "SELECT f.id, f.path, m.seconds, f.sha256 FROM media m JOIN files f ON f.id = m.file_id
                     WHERE m.too_long = 1 AND m.seconds <= ?1 ORDER BY f.id",
                )?
                .query_map([max_seconds], read_waiting)?
                .collect::<rusqlite::Result<_>>()?;
            (out, fits)
        }
        None => {
            let fits: Vec<Waiting> = tx
                .prepare(
                    "SELECT f.id, f.path, m.seconds, f.sha256 FROM media m JOIN files f ON f.id = m.file_id
                     WHERE m.too_long = 1 ORDER BY f.id",
                )?
                .query_map([], read_waiting)?
                .collect::<rusqlite::Result<_>>()?;
            (0, fits)
        }
    };
    for Waiting {
        id,
        path,
        seconds,
        hash,
    } in &fits
    {
        tx.execute("UPDATE media SET too_long = 0 WHERE file_id = ?1", [id])?;
        let copy_of_one_taken_up = match hash {
            Some(hash) => super::holder_of(&tx, hash, path)?.is_some(),
            None => false,
        };
        if copy_of_one_taken_up {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO partial (file_id, done, total, media) VALUES (?1, 0, ?2, 1)",
            params![id, seconds.ceil() as i64],
        )?;
        if hash.is_none() {
            tx.execute("UPDATE files SET mtime = ?2 WHERE id = ?1", [*id, NO_STAMP])?;
        }
    }
    tx.commit()?;
    if out + fits.len() > 0 {
        match max_seconds {
            Some(max_seconds) => eprintln!(
                "longest media indexed is now {}: {out} files left out, {} taken up",
                media::stamp(max_seconds as i64),
                fits.len()
            ),
            None => eprintln!(
                "media duration limit removed: {} files taken up",
                fits.len()
            ),
        }
    }
    Ok(Fitted {
        left_out: out,
        taken_up: fits.len(),
    })
}

/// The seconds of file `id` a call to `rest` embeds.
struct SliceAt<'a> {
    id: i64,
    path: &'a Path,
    from: u32,
    end: u32,
    probe: &'a Probe,
}

/// What a slice embedded: `(line, text)` chunks and their vectors.
#[derive(Default)]
struct Embedded {
    chunks: Vec<(usize, String)>,
    vectors: Vec<Vec<f32>>,
}

/// Embed seconds `from..from + SLICE` of file `id`, and move its partial
/// row on. A track that won't decode is left out of the slice; when neither
/// will, or the file has neither now (replaced since it was recorded, with
/// the same stamp, by subtitles or data only), the file stops where it is.
/// An error from the model ends the call (`put_back`), as one from `between`
/// (a stop, a query's turn) does.
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
    let slice = SliceAt {
        id,
        path,
        from,
        end,
        probe: &probe,
    };
    let mut out = Embedded::default();
    let mut failed = Vec::new();
    if probe.audio {
        failed.extend(embed_sound(conn, emb, &slice, &mut out, between)?);
    }
    if probe.video {
        failed.extend(embed_picture(conn, emb, &slice, &mut out, between)?);
    }
    let tracks = usize::from(probe.audio) + usize::from(probe.video);
    if tracks == 0 {
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
        &out.chunks,
        &out.vectors,
    )?;
    Ok(Rest::Parts {
        path: key,
        done: end as usize,
        total,
        chunks: out.chunks.len(),
    })
}

/// Embeds the slice's sound into `out`, quiet windows skipped: the track's
/// decoding error if it has one. Samples `log_mel` refuses (NaN in a float
/// file) are a track that won't decode, not the model failing.
fn embed_sound<E: Embed>(
    conn: &Connection,
    emb: &mut E,
    s: &SliceAt,
    out: &mut Embedded,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Option<anyhow::Error>> {
    let windows = match media::audio(s.path, s.from, s.probe) {
        Ok(w) => w,
        Err(e) => return Ok(Some(e.context("sound"))),
    };
    for w in windows {
        let (features, w) = match w.and_then(|w| Ok((crate::ml::audio::log_mel(&w.samples)?, w))) {
            Ok(w) => w,
            Err(e) => return Ok(Some(e.context("sound"))),
        };
        if w.start >= s.end {
            break;
        }
        if w.quiet() {
            continue;
        }
        let v = emb.audio(&features).map_err(|e| put_back(conn, s, e))?;
        out.vectors.push(v);
        out.chunks.push((w.start as usize, String::new()));
        between(conn, emb)?;
    }
    Ok(None)
}

/// Embeds the slice's frames into `out`: the track's decoding error if it
/// has one.
fn embed_picture<E: Embed>(
    conn: &Connection,
    emb: &mut E,
    s: &SliceAt,
    out: &mut Embedded,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Option<anyhow::Error>> {
    let frames = match media::frames(s.path, s.from, s.probe) {
        Ok(f) => f,
        Err(e) => return Ok(Some(e.context("picture"))),
    };
    for f in frames {
        let (pixels, second) =
            match f.and_then(|f| Ok((crate::ml::vision::prepare(&f.image)?, f.second))) {
                Ok(f) => f,
                Err(e) => return Ok(Some(e.context("picture"))),
            };
        if second >= f64::from(s.end) {
            break;
        }
        let v = emb.image(&pixels).map_err(|e| put_back(conn, s, e))?;
        out.vectors.push(v);
        out.chunks.push((frame_line(second), String::new()));
        between(conn, emb)?;
    }
    Ok(None)
}

/// The line a frame at `second` is stored under: rounded up, so a seek to it
/// lands in the shot, not the one before a cut a fraction of a second earlier
/// (pinned by `tests::a_frame_between_seconds_is_stored_at_the_next_one`,
/// `tests::a_frame_on_a_whole_second_is_stored_at_that_second`).
fn frame_line(second: f64) -> usize {
    FRAME_LINES + second.ceil() as usize
}

/// The model's error on the slice: the file goes behind the others waiting,
/// so one file the model can't take doesn't hold up the rest.
fn put_back(conn: &Connection, s: &SliceAt, why: anyhow::Error) -> anyhow::Error {
    match conn.execute(
        "UPDATE partial SET tries = tries + 1 WHERE file_id = ?1",
        [s.id],
    ) {
        Ok(_) => why.context(format!("embedding {}", s.path.display())),
        Err(e) => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    /// A sparse file of `len` zeros, taking no disk, named for `tag`.
    fn sparse_zeros(tag: &str, len: u64) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("vinosearch-hash-{tag}-{}", std::process::id()));
        std::fs::File::create(&path).unwrap().set_len(len).unwrap();
        path
    }

    #[test]
    fn hashing_a_big_file_lets_queries_in() {
        let path = sparse_zeros("turns", 40 << 20);
        let mut turns = 0;
        let whole = hash(&path, &mut || {
            turns += 1;
            Ok(())
        })
        .unwrap()
        .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(turns, 2);
        let zeros = Sha256::digest(vec![0u8; 40 << 20]).to_vec();
        assert_eq!(whole, zeros);
    }

    #[test]
    fn hashing_stops_when_told() {
        let path = sparse_zeros("stop", 40 << 20);
        let stopped = hash(&path, &mut || bail!("stop"));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(stopped.unwrap_err().to_string(), "stop");
    }

    #[test]
    fn hashing_a_gone_file_is_none_not_an_error() {
        let path = sparse_zeros("gone", 0);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(hash(&path, &mut || Ok(())).unwrap(), None);
    }

    #[test]
    fn a_frame_between_seconds_is_stored_at_the_next_one() {
        assert_eq!(frame_line(12.25), FRAME_LINES + 13);
    }

    #[test]
    fn a_frame_on_a_whole_second_is_stored_at_that_second() {
        assert_eq!(frame_line(12.0), FRAME_LINES + 12);
    }
}
