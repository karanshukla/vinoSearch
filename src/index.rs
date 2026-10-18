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
use std::time::UNIX_EPOCH;
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
/// Directories that are always build output, caches, generated reports or
/// vendored dependencies. Generic names a person might use for their own
/// files (`bin`, `out`, `lib`) are left out.
const SKIP_DIRS: &[&str] = &[
    // JavaScript
    "node_modules",
    "bower_components",
    "dist",
    // Rust, Java, Gradle, CMake, Elixir, Sphinx
    "target",
    "build",
    "_build",
    // .NET intermediate output (generated .cs files)
    "obj",
    // Python
    "__pycache__",
    "venv",
    "site-packages",
    // Go, PHP, Ruby
    "vendor",
    // Apple
    "Pods",
    "DerivedData",
    // Elm
    "elm-stuff",
    // Test coverage reports
    "coverage",
    "htmlcov",
];
/// Directory name prefixes with the same meaning (`cmake-build-debug`, ...).
const SKIP_DIR_PREFIXES: &[&str] = &["cmake-build-"];
/// Minified or bundled files: generated, one enormous line, never what a
/// search is for.
const SKIP_FILE_SUFFIXES: &[&str] = &[".min.js", ".min.css", ".bundle.js"];

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
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    !SKIP_FILE_SUFFIXES.iter().any(|s| name.ends_with(s))
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

pub(crate) fn skipped_dir(name: &str) -> bool {
    name.starts_with('.')
        || SKIP_DIRS.contains(&name)
        || SKIP_DIR_PREFIXES.iter().any(|p| name.starts_with(p))
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

#[cfg(test)]
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

/// (mtime, size) as the index records them.
fn stamp_of(meta: &fs::Metadata) -> Option<(i64, i64)> {
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((mtime.as_secs() as i64, meta.len() as i64))
}

/// Paths under `root` the index is behind on: wanted files that are new or
/// changed (by mtime and size), and indexed files that are gone. Needs no
/// model, so a runner can check at start whether it has anything to do.
pub fn stale(conn: &Connection, root: &Path, ex: &Excludes) -> Result<Vec<PathBuf>> {
    let mut known: std::collections::HashMap<String, (i64, i64)> = {
        let mut stmt =
            conn.prepare("SELECT path, mtime, size FROM files WHERE path LIKE ?1 ESCAPE '\\'")?;
        let rows = stmt.query_map([like_under(&root.to_string_lossy())], |r| {
            Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::new();
    for entry in walk(root, ex).filter(|e| e.file_type().is_file()) {
        let path = entry.path();
        let Some(key) = path.to_str() else { continue };
        let was = known.remove(key);
        let wanted_now = wanted(path) && entry.metadata().is_ok_and(|m| m.len() <= MAX_FILE_BYTES);
        if !wanted_now {
            // An indexed file that is no longer wanted (grew too big, say).
            if was.is_some() {
                out.push(path.to_path_buf());
            }
            continue;
        }
        if was != entry.metadata().ok().as_ref().and_then(stamp_of) {
            out.push(path.to_path_buf());
        }
    }
    // Indexed, but not found by the walk: deleted, or now excluded or skipped.
    out.extend(known.into_keys().map(PathBuf::from));
    Ok(out)
}

/// Drop everything indexed under an excluded path. Returns files removed.
pub fn purge(conn: &Connection, ex: &Excludes) -> Result<usize> {
    let mut n = 0;
    for p in ex.paths() {
        n += remove(conn, p)?;
    }
    if n > 0 {
        compact(conn)?;
    }
    Ok(n)
}

/// Hand freed space back once over a quarter of the file is free pages.
/// SQLite otherwise keeps a purged index's full size: excluding 80% of a
/// 113k-chunk index left the file at 470 MB until a VACUUM took it to 92 MB
/// (0.6 s).
pub fn compact(conn: &Connection) -> Result<()> {
    let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    if free * 4 > pages {
        conn.execute_batch("VACUUM")?;
    }
    Ok(())
}

/// Index everything wanted under `root` and remember it as a root to watch.
/// Unchanged files (same mtime and size) are skipped; files under `root`
/// that are gone are removed. `progress` is called after each file embedded.
pub fn index(
    conn: &mut Connection,
    emb: &mut Embedder,
    root: &Path,
    ex: &Excludes,
    progress: &mut dyn FnMut(&Stats),
) -> Result<Stats> {
    let root = fs::canonicalize(root).with_context(|| root.display().to_string())?;
    if ex.contains(&root) {
        bail!(
            "{} is excluded; remove it from `exclude` in the config to index it",
            root.display()
        );
    }
    let purged = purge(conn, ex)?;
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
                progress(&stats);
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
    stats.removed = gone.len() + purged;
    if !gone.is_empty() {
        compact(conn)?;
    }
    Ok(stats)
}

/// Stop treating `root` as a root and drop everything indexed under it.
/// Returns files removed.
pub fn forget(conn: &Connection, root: &Path) -> Result<usize> {
    conn.execute(
        "DELETE FROM roots WHERE path = ?1",
        [root.to_string_lossy()],
    )?;
    let n = remove(conn, root)?;
    if n > 0 {
        compact(conn)?;
    }
    Ok(n)
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

/// Every chunk's embedding, held as int8 with a per-vector scale: a quarter
/// of the memory, and a quarter of the bytes each query reads, which is what
/// bounds brute-force search. On a 113k-chunk index the top result scored at
/// most 0.001 below f32's (mean 0.00001), and 97% of the top 40 matched.
pub struct Vectors {
    ids: Vec<i64>,
    data: Vec<i8>,
    scales: Vec<f32>,
    /// Database state when loaded, to notice the index changing.
    stamp: (i64, u64),
}

/// Changes when anything is written: `data_version` for other connections'
/// commits, `total_changes` for this one's. Both are O(1); counting chunks
/// scanned the whole table on every query.
fn stamp(conn: &Connection) -> Result<(i64, u64)> {
    let version = conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
    Ok((version, conn.total_changes()))
}

impl Vectors {
    fn empty() -> Self {
        Self {
            ids: Vec::new(),
            data: Vec::new(),
            scales: Vec::new(),
            stamp: (0, 0),
        }
    }

    pub fn load(conn: &Connection) -> Result<Self> {
        let stamp = stamp(conn)?;
        let n: i64 = conn.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?;
        let n = n as usize;
        let mut out = Self {
            ids: Vec::with_capacity(n),
            data: Vec::with_capacity(n * DIM),
            scales: Vec::with_capacity(n),
            stamp,
        };
        let mut v = [0f32; DIM];
        let mut stmt = conn.prepare("SELECT id, embedding FROM chunks")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            // Borrowed, so no copy of each blob is made on the way in.
            let rusqlite::types::ValueRef::Blob(blob) = row.get_ref(1)? else {
                continue;
            };
            if blob.len() != DIM * 4 {
                continue;
            }
            for (x, c) in v.iter_mut().zip(blob.as_chunks::<4>().0) {
                *x = f32::from_le_bytes(*c);
            }
            let (q, scale) = quantize(&v);
            out.ids.push(row.get(0)?);
            out.data.extend_from_slice(&q);
            out.scales.push(scale);
        }
        Ok(out)
    }

    /// Reload if the index changed since loading. The old copy is dropped
    /// first, so a reload never holds two.
    pub fn refresh(&mut self, conn: &Connection) -> Result<()> {
        if stamp(conn)? != self.stamp {
            *self = Self::empty();
            *self = Self::load(conn)?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Symmetric int8: `v ≈ q * scale`, with the largest component at ±127.
fn quantize(v: &[f32; DIM]) -> ([i8; DIM], f32) {
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs()));
    if max == 0.0 {
        return ([0; DIM], 0.0);
    }
    let scale = max / 127.0;
    (std::array::from_fn(|i| (v[i] / scale).round() as i8), scale)
}

/// Lanes summed independently, so the compiler can use SIMD.
const LANES: usize = 16;

fn dot(a: &[i8; DIM], b: &[i8; DIM]) -> i32 {
    let mut acc = [0i32; LANES];
    for (x, y) in a
        .as_chunks::<LANES>()
        .0
        .iter()
        .zip(b.as_chunks::<LANES>().0)
    {
        for i in 0..LANES {
            acc[i] += i32::from(x[i]) * i32::from(y[i]);
        }
    }
    acc.iter().sum()
}

/// Cosine against every chunk (embeddings are unit length), best `k`.
pub fn search(conn: &Connection, vectors: &Vectors, query: &[f32], k: usize) -> Result<Vec<Hit>> {
    let query: &[f32; DIM] = query.try_into().context("query is not DIM long")?;
    let (q, qs) = quantize(query);
    let mut scored: Vec<(f32, i64)> = vectors
        .data
        .as_chunks::<DIM>()
        .0
        .iter()
        .zip(&vectors.scales)
        .zip(&vectors.ids)
        .map(|((v, s), id)| (dot(v, &q) as f32 * s * qs, *id))
        .collect();
    let by_score = |a: &(f32, i64), b: &(f32, i64)| b.0.total_cmp(&a.0);
    if scored.len() > k && k > 0 {
        scored.select_nth_unstable_by(k - 1, by_score);
        scored.truncate(k);
    }
    scored.sort_by(by_score);
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

    #[test]
    fn generated_trees_and_files_are_skipped() {
        for d in [
            "coverage",
            "obj",
            "vendor",
            "cmake-build-release",
            ".git",
            "node_modules",
        ] {
            assert!(skipped_dir(d), "{d}");
        }
        for d in ["src", "bin", "out", "docs", "covered"] {
            assert!(!skipped_dir(d), "{d}");
        }
        assert!(!wanted(Path::new("app.min.js")));
        assert!(!wanted(Path::new("vendor.bundle.js")));
        assert!(wanted(Path::new("app.js")));
        assert!(wanted(Path::new("README.md")));
    }

    #[test]
    fn compact_returns_freed_pages() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (b BLOB); PRAGMA page_size = 4096;")
            .unwrap();
        for _ in 0..200 {
            conn.execute("INSERT INTO t VALUES (zeroblob(4000))", [])
                .unwrap();
        }
        conn.execute("DELETE FROM t", []).unwrap();
        let free = |c: &Connection| -> i64 {
            c.query_row("PRAGMA freelist_count", [], |r| r.get(0))
                .unwrap()
        };
        assert!(free(&conn) > 100);
        compact(&conn).unwrap();
        assert_eq!(free(&conn), 0);
    }

    #[test]
    fn stale_finds_new_changed_and_gone_files() {
        let dir = std::env::temp_dir().join(format!("vinosearch-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let conn = open(&dir.join("db/index.sqlite")).unwrap();
        let root = dir.join("root");
        fs::create_dir_all(&root).unwrap();
        for f in ["same.md", "edited.md", "new.md", "skip.toml"] {
            fs::write(root.join(f), f).unwrap();
        }
        let record = |name: &str, stamp: (i64, i64)| {
            conn.execute(
                "INSERT INTO files (path, mtime, size) VALUES (?1, ?2, ?3)",
                params![root.join(name).to_str().unwrap(), stamp.0, stamp.1],
            )
            .unwrap();
        };
        let real = |name: &str| stamp_of(&fs::metadata(root.join(name)).unwrap()).unwrap();
        record("same.md", real("same.md"));
        record("edited.md", (0, 0));
        record("deleted.md", (0, 0));
        record("skip.toml", real("skip.toml"));
        let mut got: Vec<String> = stale(&conn, &root, &Excludes::default())
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, ["deleted.md", "edited.md", "new.md", "skip.toml"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn int8_scores_track_f32() {
        let a: [f32; DIM] = std::array::from_fn(|i| ((i * 7) % 13) as f32 / 13.0 - 0.5);
        let b: [f32; DIM] = std::array::from_fn(|i| ((i * 5) % 11) as f32 / 11.0 - 0.5);
        let exact: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        let ((qa, sa), (qb, sb)) = (quantize(&a), quantize(&b));
        let approx = dot(&qa, &qb) as f32 * sa * sb;
        assert!(
            (approx - exact).abs() < 0.01 * exact.abs().max(1.0),
            "{approx} vs {exact}"
        );
        assert_eq!(quantize(&[0.0; DIM]), ([0; DIM], 0.0));
    }

    #[test]
    fn search_returns_the_best_k_in_order() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, mtime INTEGER, size INTEGER);
             CREATE TABLE chunks (id INTEGER PRIMARY KEY, file_id INTEGER, start_line INTEGER, text TEXT, embedding BLOB);
             INSERT INTO files VALUES (1, '/f', 0, 0);",
        )
        .unwrap();
        // Chunk i points along axis 0 with weight i/10, so higher i scores higher.
        for i in 0..10 {
            let mut v = vec![0f32; DIM];
            v[0] = i as f32 / 10.0;
            conn.execute(
                "INSERT INTO chunks VALUES (?1, 1, ?1, '', ?2)",
                params![i + 1, to_blob(&v)],
            )
            .unwrap();
        }
        let vectors = Vectors::load(&conn).unwrap();
        let mut q = vec![0f32; DIM];
        q[0] = 1.0;
        let lines: Vec<i64> = search(&conn, &vectors, &q, 3)
            .unwrap()
            .iter()
            .map(|h| h.line)
            .collect();
        assert_eq!(lines, [10, 9, 8]);
    }
}
