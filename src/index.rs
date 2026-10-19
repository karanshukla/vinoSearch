//! Files -> chunks -> embeddings in SQLite, and brute-force search over them.
//! A text file is many chunks; an image is one, embedded from its pixels.
//!
//! Plaintext on disk for now. Embeddings can be partly inverted back to their
//! text, so this needs encrypting at rest (TPM-sealed key) before it holds
//! anything a user would not leave readable.

use crate::config::{Excludes, Formats};
use crate::embed::{DIM, Embedder, document_text};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
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
    // Conda installs, wherever they are
    "miniconda3",
    "anaconda3",
    "miniforge3",
    "mambaforge",
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

/// Images past this are skipped: decoding takes several times the file in memory.
const MAX_IMAGE_BYTES: u64 = 64 << 20;

/// What a chunk was embedded from, stored in `chunks.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text = 0,
    Image = 1,
}

impl Kind {
    fn from_db(v: i64) -> Self {
        if v == Kind::Image as i64 {
            Kind::Image
        } else {
            Kind::Text
        }
    }

    fn max_bytes(self) -> u64 {
        match self {
            Kind::Text => MAX_FILE_BYTES,
            Kind::Image => MAX_IMAGE_BYTES,
        }
    }
}

/// The file types indexed: text always, images by extension while they are
/// on (#73).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileTypes {
    images: Vec<String>,
}

impl FileTypes {
    pub fn new(formats: &Formats) -> Result<Self> {
        Ok(Self {
            images: formats.image_extensions()?,
        })
    }

    /// The kind `path` is indexed as, by its name; None if it is not.
    pub fn kind(&self, path: &Path) -> Option<Kind> {
        if text_file(path) {
            return Some(Kind::Text);
        }
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.images.contains(&ext).then_some(Kind::Image)
    }
}

/// Log entries kept. A Vectors further behind than this reloads in full,
/// which at that many changes is no slower than applying them.
const CHUNK_LOG_KEEP: i64 = 100_000;

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
    conn.execute_batch(&format!(
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
         CREATE TABLE IF NOT EXISTS roots (path TEXT PRIMARY KEY);
         -- Every chunk id inserted or deleted, in commit order, so a loaded
         -- Vectors can catch up without rereading the table. AUTOINCREMENT
         -- because seq must never be reused; chunks.id can be (#27). The
         -- triggers also see deletes cascaded from files.
         CREATE TABLE IF NOT EXISTS chunk_log (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             chunk_id INTEGER NOT NULL
         );
         CREATE TRIGGER IF NOT EXISTS chunk_log_insert AFTER INSERT ON chunks
         BEGIN INSERT INTO chunk_log (chunk_id) VALUES (NEW.id); END;
         CREATE TRIGGER IF NOT EXISTS chunk_log_delete AFTER DELETE ON chunks
         BEGIN INSERT INTO chunk_log (chunk_id) VALUES (OLD.id); END;
         CREATE TRIGGER IF NOT EXISTS chunk_log_prune AFTER INSERT ON chunk_log
         BEGIN DELETE FROM chunk_log WHERE seq <= NEW.seq - {CHUNK_LOG_KEEP}; END;"
    ))?;
    // Content hashes (#28), added to databases from before them. Their rows
    // stay NULL until `index` next sees the file.
    let has_hash: bool = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('files') WHERE name = 'sha256'",
        [],
        |r| r.get(0),
    )?;
    if !has_hash {
        conn.execute_batch("ALTER TABLE files ADD COLUMN sha256 BLOB")?;
    }
    // What each chunk was embedded from (#73), added to databases from before
    // images: everything in those is text.
    let has_kind: bool = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('chunks') WHERE name = 'kind'",
        [],
        |r| r.get(0),
    )?;
    if !has_kind {
        conn.execute_batch("ALTER TABLE chunks ADD COLUMN kind INTEGER NOT NULL DEFAULT 0")?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS files_sha256 ON files(sha256);
         -- A file whose content another file already has keeps no chunks of
         -- its own. When the one holding them goes (deleted, changed,
         -- forgotten), they pass to a copy instead of being dropped. Chunk
         -- ids don't change, so loaded Vectors stay valid.
         CREATE TRIGGER IF NOT EXISTS files_hand_over BEFORE DELETE ON files
         WHEN OLD.sha256 IS NOT NULL
         BEGIN
             UPDATE chunks
             SET file_id = (SELECT id FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id LIMIT 1)
             WHERE file_id = OLD.id
               AND EXISTS (SELECT 1 FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id);
         END;",
    )?;
    Ok(conn)
}

/// Whether `path` is indexed, as `types` takes it.
pub(crate) fn wanted(path: &Path, types: &FileTypes) -> bool {
    types.kind(path).is_some()
}

fn text_file(path: &Path) -> bool {
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
    /// Files whose content was already indexed, so nothing was embedded.
    pub duplicates: usize,
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

/// A directory, and not a symlink to one. Following a link would index its
/// target under the link's path, where exclusions (canonical paths) miss it.
pub fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Everything under `root` that indexing looks at, skipping hidden and build
/// directories and anything excluded.
pub fn walk<'a>(root: &Path, ex: &'a Excludes) -> impl Iterator<Item = walkdir::DirEntry> + 'a {
    // follow_links(false) alone still follows `root` itself when it is a link.
    WalkDir::new(root)
        .follow_links(false)
        .follow_root_links(false)
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
    /// Same content as a file already indexed, whose chunks it shares.
    Duplicate,
    /// Not a wanted file (type, size, encoding); any old entry for it is dropped.
    Skipped,
}

/// Index one file if it is wanted and changed since it was last indexed.
/// `between` runs after each chunk is embedded, so a 1 MiB file (~1,100
/// chunks, ~40 s) does not hold a waiting query for its whole length. An
/// error from it stops the file unwritten.
pub fn index_file(
    conn: &mut Connection,
    emb: &mut Embedder,
    path: &Path,
    types: &FileTypes,
    between: &mut dyn FnMut(&Connection, &mut Embedder) -> Result<()>,
) -> Result<Outcome> {
    let kind = types.kind(path);
    let (meta, kind) = match (fs::symlink_metadata(path), kind) {
        (Ok(m), Some(kind)) if m.is_file() && m.len() <= kind.max_bytes() => (m, kind),
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
    let known: Option<(i64, i64, i64, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT id, mtime, size, sha256 FROM files WHERE path = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let unchanged = known.as_ref().is_some_and(|k| (k.1, k.2) == (mtime, size));
    if unchanged && known.as_ref().is_some_and(|k| k.3.is_some()) {
        return Ok(Outcome::Unchanged);
    }
    let read = match kind {
        Kind::Text => fs::read_to_string(path).map(String::into_bytes),
        Kind::Image => fs::read(path),
    };
    let Ok(bytes) = read else {
        remove(conn, path)?;
        return Ok(Outcome::Skipped);
    };
    let hash = Sha256::digest(&bytes).to_vec();
    let holder = holder_of(conn, &hash, key)?;
    if let Some((id, _, _, old_hash)) = known {
        if old_hash.as_ref() == Some(&hash) {
            // Touched, not changed.
            conn.execute(
                "UPDATE files SET mtime = ?1, size = ?2 WHERE id = ?3",
                params![mtime, size, id],
            )?;
            return Ok(Outcome::Unchanged);
        }
        if unchanged {
            // Indexed before content hashes: record this one, and if another
            // file already holds the same chunks, drop this file's copy.
            conn.execute(
                "UPDATE files SET sha256 = ?1 WHERE id = ?2",
                params![hash, id],
            )?;
            if holder.is_some() && conn.execute("DELETE FROM chunks WHERE file_id = ?1", [id])? > 0
            {
                return Ok(Outcome::Duplicate);
            }
            return Ok(Outcome::Unchanged);
        }
    }
    if holder.is_some() {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
        tx.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, ?4)",
            params![key, mtime, size, hash],
        )?;
        tx.commit()?;
        return Ok(Outcome::Duplicate);
    }
    let (chunks, vectors) = match kind {
        Kind::Text => {
            let text = String::from_utf8(bytes).expect("read as a string");
            let chunks = chunk(&text);
            let title = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            let mut vectors = Vec::with_capacity(chunks.len());
            for (_, c) in &chunks {
                vectors.extend(emb.documents(&[document_text(&title, c)])?);
                between(conn, emb)?;
            }
            (chunks, vectors)
        }
        // One chunk, with no text and line 0, from the pixels alone.
        Kind::Image => {
            let pixels =
                match crate::vision::load(path).and_then(|img| crate::vision::prepare(&img)) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("skip {}: {e:#}", path.display());
                        remove(conn, path)?;
                        return Ok(Outcome::Skipped);
                    }
                };
            let v = emb.image(&pixels)?;
            between(conn, emb)?;
            (vec![(0, String::new())], vec![v])
        }
    };

    let tx = conn.transaction()?;
    tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
    tx.execute(
        "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, ?4)",
        params![key, mtime, size, hash],
    )?;
    let file_id = tx.last_insert_rowid();
    for ((line, text), v) in chunks.iter().zip(&vectors) {
        tx.execute(
            "INSERT INTO chunks (file_id, start_line, text, embedding, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![file_id, *line as i64, text, to_blob(v), kind as i64],
        )?;
    }
    tx.commit()?;
    Ok(Outcome::Embedded(chunks.len()))
}

/// Another file with content `hash` that holds its chunks, if there is one.
fn holder_of(conn: &Connection, hash: &[u8], except: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT f.id FROM files f WHERE f.sha256 = ?1 AND f.path != ?2
             AND EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = f.id) LIMIT 1",
            params![hash, except],
            |r| r.get(0),
        )
        .optional()?)
}

/// (mtime, size) as the index records them.
fn stamp_of(meta: &fs::Metadata) -> Option<(i64, i64)> {
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((mtime.as_secs() as i64, meta.len() as i64))
}

/// Paths under `root` the index is behind on: wanted files that are new or
/// changed (by mtime and size), and indexed files that are gone. Needs no
/// model, so a runner can check at start whether it has anything to do.
pub fn stale(
    conn: &Connection,
    root: &Path,
    ex: &Excludes,
    types: &FileTypes,
) -> Result<Vec<PathBuf>> {
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
        let wanted_now = types
            .kind(path)
            .is_some_and(|k| entry.metadata().is_ok_and(|m| m.len() <= k.max_bytes()));
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

/// Drop every indexed file `types` no longer takes (images, once turned
/// off). Returns files removed.
pub fn purge_types(conn: &Connection, types: &FileTypes) -> Result<usize> {
    let paths: Vec<String> = conn
        .prepare("SELECT path FROM files")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let tx = conn.unchecked_transaction()?;
    let mut n = 0;
    for p in paths.iter().filter(|p| !wanted(Path::new(p), types)) {
        n += tx.execute("DELETE FROM files WHERE path = ?1", [p])?;
    }
    tx.commit()?;
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

/// Called after each file `index` looks at, with the connection and model
/// lent back: for progress, and for the runner to answer queries mid-index.
/// An error from it stops indexing; every file finished so far is kept.
pub type AfterFile<'a> = dyn FnMut(&Stats, &Connection, &mut Embedder) -> Result<()> + 'a;

/// Index everything wanted under `root` and remember it as a root to watch.
/// Unchanged files (same mtime and size) are skipped; files under `root`
/// that are gone are removed.
pub fn index(
    conn: &mut Connection,
    emb: &mut Embedder,
    root: &Path,
    ex: &Excludes,
    types: &FileTypes,
    after: &mut AfterFile,
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
        duplicates: 0,
    };
    let mut seen = HashSet::new();
    for entry in walk(&root, ex) {
        if !entry.file_type().is_file() || !wanted(entry.path(), types) {
            continue;
        }
        if let Some(p) = entry.path().to_str() {
            seen.insert(p.to_owned());
        }
        match index_file(conn, emb, entry.path(), types, &mut |c, e| {
            after(&stats, c, e)
        })? {
            Outcome::Embedded(n) => {
                stats.files_embedded += 1;
                stats.chunks += n;
                stats.files_seen += 1;
            }
            Outcome::Unchanged => stats.files_seen += 1,
            Outcome::Duplicate => {
                stats.duplicates += 1;
                stats.files_seen += 1;
            }
            Outcome::Skipped => {
                if let Some(p) = entry.path().to_str() {
                    seen.remove(p);
                }
            }
        }
        after(&stats, conn, emb)?;
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

/// Stop treating `root` as a root and drop what was indexed under it, except
/// files another root still covers (with roots `~/Dev` and `~/Dev/proj`,
/// forgetting either keeps the files `~/Dev` still walks). Returns files
/// removed.
pub fn forget(conn: &Connection, root: &Path) -> Result<usize> {
    conn.execute(
        "DELETE FROM roots WHERE path = ?1",
        [root.to_string_lossy()],
    )?;
    let others: Vec<PathBuf> = roots(conn)?
        .into_iter()
        .filter(|r| r.starts_with(root) || root.starts_with(r))
        .collect();
    let n = if others.is_empty() {
        remove(conn, root)?
    } else {
        let covered = |p: &Path| {
            others
                .iter()
                .any(|r| p.starts_with(r) && !skipped_under(r, p))
        };
        let under: Vec<String> = conn
            .prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\'")?
            .query_map([like_under(&root.to_string_lossy())], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        for p in under.iter().filter(|p| !covered(Path::new(p))) {
            n += tx.execute("DELETE FROM files WHERE path = ?1", [p])?;
        }
        tx.commit()?;
        n
    };
    if n > 0 {
        compact(conn)?;
    }
    Ok(n)
}

/// Whether `path` is an indexed file.
pub fn is_indexed(conn: &Connection, path: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM files WHERE path = ?1", [path], |_| Ok(()))
        .optional()?
        .is_some())
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
    let mut biggest_child: HashMap<&Path, u64> = HashMap::new();
    for (dir, n) in &chunks {
        if let Some(parent) = dir.parent() {
            let m = biggest_child.entry(parent).or_default();
            *m = (*m).max(*n);
        }
    }
    let mut dirs: Vec<(&PathBuf, u64)> = chunks.iter().map(|(d, n)| (d, *n)).collect();
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
    Ok(picked)
}

pub struct Hit {
    pub kind: Kind,
    pub score: f32,
    pub path: String,
    pub line: i64,
    pub text: String,
    /// Other indexed files with the same content.
    pub copies: Vec<String>,
}

/// Every chunk's embedding, held as int8 with a per-vector scale: a quarter
/// of the memory, and a quarter of the bytes each query reads, which is what
/// bounds brute-force search. On a 113k-chunk index the top result scored at
/// most 0.001 below f32's (mean 0.00001), and 97% of the top 40 matched.
pub struct Vectors {
    ids: Vec<i64>,
    data: Vec<i8>,
    scales: Vec<f32>,
    kinds: Vec<Kind>,
    /// Where each id is in `ids`, for removing it.
    pos: HashMap<i64, usize>,
    /// Database state when loaded, to notice the index changing.
    stamp: (i64, u64),
    /// The last `chunk_log` entry these vectors include.
    seq: i64,
}

/// Changes when anything is written: `data_version` for other connections'
/// commits, `total_changes` for this one's. Both are O(1); counting chunks
/// scanned the whole table on every query.
fn stamp(conn: &Connection) -> Result<(i64, u64)> {
    let version = conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
    Ok((version, conn.total_changes()))
}

/// The newest `chunk_log` seq ever assigned (0 for none), from
/// sqlite_sequence so it survives the log being pruned empty.
fn log_end(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'chunk_log'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

impl Vectors {
    fn empty() -> Self {
        Self {
            ids: Vec::new(),
            data: Vec::new(),
            scales: Vec::new(),
            kinds: Vec::new(),
            pos: HashMap::new(),
            stamp: (0, 0),
            seq: 0,
        }
    }

    /// Append one chunk's embedding, if the blob is a DIM-long f32 vector.
    fn push(&mut self, id: i64, blob: &[u8], kind: Kind) {
        if blob.len() != DIM * 4 {
            return;
        }
        let mut v = [0f32; DIM];
        for (x, c) in v.iter_mut().zip(blob.as_chunks::<4>().0) {
            *x = f32::from_le_bytes(*c);
        }
        let (q, scale) = quantize(&v);
        self.pos.insert(id, self.ids.len());
        self.ids.push(id);
        self.data.extend_from_slice(&q);
        self.scales.push(scale);
        self.kinds.push(kind);
    }

    /// Drop `id` by moving the last vector into its slot.
    fn remove(&mut self, id: i64) {
        let Some(p) = self.pos.remove(&id) else {
            return;
        };
        let last = self.ids.len() - 1;
        if p != last {
            self.data.copy_within(last * DIM..(last + 1) * DIM, p * DIM);
            self.ids[p] = self.ids[last];
            self.scales[p] = self.scales[last];
            self.kinds[p] = self.kinds[last];
            self.pos.insert(self.ids[p], p);
        }
        self.ids.truncate(last);
        self.data.truncate(last * DIM);
        self.scales.truncate(last);
        self.kinds.truncate(last);
    }

    pub fn load(conn: &Connection) -> Result<Self> {
        // One read transaction, so the log position matches the rows read.
        let tx = conn.unchecked_transaction()?;
        let n: i64 = tx.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?;
        let n = n as usize;
        let mut out = Self {
            ids: Vec::with_capacity(n),
            data: Vec::with_capacity(n * DIM),
            scales: Vec::with_capacity(n),
            kinds: Vec::with_capacity(n),
            pos: HashMap::with_capacity(n),
            stamp: stamp(&tx)?,
            seq: log_end(&tx)?,
        };
        let mut stmt = tx.prepare("SELECT id, embedding, kind FROM chunks")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            // Borrowed, so no copy of each blob is made on the way in.
            if let rusqlite::types::ValueRef::Blob(blob) = row.get_ref(1)? {
                out.push(row.get(0)?, blob, Kind::from_db(row.get(2)?));
            }
        }
        Ok(out)
    }

    /// Catch up with the index if it changed since loading: only the chunks
    /// inserted or deleted since, from `chunk_log`. A full reload when the
    /// log no longer reaches back far enough, or when most of the index
    /// changed; the old copy is dropped first, so a reload never holds two.
    pub fn refresh(&mut self, conn: &Connection) -> Result<()> {
        if stamp(conn)? == self.stamp {
            return Ok(());
        }
        if !self.catch_up(conn)? {
            *self = Self::empty();
            *self = Self::load(conn)?;
        }
        Ok(())
    }

    /// Ok(false) when a full reload is needed instead.
    fn catch_up(&mut self, conn: &Connection) -> Result<bool> {
        let tx = conn.unchecked_transaction()?;
        let end = log_end(&tx)?;
        if end == self.seq {
            self.stamp = stamp(&tx)?;
            return Ok(true);
        }
        let first: Option<i64> = tx.query_row(
            "SELECT min(seq) FROM chunk_log WHERE seq > ?1",
            [self.seq],
            |r| r.get(0),
        )?;
        // Pruned past us (or a database from before the log, or rebuilt).
        if end < self.seq || first != Some(self.seq + 1) {
            return Ok(false);
        }
        let touched: HashSet<i64> = {
            let mut stmt = tx.prepare("SELECT chunk_id FROM chunk_log WHERE seq > ?1")?;
            stmt.query_map([self.seq], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        if touched.len() > self.ids.len() / 2 {
            return Ok(false);
        }
        // Whatever happened to an id in between, its row now (or its
        // absence) is what counts: drop the old vector, read the current one.
        for id in &touched {
            self.remove(*id);
        }
        let mut stmt = tx.prepare("SELECT embedding, kind FROM chunks WHERE id = ?1")?;
        let mut added = Vec::new();
        for id in &touched {
            let mut rows = stmt.query([id])?;
            if let Some(row) = rows.next()? {
                added.push((*id, row.get::<_, Vec<u8>>(0)?, Kind::from_db(row.get(1)?)));
            }
        }
        // Exact, so a few new chunks do not double a 100 MB buffer.
        self.ids.reserve_exact(added.len());
        self.scales.reserve_exact(added.len());
        self.kinds.reserve_exact(added.len());
        self.data.reserve_exact(added.len() * DIM);
        for (id, blob, kind) in &added {
            self.push(*id, blob, *kind);
        }
        self.seq = end;
        self.stamp = stamp(&tx)?;
        Ok(true)
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

/// Cosine against every chunk of `kind` (embeddings are unit length), best `k`.
pub fn search(
    conn: &Connection,
    vectors: &Vectors,
    query: &[f32],
    kind: Kind,
    k: usize,
) -> Result<Vec<Hit>> {
    let query: &[f32; DIM] = query.try_into().context("query is not DIM long")?;
    let (q, qs) = quantize(query);
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
    scored.sort_by(by_score);
    scored.truncate(k);
    let mut stmt = conn.prepare(
        "SELECT f.id, f.path, f.sha256, c.start_line, c.text
         FROM chunks c JOIN files f ON f.id = c.file_id WHERE c.id = ?1",
    )?;
    let mut copies =
        conn.prepare("SELECT path FROM files WHERE sha256 = ?1 AND id != ?2 ORDER BY path")?;
    let mut hits = Vec::with_capacity(scored.len());
    for (score, id) in scored {
        // Gone since the vectors were loaded.
        let Ok((file, path, hash, line, text)) = stmt.query_row([id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        }) else {
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
        assert!(is_indexed(&conn, "/a_b/z.md").unwrap());
        assert!(!is_indexed(&conn, "/a_b").unwrap());
        assert!(!is_indexed(&conn, "/c.md").unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copies_share_chunks_and_inherit_them() {
        let dir = std::env::temp_dir().join(format!("vinosearch-dup-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        let file = |path: &str, hash: &[u8]| {
            conn.execute(
                "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, 0, 0, ?2)",
                params![path, hash],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let a = file("/a/x.md", b"same");
        let mut v = vec![0f32; DIM];
        v[0] = 1.0;
        conn.execute(
            "INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (?1, 1, 'hi', ?2)",
            params![a, to_blob(&v)],
        )
        .unwrap();
        let b = file("/b/x.md", b"same");
        file("/c/y.md", b"other");
        assert_eq!(holder_of(&conn, b"same", "/b/x.md").unwrap(), Some(a));
        assert_eq!(holder_of(&conn, b"same", "/a/x.md").unwrap(), None);

        // One hit for the content, naming the copy.
        let hits = search(&conn, &Vectors::load(&conn).unwrap(), &v, Kind::Text, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/a/x.md");
        assert_eq!(hits[0].copies, ["/b/x.md"]);

        // The holder goes: the copy takes its chunks, same ids.
        remove(&conn, Path::new("/a")).unwrap();
        let owner: i64 = conn
            .query_row("SELECT file_id FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(owner, b);
        let hits = search(&conn, &Vectors::load(&conn).unwrap(), &v, Kind::Text, 5).unwrap();
        assert_eq!(
            (hits[0].path.as_str(), hits[0].copies.len()),
            ("/b/x.md", 0)
        );

        // The last copy goes: so do the chunks.
        remove(&conn, Path::new("/b/x.md")).unwrap();
        assert_eq!(counts(&conn).unwrap(), (1, 0));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_old_index_gains_the_hash_and_kind_columns() {
        let dir = std::env::temp_dir().join(format!("vinosearch-mig-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("i.sqlite");
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                 mtime INTEGER NOT NULL, size INTEGER NOT NULL);
                 INSERT INTO files (path, mtime, size) VALUES ('/old.md', 1, 2);
                 CREATE TABLE chunks (id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL,
                 start_line INTEGER NOT NULL, text TEXT NOT NULL, embedding BLOB NOT NULL);
                 INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (1, 1, 'x', x'');",
            )
            .unwrap();
        let conn = open(&db).unwrap();
        let hash: Option<Vec<u8>> = conn
            .query_row("SELECT sha256 FROM files WHERE path = '/old.md'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(hash, None);
        let kind: i64 = conn
            .query_row("SELECT kind FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, Kind::Text as i64);
        drop(conn);
        open(&db).unwrap(); // and opening it again is a no-op
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn forget_keeps_files_another_root_covers() {
        let dir = std::env::temp_dir().join(format!("vinosearch-forget-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        let files = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT path FROM files ORDER BY path")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let setup = |conn: &Connection| {
            conn.execute_batch("DELETE FROM files; DELETE FROM roots;")
                .unwrap();
            for r in ["/dev", "/dev/proj", "/dev/.hidden/p2"] {
                conn.execute("INSERT INTO roots (path) VALUES (?1)", [r])
                    .unwrap();
            }
            for p in ["/dev/a.md", "/dev/proj/b.md", "/dev/.hidden/p2/c.md"] {
                conn.execute(
                    "INSERT INTO files (path, mtime, size) VALUES (?1, 0, 0)",
                    [p],
                )
                .unwrap();
            }
        };

        // The inner root: /dev still walks proj/b.md.
        setup(&conn);
        assert_eq!(forget(&conn, Path::new("/dev/proj")).unwrap(), 0);
        assert_eq!(
            files(&conn),
            ["/dev/.hidden/p2/c.md", "/dev/a.md", "/dev/proj/b.md"]
        );

        // The outer root: proj and the hidden root stay, as their own roots.
        setup(&conn);
        assert_eq!(forget(&conn, Path::new("/dev")).unwrap(), 1);
        assert_eq!(files(&conn), ["/dev/.hidden/p2/c.md", "/dev/proj/b.md"]);

        // A root under a hidden directory, which /dev's walk skips: all of it goes.
        setup(&conn);
        assert_eq!(forget(&conn, Path::new("/dev/.hidden/p2")).unwrap(), 1);
        assert_eq!(files(&conn), ["/dev/a.md", "/dev/proj/b.md"]);

        // No other root overlaps: everything under it goes.
        conn.execute_batch("DELETE FROM roots WHERE path != '/dev/proj'")
            .unwrap();
        assert_eq!(forget(&conn, Path::new("/dev/proj")).unwrap(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn largest_folders_name_where_the_bulk_is() {
        let dir = std::env::temp_dir().join(format!("vinosearch-largest-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        conn.execute_batch("INSERT INTO roots VALUES ('/h'), ('/h/dev/app');")
            .unwrap();
        let mut v = vec![0f32; DIM];
        v[0] = 1.0;
        for (path, n) in [
            // Almost all of linux/ is drivers/gpu: that is what to show.
            ("/h/dev/linux/drivers/gpu/a.c", 40),
            ("/h/dev/linux/drivers/gpu/b.c", 50),
            ("/h/dev/linux/README", 5),
            ("/h/notes/x.md", 20),
            ("/h/notes/y.md", 10),
            // A root inside a root: its folders count, the root itself doesn't.
            ("/h/dev/app/src/main.rs", 30),
            ("/h/top.md", 99),
        ] {
            conn.execute(
                "INSERT INTO files (path, mtime, size) VALUES (?1, 0, 0)",
                [path],
            )
            .unwrap();
            let file = conn.last_insert_rowid();
            for _ in 0..n {
                conn.execute(
                    "INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (?1, 1, '', ?2)",
                    params![file, to_blob(&v)],
                )
                .unwrap();
            }
        }
        let got = largest_folders(&conn, 10).unwrap();
        let got: Vec<(&str, u64)> = got.iter().map(|(p, n)| (p.to_str().unwrap(), *n)).collect();
        assert_eq!(
            got,
            [
                ("/h/dev/linux/drivers/gpu", 90),
                ("/h/dev/app/src", 30),
                ("/h/notes", 30)
            ]
        );
        assert_eq!(largest_folders(&conn, 1).unwrap().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_files_are_not_indexed() {
        for f in ["a.toml", "b.yaml", "c.json", "d.ini", "e.conf", ".env"] {
            assert!(!text_file(Path::new(f)), "{f}");
        }
        assert!(text_file(Path::new("notes.md")));
    }

    #[test]
    fn walk_does_not_follow_symlinks() {
        let dir = std::env::temp_dir().join(format!("vinosearch-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("root/real")).unwrap();
        fs::create_dir_all(dir.join("secret")).unwrap();
        fs::write(dir.join("root/real/a.md"), "a").unwrap();
        fs::write(dir.join("secret/tax.md"), "s").unwrap();
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("root/link")).unwrap();
        let files = |p: &Path| -> Vec<PathBuf> {
            walk(p, &Excludes::default())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .collect()
        };
        assert_eq!(files(&dir.join("root")), [dir.join("root/real/a.md")]);
        // The watcher walks a created path directly, so the link is the root.
        assert!(files(&dir.join("root/link")).is_empty());
        assert!(!is_real_dir(&dir.join("root/link")));
        assert!(is_real_dir(&dir.join("root/real")));
        fs::remove_dir_all(&dir).unwrap();
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
            "miniconda3",
        ] {
            assert!(skipped_dir(d), "{d}");
        }
        for d in ["src", "bin", "out", "docs", "covered"] {
            assert!(!skipped_dir(d), "{d}");
        }
        assert!(!text_file(Path::new("app.min.js")));
        assert!(!text_file(Path::new("vendor.bundle.js")));
        assert!(text_file(Path::new("app.js")));
        assert!(text_file(Path::new("README.md")));
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
        let mut got: Vec<String> = stale(&conn, &root, &Excludes::default(), &FileTypes::default())
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
        let dir = std::env::temp_dir().join(format!("vinosearch-search-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        conn.execute(
            "INSERT INTO files (id, path, mtime, size) VALUES (1, '/f', 0, 0)",
            [],
        )
        .unwrap();
        // Chunk i points along axis 0 with weight i/10, so higher i scores higher.
        for i in 0..10 {
            let mut v = vec![0f32; DIM];
            v[0] = i as f32 / 10.0;
            conn.execute(
                "INSERT INTO chunks (id, file_id, start_line, text, embedding) VALUES (?1, 1, ?1, '', ?2)",
                params![i + 1, to_blob(&v)],
            )
            .unwrap();
        }
        let vectors = Vectors::load(&conn).unwrap();
        let mut q = vec![0f32; DIM];
        q[0] = 1.0;
        let lines: Vec<i64> = search(&conn, &vectors, &q, Kind::Text, 3)
            .unwrap()
            .iter()
            .map(|h| h.line)
            .collect();
        assert_eq!(lines, [10, 9, 8]);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn images_on() -> FileTypes {
        let mut f = Formats::default();
        f.images.enabled = Some(true);
        FileTypes::new(&f).unwrap()
    }

    #[test]
    fn images_are_wanted_only_while_on() {
        let off = FileTypes::default();
        assert_eq!(off.kind(Path::new("/p/a.JPG")), None);
        assert_eq!(off.kind(Path::new("/p/a.md")), Some(Kind::Text));
        let on = images_on();
        assert_eq!(on.kind(Path::new("/p/a.JPG")), Some(Kind::Image));
        assert_eq!(on.kind(Path::new("/p/a.webp")), Some(Kind::Image));
        assert_eq!(on.kind(Path::new("/p/a.md")), Some(Kind::Text));
        assert_eq!(on.kind(Path::new("/p/a.xcf")), None);
    }

    #[test]
    fn kinds_are_searched_apart_and_purged_when_turned_off() {
        let dir = std::env::temp_dir().join(format!("vinosearch-kinds-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        add_file(&conn, "/notes.md", &[0, 1]);
        add_file(&conn, "/cat.png", &[0]);
        conn.execute("UPDATE chunks SET kind = 1, start_line = 0 WHERE file_id = (SELECT id FROM files WHERE path = '/cat.png')", []).unwrap();
        let mut v = Vectors::load(&conn).unwrap();
        let mut q = vec![0f32; DIM];
        q[0] = 1.0;
        let paths = |v: &Vectors, kind| -> Vec<String> {
            search(&conn, v, &q, kind, 5)
                .unwrap()
                .into_iter()
                .map(|h| h.path)
                .collect()
        };
        assert_eq!(paths(&v, Kind::Image), ["/cat.png"]);
        assert_eq!(paths(&v, Kind::Text), ["/notes.md", "/notes.md"]);
        // Kinds stay with their vectors as rows are swapped out on removal.
        remove(&conn, Path::new("/notes.md")).unwrap();
        add_file(&conn, "/b.md", &[0]);
        v.refresh(&conn).unwrap();
        assert_eq!(paths(&v, Kind::Image), ["/cat.png"]);
        assert_eq!(paths(&v, Kind::Text), ["/b.md"]);

        assert_eq!(purge_types(&conn, &images_on()).unwrap(), 0);
        assert_eq!(purge_types(&conn, &FileTypes::default()).unwrap(), 1);
        v.refresh(&conn).unwrap();
        assert!(paths(&v, Kind::Image).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A unit vector along `axis`, as an embedding blob.
    fn axis(axis: usize) -> Vec<u8> {
        let mut v = vec![0f32; DIM];
        v[axis] = 1.0;
        to_blob(&v)
    }

    fn add_file(conn: &Connection, path: &str, axes: &[usize]) -> Vec<i64> {
        conn.execute(
            "INSERT INTO files (path, mtime, size) VALUES (?1, 0, 0)",
            [path],
        )
        .unwrap();
        let file = conn.last_insert_rowid();
        axes.iter()
            .map(|a| {
                conn.execute(
                    "INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (?1, 1, '', ?2)",
                    params![file, axis(*a)],
                )
                .unwrap();
                conn.last_insert_rowid()
            })
            .collect()
    }

    /// What refresh produced, compared with a fresh load: (id, best axis) pairs.
    fn contents(v: &Vectors) -> Vec<(i64, usize)> {
        let mut out: Vec<_> = v
            .ids
            .iter()
            .zip(v.data.as_chunks::<DIM>().0)
            .map(|(id, d)| (*id, d.iter().position(|x| *x == 127).unwrap()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn refresh_applies_only_what_changed() {
        let dir = std::env::temp_dir().join(format!("vinosearch-refresh-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        // A second connection writes, as `vinosearch index` or the watcher would.
        let writer = open(&dir.join("i.sqlite")).unwrap();
        add_file(&writer, "/base.md", &(0..20).collect::<Vec<_>>());
        add_file(&writer, "/a.md", &[20, 21, 22]);
        let mut v = Vectors::load(&conn).unwrap();
        assert_eq!(v.len(), 23);

        // Incremental, and the same as loading from scratch.
        let caught_up = |v: &mut Vectors| {
            assert!(v.catch_up(&conn).unwrap(), "fell back to a full reload");
            assert_eq!(contents(v), contents(&Vectors::load(&conn).unwrap()));
        };
        // New chunks, and a file replaced (delete cascades to its chunks).
        add_file(&writer, "/c.md", &[23]);
        remove(&writer, Path::new("/a.md")).unwrap();
        add_file(&writer, "/a.md", &[24, 25]);
        caught_up(&mut v);
        assert_eq!(v.len(), 23);
        // The highest id deleted, then reused for a different vector: SQLite
        // hands out max(id) + 1, so the new chunk gets the old one's id.
        let max: i64 = writer
            .query_row("SELECT max(id) FROM chunks", [], |r| r.get(0))
            .unwrap();
        remove(&writer, Path::new("/a.md")).unwrap();
        assert_eq!(add_file(&writer, "/d.md", &[26, 27]).last(), Some(&max));
        caught_up(&mut v);
        // Nothing changed.
        caught_up(&mut v);

        // The log pruned past this copy: refresh reloads in full.
        add_file(&writer, "/e.md", &[28]);
        writer.execute("DELETE FROM chunk_log", []).unwrap();
        assert!(!v.catch_up(&conn).unwrap());
        v.refresh(&conn).unwrap();
        assert_eq!(contents(&v), contents(&Vectors::load(&conn).unwrap()));
        // Most of the index changed: also a full reload.
        remove(&writer, Path::new("/base.md")).unwrap();
        assert!(!v.catch_up(&conn).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }
}
