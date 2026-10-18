//! Files -> chunks -> embeddings in SQLite, and brute-force search over them.
//!
//! Plaintext on disk for now. Embeddings can be partly inverted back to their
//! text, so this needs encrypting at rest (TPM-sealed key) before it holds
//! anything a user would not leave readable.

use crate::embed::{DIM, Embedder, document_text};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};
use walkdir::WalkDir;

/// About 250 tokens of English, inside the 256-token document shape.
const CHUNK_CHARS: usize = 900;
const MAX_FILE_BYTES: u64 = 1 << 20;
const EXTENSIONS: &[&str] = &[
    "md", "txt", "rst", "org", "tex", "rs", "py", "js", "ts", "tsx", "jsx", "go", "c", "h", "cpp",
    "hpp", "java", "kt", "swift", "rb", "sh", "bash", "zsh", "fish", "toml", "yaml", "yml", "json",
    "ini", "conf", "cfg", "html", "css", "scss", "sql", "lua", "zig", "nix", "cs", "php",
];
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    "venv",
];

pub fn default_db() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".local/share"),
    };
    Ok(base.join("vinosearch/index.sqlite"))
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS files (
             id INTEGER PRIMARY KEY,
             path TEXT NOT NULL UNIQUE,
             mtime INTEGER NOT NULL,
             size INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS chunks (
             id INTEGER PRIMARY KEY,
             file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
             start_line INTEGER NOT NULL,
             text TEXT NOT NULL,
             embedding BLOB NOT NULL
         );
         CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);",
    )?;
    Ok(conn)
}

fn wanted(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

fn skipped_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

/// Line-aligned chunks of up to CHUNK_CHARS, with the line each starts on.
/// A single longer line is split on char boundaries.
pub fn chunk(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 1;
    for (i, line) in text.lines().enumerate() {
        if !cur.is_empty() && cur.len() + line.len() + 1 > CHUNK_CHARS {
            out.push((start, std::mem::take(&mut cur)));
        }
        if cur.is_empty() {
            start = i + 1;
        }
        let mut rest = line;
        while rest.len() > CHUNK_CHARS {
            let mut cut = CHUNK_CHARS;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push((i + 1, rest[..cut].to_string()));
            rest = &rest[cut..];
        }
        cur.push_str(rest);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push((start, cur));
    }
    out.retain(|(_, c)| !c.trim().is_empty());
    out
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Option<Vec<f32>> {
    (b.len() == DIM * 4).then(|| {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect()
    })
}

pub struct Stats {
    pub files_seen: usize,
    pub files_embedded: usize,
    pub chunks: usize,
    pub removed: usize,
}

/// Index everything wanted under `root`. Unchanged files (same mtime and size)
/// are skipped; files under `root` that are gone are removed.
pub fn index(conn: &mut Connection, emb: &mut Embedder, root: &Path) -> Result<Stats> {
    let root = fs::canonicalize(root).with_context(|| root.display().to_string())?;
    let mut stats = Stats {
        files_seen: 0,
        files_embedded: 0,
        chunks: 0,
        removed: 0,
    };
    let mut seen = HashSet::new();
    let t0 = Instant::now();
    let walker = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !(e.file_type().is_dir() && skipped_dir(&e.file_name().to_string_lossy()))
        });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skip: {e}");
                continue;
            }
        };
        if !entry.file_type().is_file() || !wanted(entry.path()) {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Some(path) = entry.path().to_str().map(str::to_owned) else {
            continue;
        };
        stats.files_seen += 1;
        seen.insert(path.clone());
        let mtime = meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let size = meta.len() as i64;
        let known: Option<(i64, i64)> = conn
            .query_row(
                "SELECT mtime, size FROM files WHERE path = ?1",
                [&path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if known == Some((mtime, size)) {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let chunks = chunk(&text);
        let title = entry.file_name().to_string_lossy();
        let inputs: Vec<String> = chunks
            .iter()
            .map(|(_, c)| document_text(&title, c))
            .collect();
        let vectors = emb.documents(&inputs)?;

        let tx = conn.transaction()?;
        tx.execute("DELETE FROM files WHERE path = ?1", [&path])?;
        tx.execute(
            "INSERT INTO files (path, mtime, size) VALUES (?1, ?2, ?3)",
            params![path, mtime, size],
        )?;
        let file_id = tx.last_insert_rowid();
        for ((line, text), v) in chunks.iter().zip(&vectors) {
            tx.execute(
                "INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (?1, ?2, ?3, ?4)",
                params![file_id, *line as i64, text, to_blob(v)],
            )?;
        }
        tx.commit()?;
        stats.files_embedded += 1;
        stats.chunks += chunks.len();
        let secs = t0.elapsed().as_secs_f64();
        eprint!(
            "\r{} files, {} chunks, {:.1} chunks/s   ",
            stats.files_embedded,
            stats.chunks,
            stats.chunks as f64 / secs
        );
    }
    if stats.files_embedded > 0 {
        eprintln!();
    }

    let prefix = format!("{}/", root.display());
    let gone: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\'")?;
        let pattern = format!(
            "{}%",
            prefix
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let rows = stmt.query_map([pattern], |r| r.get::<_, String>(0))?;
        rows.filter_map(Result::ok)
            .filter(|p| !seen.contains(p))
            .collect()
    };
    for p in &gone {
        conn.execute("DELETE FROM files WHERE path = ?1", [p])?;
    }
    stats.removed = gone.len();
    Ok(stats)
}

pub struct Hit {
    pub score: f32,
    pub path: String,
    pub line: i64,
    pub text: String,
}

/// Every chunk's embedding, flattened, for searching without re-reading SQLite.
pub struct Vectors {
    ids: Vec<i64>,
    data: Vec<f32>,
    /// (max chunk id, chunk count) when loaded, to notice the index changing.
    stamp: (i64, i64),
}

fn stamp(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(id), 0), COUNT(*) FROM chunks",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

impl Vectors {
    pub fn load(conn: &Connection) -> Result<Self> {
        let stamp = stamp(conn)?;
        let mut stmt = conn.prepare("SELECT id, embedding FROM chunks")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let (mut ids, mut data) = (Vec::new(), Vec::new());
        for row in rows {
            let (id, blob) = row?;
            let Some(v) = from_blob(&blob) else { continue };
            ids.push(id);
            data.extend(v);
        }
        Ok(Self { ids, data, stamp })
    }

    /// Reload if chunks were added or removed since loading.
    pub fn refresh(&mut self, conn: &Connection) -> Result<()> {
        if stamp(conn)? != self.stamp {
            *self = Self::load(conn)?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Cosine against every chunk (embeddings are unit length), best `k`.
pub fn search(conn: &Connection, vectors: &Vectors, query: &[f32], k: usize) -> Result<Vec<Hit>> {
    let mut scored: Vec<(f32, i64)> = vectors
        .data
        .as_chunks::<DIM>()
        .0
        .iter()
        .zip(&vectors.ids)
        .map(|(v, id)| (v.iter().zip(query).map(|(a, b)| a * b).sum(), *id))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(k);
    let mut stmt = conn.prepare(
        "SELECT f.path, c.start_line, c.text FROM chunks c JOIN files f ON f.id = c.file_id WHERE c.id = ?1",
    )?;
    let mut hits = Vec::with_capacity(scored.len());
    for (score, id) in scored {
        // Gone since the vectors were loaded.
        let Ok((path, line, text)) =
            stmt.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        else {
            continue;
        };
        hits.push(Hit {
            score,
            path,
            line,
            text,
        });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_are_line_aligned_and_bounded() {
        let text = "a\n".repeat(1000);
        let chunks = chunk(&text);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|(_, c)| c.len() <= CHUNK_CHARS + 1));
        assert_eq!(chunks[0].0, 1);
        assert_eq!(
            chunks.iter().map(|(_, c)| c.lines().count()).sum::<usize>(),
            1000
        );
    }

    #[test]
    fn long_lines_split_on_char_boundaries() {
        let text = "é".repeat(CHUNK_CHARS);
        let chunks = chunk(&text);
        assert!(chunks.len() >= 2);
        assert_eq!(
            chunks
                .iter()
                .map(|(_, c)| c.trim_end().chars().count())
                .sum::<usize>(),
            CHUNK_CHARS
        );
    }

    #[test]
    fn blank_text_has_no_chunks() {
        assert!(chunk("\n\n  \n").is_empty());
    }

    #[test]
    fn blob_roundtrip() {
        let v: Vec<f32> = (0..DIM).map(|i| i as f32 / 7.0).collect();
        assert_eq!(from_blob(&to_blob(&v)).unwrap(), v);
        assert!(from_blob(&[0u8; 12]).is_none());
    }
}
