//! Files -> chunks -> embeddings in SQLite, and brute-force search over them.
//!
//! Plaintext on disk for now. Embeddings can be partly inverted back to their
//! text, so this needs encrypting at rest (TPM-sealed key) before it holds
//! anything a user would not leave readable.

use crate::config::Excludes;
use crate::embed::{DIM, Embedder, document_text};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};
use walkdir::WalkDir;

/// About 250 tokens of English, inside the 256-token document shape.
const CHUNK_CHARS: usize = 900;
const MAX_FILE_BYTES: u64 = 1 << 20;
const EXTENSIONS: &[&str] = &[
    "md", "txt", "rst", "org", "tex", "rs", "py", "js", "ts", "tsx", "jsx", "go", "c", "h", "cpp",
    "hpp", "java", "kt", "swift", "rb", "sh", "bash", "zsh", "fish", "html", "css", "scss", "sql",
    "lua", "zig", "nix", "cs", "php",
];
// No config formats (toml, yaml, json, ini, conf, ...): they are where
// credentials live, and the index is not encrypted yet (#4).
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

/// The index holds copies of file contents, so only the user may read it:
/// the directory 0700, the database 0600 (SQLite gives its -wal and -shm
/// files the database's mode).
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| path.display().to_string())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
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
         CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);
         CREATE TABLE IF NOT EXISTS roots (path TEXT PRIMARY KEY);",
    )?;
    Ok(conn)
}

pub(crate) fn wanted(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

pub(crate) fn skipped_dir(name: &str) -> bool {
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

/// Hidden directories and build output under `root` (not `root` itself).
pub fn skipped_under(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        let mut parts: Vec<_> = rel.components().collect();
        parts.pop(); // the file or directory itself is judged by `wanted` / its own walk
        parts
            .iter()
            .any(|c| skipped_dir(&c.as_os_str().to_string_lossy()))
    })
}

/// Everything under `root` that indexing looks at, skipping hidden and build
/// directories and anything excluded.
pub fn walk<'a>(root: &Path, ex: &'a Excludes) -> impl Iterator<Item = walkdir::DirEntry> + 'a {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(move |e| {
            !ex.contains(e.path())
                && (e.depth() == 0
                    || !(e.file_type().is_dir() && skipped_dir(&e.file_name().to_string_lossy())))
        })
        .filter_map(|e| e.map_err(|e| eprintln!("skip: {e}")).ok())
}

fn like_under(dir: &str) -> String {
    format!(
        "{}/%",
        dir.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

/// Drop `path` and, if it was a directory, everything under it. Returns files removed.
pub fn remove(conn: &Connection, path: &Path) -> Result<usize> {
    let p = path.to_string_lossy();
    let n = conn.execute(
        "DELETE FROM files WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
        params![p, like_under(&p)],
    )?;
    Ok(n)
}

pub enum Outcome {
    Embedded(usize),
    Unchanged,
    /// Not a wanted file (type, size, encoding); any old entry for it is dropped.
    Skipped,
}

/// Index one file if it is wanted and changed since it was last indexed.
pub fn index_file(conn: &mut Connection, emb: &mut Embedder, path: &Path) -> Result<Outcome> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && m.len() <= MAX_FILE_BYTES && wanted(path) => m,
        _ => {
            remove(conn, path)?;
            return Ok(Outcome::Skipped);
        }
    };
    let Some(key) = path.to_str() else {
        return Ok(Outcome::Skipped);
    };
    let mtime = meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let size = meta.len() as i64;
    let known: Option<(i64, i64)> = conn
        .query_row(
            "SELECT mtime, size FROM files WHERE path = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    if known == Some((mtime, size)) {
        return Ok(Outcome::Unchanged);
    }
    let Ok(text) = fs::read_to_string(path) else {
        remove(conn, path)?;
        return Ok(Outcome::Skipped);
    };
    let chunks = chunk(&text);
    let title = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let inputs: Vec<String> = chunks
        .iter()
        .map(|(_, c)| document_text(&title, c))
        .collect();
    let vectors = emb.documents(&inputs)?;

    let tx = conn.transaction()?;
    tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
    tx.execute(
        "INSERT INTO files (path, mtime, size) VALUES (?1, ?2, ?3)",
        params![key, mtime, size],
    )?;
    let file_id = tx.last_insert_rowid();
    for ((line, text), v) in chunks.iter().zip(&vectors) {
        tx.execute(
            "INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (?1, ?2, ?3, ?4)",
            params![file_id, *line as i64, text, to_blob(v)],
        )?;
    }
    tx.commit()?;
    Ok(Outcome::Embedded(chunks.len()))
}

/// Drop everything indexed under an excluded path. Returns files removed.
pub fn purge(conn: &Connection, ex: &Excludes) -> Result<usize> {
    let mut n = 0;
    for p in ex.paths() {
        n += remove(conn, p)?;
    }
    Ok(n)
}

/// Index everything wanted under `root` and remember it as a root to watch.
/// Unchanged files (same mtime and size) are skipped; files under `root`
/// that are gone are removed.
pub fn index(
    conn: &mut Connection,
    emb: &mut Embedder,
    root: &Path,
    ex: &Excludes,
) -> Result<Stats> {
    let root = fs::canonicalize(root).with_context(|| root.display().to_string())?;
    if ex.contains(&root) {
        bail!(
            "{} is excluded; remove it from `exclude` in the config to index it",
            root.display()
        );
    }
    purge(conn, ex)?;
    conn.execute(
        "INSERT OR IGNORE INTO roots (path) VALUES (?1)",
        [root.to_string_lossy()],
    )?;
    let mut stats = Stats {
        files_seen: 0,
        files_embedded: 0,
        chunks: 0,
        removed: 0,
    };
    let mut seen = HashSet::new();
    let t0 = Instant::now();
    for entry in walk(&root, ex) {
        if !entry.file_type().is_file() || !wanted(entry.path()) {
            continue;
        }
        if let Some(p) = entry.path().to_str() {
            seen.insert(p.to_owned());
        }
        match index_file(conn, emb, entry.path())? {
            Outcome::Embedded(n) => {
                stats.files_embedded += 1;
                stats.chunks += n;
                let secs = t0.elapsed().as_secs_f64();
                eprint!(
                    "\r{} files, {} chunks, {:.1} chunks/s   ",
                    stats.files_embedded,
                    stats.chunks,
                    stats.chunks as f64 / secs
                );
            }
            Outcome::Unchanged => {}
            Outcome::Skipped => {
                if let Some(p) = entry.path().to_str() {
                    seen.remove(p);
                }
                continue;
            }
        }
        stats.files_seen += 1;
    }
    if stats.files_embedded > 0 {
        eprintln!();
    }

    let gone: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\'")?;
        let rows = stmt.query_map([like_under(&root.to_string_lossy())], |r| {
            r.get::<_, String>(0)
        })?;
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

/// Stop treating `root` as a root and drop everything indexed under it.
/// Returns files removed.
pub fn forget(conn: &Connection, root: &Path) -> Result<usize> {
    conn.execute(
        "DELETE FROM roots WHERE path = ?1",
        [root.to_string_lossy()],
    )?;
    remove(conn, root)
}

/// Files and chunks in the index.
pub fn counts(conn: &Connection) -> Result<(i64, i64)> {
    Ok((
        conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?,
        conn.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?,
    ))
}

/// Roots `index` has been run on, which the watcher follows.
pub fn roots(conn: &Connection) -> Result<Vec<PathBuf>> {
    let mut stmt = conn.prepare("SELECT path FROM roots ORDER BY path")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(Result::ok).map(PathBuf::from).collect())
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
    fn index_is_private_to_the_user() {
        let dir = std::env::temp_dir().join(format!("vinosearch-test-{}", std::process::id()));
        let db = dir.join("sub/index.sqlite");
        open(&db).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(db.parent().unwrap()), 0o700);
        assert_eq!(mode(&db), 0o600);
        // An existing, looser database is tightened on open.
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        open(&db).unwrap();
        assert_eq!(mode(&db), 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skipped_under_judges_parent_directories_only() {
        let root = Path::new("/r");
        assert!(skipped_under(root, Path::new("/r/.git/HEAD")));
        assert!(skipped_under(root, Path::new("/r/a/target/x.rs")));
        assert!(!skipped_under(root, Path::new("/r/a/b.rs")));
        assert!(!skipped_under(root, Path::new("/r/target")));
        assert!(!skipped_under(
            Path::new("/home/u/.notes"),
            Path::new("/home/u/.notes/a.md")
        ));
    }

    #[test]
    fn remove_drops_a_file_or_a_whole_directory() {
        let dir = std::env::temp_dir().join(format!("vinosearch-rm-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        for p in ["/a/x.md", "/a/b/y.md", "/a_b/z.md", "/c.md"] {
            conn.execute(
                "INSERT INTO files (path, mtime, size) VALUES (?1, 0, 0)",
                [p],
            )
            .unwrap();
        }
        assert_eq!(remove(&conn, Path::new("/a")).unwrap(), 2);
        assert_eq!(remove(&conn, Path::new("/c.md")).unwrap(), 1);
        let left: Vec<String> = conn
            .prepare("SELECT path FROM files")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(left, ["/a_b/z.md"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_files_are_not_indexed() {
        for f in ["a.toml", "b.yaml", "c.json", "d.ini", "e.conf", ".env"] {
            assert!(!wanted(Path::new(f)), "{f}");
        }
        assert!(wanted(Path::new("notes.md")));
    }

    #[test]
    fn blob_roundtrip() {
        let v: Vec<f32> = (0..DIM).map(|i| i as f32 / 7.0).collect();
        assert_eq!(from_blob(&to_blob(&v)).unwrap(), v);
        assert!(from_blob(&[0u8; 12]).is_none());
    }
}
