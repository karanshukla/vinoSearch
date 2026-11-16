//! Reading the index: what a search matches, and what the status, the
//! menu and the settings app ask about the files in it.

use super::vectors::{dot, quantize};
use super::words::WORDS_BOOST;
use super::{Kind, Vectors, Words, from_blob, text_file};
use crate::ml::embed::DIM;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An indexed image's vector (#77), its holder's if it is a byte-identical
/// copy; None for a path that isn't an indexed image.
pub fn image_vector(conn: &Connection, path: &str) -> Result<Option<Vec<f32>>> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT c.embedding FROM files f
             JOIN files g ON g.id = f.id OR (g.sha256 IS NOT NULL AND g.sha256 = f.sha256)
             JOIN chunks c ON c.file_id = g.id
             WHERE f.path = ?1 AND c.kind = ?2 LIMIT 1",
            params![path, Kind::Image as i64],
            |r| r.get(0),
        )
        .optional()?;
    Ok(blob.as_deref().and_then(from_blob))
}

/// Whether `path` is an indexed file.
pub fn is_indexed(conn: &Connection, path: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM files WHERE path = ?1", [path], |_| Ok(()))
        .optional()?
        .is_some())
}

/// The kind `path` is read aloud as, by its name: text, code or a
/// document, never an image. None for anything else.
pub fn readable_kind(path: &Path) -> Option<Kind> {
    if text_file(path) {
        Some(Kind::Text)
    } else if crate::index::code::is_code(path) {
        Some(Kind::Code)
    } else if crate::extract::is_document(path) {
        Some(Kind::Document)
    } else if crate::extract::is_session(path) {
        Some(Kind::Session)
    } else {
        None
    }
}

/// The kind `path` opens as, by its name: one read aloud, or audio or video.
pub fn openable_kind(path: &Path) -> Option<Kind> {
    readable_kind(path).or_else(|| {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        crate::config::MEDIA_EXTENSIONS
            .contains(&ext.as_str())
            .then_some(Kind::Media)
    })
}

/// An indexed audio or video file whose contents hash to `sha256`, for a
/// session's recording (#188). None while media isn't indexed.
pub fn media_with_hash(conn: &Connection, sha256: &[u8]) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT path FROM files WHERE sha256 = ?1 ORDER BY path")?;
    let paths = stmt.query_map([sha256], |r| r.get::<_, String>(0))?;
    for path in paths {
        let path = path?;
        if openable_kind(Path::new(&path)) == Some(Kind::Media) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Indexed text files and documents, newest first by the mtime the index
/// recorded: at most `limit`, less those `exists` says are gone.
pub fn recent_files(
    conn: &Connection,
    limit: usize,
    exists: impl Fn(&Path) -> bool,
) -> Result<Vec<(String, Kind, i64)>> {
    let mut stmt = conn.prepare("SELECT path, mtime FROM files ORDER BY mtime DESC, path")?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while out.len() < limit {
        let Some(row) = rows.next()? else { break };
        let path: String = row.get(0)?;
        let Some(kind) = readable_kind(Path::new(&path)) else {
            continue;
        };
        if exists(Path::new(&path)) {
            out.push((path, kind, row.get(1)?));
        }
    }
    Ok(out)
}

/// Files and chunks in the index.
pub fn counts(conn: &Connection) -> Result<(i64, i64)> {
    Ok((
        conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?,
        conn.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?,
    ))
}

/// (files, chunks) of each kind in the index, in `Kind::ALL`'s order (#73).
/// A file counts under the kind of its chunks, an image's text (#106) under
/// images, so one recorded without any (a photo with no decoder) counts
/// under none.
pub fn counts_by_kind(conn: &Connection) -> Result<[(i64, i64); 6]> {
    let mut out = [(0, 0); 6];
    let mut stmt = conn.prepare(
        "SELECT CASE WHEN EXISTS (SELECT 1 FROM chunks i WHERE i.file_id = c.file_id AND i.kind = ?1)
                THEN ?1 ELSE c.kind END AS k,
                count(DISTINCT c.file_id), count(*)
         FROM chunks c GROUP BY k",
    )?;
    let rows = stmt.query_map([Kind::Image as i64], |r| {
        Ok((
            Kind::from_db(r.get(0)?),
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (kind, files, chunks) = row?;
        let i = Kind::ALL
            .iter()
            .position(|k| *k == kind)
            .expect("every kind is listed");
        out[i] = (files, chunks);
    }
    Ok(out)
}

/// vinoWhisper sessions indexed with no chunks (#143): in a format this
/// build doesn't read, or not readable at all.
pub fn unread_sessions(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM files f
         WHERE f.path LIKE '%.vwsession'
           AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = f.id)
           -- A copy keeps no chunks of its own: the file it copies holds them.
           AND NOT EXISTS (
               SELECT 1 FROM files g JOIN chunks c ON c.file_id = g.id
               WHERE g.sha256 = f.sha256 AND g.id != f.id
           )",
        [],
        |r| r.get(0),
    )?)
}

/// Roots `index` has been run on, which the watcher follows.
pub fn roots(conn: &Connection) -> Result<Vec<PathBuf>> {
    let mut stmt = conn.prepare("SELECT path FROM roots ORDER BY path")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(Result::ok).map(PathBuf::from).collect())
}

/// The folders holding most of the index, biggest first, for the settings
/// window to offer excluding: at most `limit`, none inside another, never a
/// root or above one. A folder gives way to a subfolder holding 3/4 of its
/// chunks, so a copied source tree shows as the part of it that is the bulk
/// (on ~/Development, two copies of one driver tree were 55% of the index).
pub fn largest_folders(conn: &Connection, limit: usize) -> Result<Vec<(PathBuf, u64)>> {
    let roots = roots(conn)?;
    let mut chunks: HashMap<PathBuf, u64> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT f.path, count(*) FROM chunks c JOIN files f ON f.id = c.file_id GROUP BY c.file_id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (path, n) = row?;
        for dir in Path::new(&path).ancestors().skip(1) {
            if roots.iter().any(|r| r == dir) || !roots.iter().any(|r| dir.starts_with(r)) {
                break;
            }
            *chunks.entry(dir.to_owned()).or_default() += n as u64;
        }
    }
    Ok(biggest(&chunks, limit))
}

/// The heaviest folders in `weights` (each folder's total, its subfolders'
/// included), biggest first: at most `limit`, none inside another. A folder
/// gives way to a subfolder holding 3/4 of its weight, so what's named is
/// where the bulk actually is.
pub fn biggest(weights: &HashMap<PathBuf, u64>, limit: usize) -> Vec<(PathBuf, u64)> {
    let mut biggest_child: HashMap<&Path, u64> = HashMap::new();
    for (dir, n) in weights {
        if let Some(parent) = dir.parent() {
            let m = biggest_child.entry(parent).or_default();
            *m = (*m).max(*n);
        }
    }
    let mut dirs: Vec<(&PathBuf, u64)> = weights.iter().map(|(d, n)| (d, *n)).collect();
    // Of equal folders, the deeper says more.
    dirs.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.0.components().count().cmp(&a.0.components().count()))
            .then(a.0.cmp(b.0))
    });
    let mut picked: Vec<(PathBuf, u64)> = Vec::new();
    for (dir, n) in dirs {
        if picked.len() == limit {
            break;
        }
        let gives_way = biggest_child
            .get(dir.as_path())
            .is_some_and(|m| m * 4 >= n * 3);
        let overlaps = picked
            .iter()
            .any(|(p, _)| p.starts_with(dir) || dir.starts_with(p));
        if !gives_way && !overlaps {
            picked.push((dir.clone(), n));
        }
    }
    picked
}

pub struct Hit {
    pub kind: Kind,
    pub score: f32,
    pub path: String,
    pub line: i64,
    pub text: String,
    /// Other indexed files with the same content.
    pub copies: Vec<String>,
    /// An image's own vector, for telling near-duplicates apart (#78).
    pub vector: Option<Vec<f32>>,
    /// Near-duplicate images folded into this one (`dupes::collapse`).
    pub similar: Vec<String>,
}

/// What `query` searches: code alone, for a query starting `code:` (#110),
/// or everything but code, text first; with the query to embed and look up,
/// less its `code:`.
pub fn scope(query: &str) -> (&str, &'static [Kind]) {
    match crate::index::code::code_query(query) {
        Some(q) => (q, &[Kind::Code]),
        None => (
            query,
            &[
                Kind::Text,
                Kind::Document,
                Kind::Image,
                Kind::Session,
                Kind::Media,
            ],
        ),
    }
}

/// Cosine against every chunk of `kind` (embeddings are unit length), best
/// `k`. With `words`, chunks holding them score higher too (#104): by
/// WORDS_BOOST, or for an identifier ahead of everything, best match first.
/// Scores stay on the cosine's scale, so each kind's `min_score` still means
/// what it did.
pub fn search(
    conn: &Connection,
    vectors: &Vectors,
    query: &[f32],
    words: Option<&Words>,
    kind: Kind,
    k: usize,
) -> Result<Vec<Hit>> {
    let query: &[f32; DIM] = query.try_into().context("query is not DIM long")?;
    let (q, qs) = quantize(query);
    let score =
        |p: usize| dot(&vectors.data.as_chunks::<DIM>().0[p], &q) as f32 * vectors.scales[p] * qs;
    let mut scored: Vec<(f32, i64)> = vectors
        .data
        .as_chunks::<DIM>()
        .0
        .iter()
        .zip(&vectors.scales)
        .zip(&vectors.ids)
        .zip(&vectors.kinds)
        .filter(|(_, k)| **k == kind)
        .map(|(((v, s), id), _)| (dot(v, &q) as f32 * s * qs, *id))
        .collect();
    let by_score = |a: &(f32, i64), b: &(f32, i64)| b.0.total_cmp(&a.0);
    if scored.len() > k && k > 0 {
        scored.select_nth_unstable_by(k - 1, by_score);
        scored.truncate(k);
    }
    if let Some(words) = words {
        // Not loaded yet: it shows once the vectors catch up.
        let of_kind = words
            .ids
            .iter()
            .filter_map(|id| Some((id, *vectors.pos.get(id)?)))
            .filter(|(_, p)| vectors.kinds[*p] == kind)
            .take(k);
        for (rank, (id, p)) in of_kind.enumerate() {
            let s = if words.exact {
                // Just under a perfect cosine, in bm25's order.
                1.0 - 0.001 * (rank + 1) as f32
            } else {
                (score(p) + WORDS_BOOST).min(1.0)
            };
            match scored.iter_mut().find(|(_, i)| i == id) {
                Some(hit) => hit.0 = hit.0.max(s),
                None => scored.push((s, *id)),
            }
        }
    }
    scored.sort_by(by_score);
    scored.truncate(k);
    let mut stmt = conn.prepare(
        "SELECT f.id, f.path, f.sha256, c.start_line, c.text,
                CASE WHEN c.kind = ?2 THEN c.embedding END
         FROM chunks c JOIN files f ON f.id = c.file_id WHERE c.id = ?1",
    )?;
    let mut copies =
        conn.prepare("SELECT path FROM files WHERE sha256 = ?1 AND id != ?2 ORDER BY path")?;
    let mut hits = Vec::with_capacity(scored.len());
    for (score, id) in scored {
        // Gone since the vectors were loaded.
        let Ok((file, path, hash, line, text, vector)) =
            stmt.query_row(params![id, Kind::Image as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get::<_, Option<Vec<u8>>>(5)?,
                ))
            })
        else {
            continue;
        };
        let copies = match hash {
            Some(h) => copies
                .query_map(params![h, file], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?,
            None => Vec::new(),
        };
        hits.push(Hit {
            kind,
            score,
            path,
            line,
            text,
            copies,
            vector: vector.as_deref().and_then(from_blob),
            similar: Vec::new(),
        });
    }
    Ok(hits)
}
