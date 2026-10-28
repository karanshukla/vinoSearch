//! Files -> chunks -> embeddings in SQLite, and brute-force search over them.
//! A text file is many chunks; an image is one, embedded from its pixels.
//!
//! Embeddings can be partly inverted back to their text, so the index is as
//! sensitive as the files it covers: encrypted at rest when `encryption` is
//! on (encryption.rs, #4), plaintext readable only by the user otherwise.

use crate::config::{Excludes, Formats};
use crate::embed::{DIM, Embed, Embedder, document_text};
use crate::extract::{Extract, MAX_PARTS, PARTS_AT_ONCE, Parser};
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
pub(crate) const CHUNK_CHARS: usize = 900;
pub(crate) const MAX_FILE_BYTES: u64 = 1 << 20;
/// A document's text, or a prose file's, all its parts together (#108):
/// about 5,000 printed pages, ~18,000 chunks.
pub(crate) const MAX_TEXT_BYTES: u64 = 16 << 20;
/// Text files that may be books, held to MAX_TEXT_BYTES instead of
/// MAX_FILE_BYTES.
const PROSE: &[&str] = &["txt", "md", "rst", "org", "tex"];
/// Prose and markup. Code is a kind of its own, in `code::EXTENSIONS` (#110).
pub(crate) const TEXT_EXTENSIONS: &[&str] = &["md", "txt", "rst", "org", "tex", "html", "htm"];
// No config formats (toml, yaml, json, ini, conf, ...): they are where
// credentials live, and the index is plaintext unless encryption is on (#4).
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

use vinosearch_shared::limits::MAX_IMAGE_BYTES;
/// Documents are parsed whole in memory too, once for each run of parts.
/// Their extracted text is held to MAX_TEXT_BYTES.
pub(crate) const MAX_DOCUMENT_BYTES: u64 = 32 << 20;

/// What a chunk was embedded from, stored in `chunks.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text = 0,
    Image = 1,
    /// Text extracted from a PDF, Office file or spreadsheet (#55), or read
    /// from an image or scanned page by OCR (#106). Scored like text, but
    /// searched apart so neither crowds the other out.
    Document = 2,
    /// Source code (#110): on only when asked for, and searched only by
    /// queries starting `code:`.
    Code = 3,
    /// A vinoWhisper session's transcript and summary (#143), chunks of one
    /// `.vwsession` file; `extract::session::Part::of` says which.
    Session = 4,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Text,
        Kind::Document,
        Kind::Image,
        Kind::Code,
        Kind::Session,
    ];

    fn from_db(v: i64) -> Self {
        match v {
            1 => Kind::Image,
            2 => Kind::Document,
            3 => Kind::Code,
            4 => Kind::Session,
            _ => Kind::Text,
        }
    }

    /// The largest `path` of this kind that is indexed.
    fn max_bytes(self, path: &Path) -> u64 {
        match self {
            Kind::Text if is_prose(path) => MAX_TEXT_BYTES,
            Kind::Text | Kind::Code => MAX_FILE_BYTES,
            Kind::Image => MAX_IMAGE_BYTES,
            Kind::Document | Kind::Session => MAX_DOCUMENT_BYTES,
        }
    }

    /// What made this kind's vectors, as `meta` records it (#80): the text
    /// model's revision, and for images the vision encoder's, the soft-token
    /// count and the OCR model's, which reads their text (#106). Vectors with another tag score against this
    /// build's queries without erroring, just wrongly, so they are dropped.
    pub fn model_tag(self) -> String {
        match self {
            Kind::Text | Kind::Document | Kind::Code | Kind::Session => {
                crate::model::REVISION.to_owned()
            }
            Kind::Image => format!(
                "{}+{}/{}+ocr:{}/{}",
                crate::model::REVISION,
                crate::model::VISION_REVISION,
                crate::vision::SOFT_TOKENS,
                crate::model::OCR_REVISION,
                crate::ocr::READING,
            ),
        }
    }

    fn meta_key(self) -> &'static str {
        match self {
            Kind::Text => "model.text",
            Kind::Document => "model.document",
            Kind::Image => "model.image",
            Kind::Code => "model.code",
            Kind::Session => "model.session",
        }
    }
}

/// The file types indexed: text always, images by extension, documents,
/// code and vinoWhisper sessions while they are on (#73, #55, #110, #143).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileTypes {
    images: Vec<String>,
    documents: bool,
    code: bool,
    sessions: bool,
    /// vinoWhisper's own folder of sessions, where nothing else is taken.
    sessions_dir: Option<PathBuf>,
    ignores: SharedIgnores,
}

/// The ignore files read, shared by a `FileTypes` and its clones; no part
/// of what makes two equal.
#[derive(Debug, Clone, Default)]
struct SharedIgnores(std::sync::Arc<crate::code::Ignores>);

impl PartialEq for SharedIgnores {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl FileTypes {
    pub fn new(formats: &Formats) -> Result<Self> {
        Ok(Self {
            images: formats.image_extensions()?,
            documents: formats.documents_enabled(),
            code: formats.code_enabled(),
            sessions: formats.sessions_enabled(),
            sessions_dir: sessions_dir(),
            ignores: SharedIgnores::default(),
        })
    }

    #[cfg(test)]
    pub fn with_sessions(mut self, on: bool, dir: Option<PathBuf>) -> Self {
        self.sessions = on;
        self.sessions_dir = dir;
        self
    }

    /// vinoWhisper's folder of sessions, and whether sessions are on. While
    /// they are it is a root of its own: the walk of a home folder skips
    /// hidden directories.
    pub fn sessions_dir(&self) -> Option<(&Path, bool)> {
        Some((self.sessions_dir.as_deref()?, self.sessions))
    }

    /// The kind `path` is indexed as, by its name (and for code, the ignore
    /// files above it); None if it is not.
    pub fn kind(&self, path: &Path) -> Option<Kind> {
        let session = crate::extract::is_session(path);
        // A live transcript or a half-written bundle there is not wanted.
        if self
            .sessions_dir
            .as_ref()
            .is_some_and(|d| path.starts_with(d))
            && !session
        {
            return None;
        }
        if session {
            return self.sessions.then_some(Kind::Session);
        }
        if text_file(path) {
            return Some(Kind::Text);
        }
        if crate::code::is_code(path) {
            return (self.code && !self.ignores.0.left_out(path)).then_some(Kind::Code);
        }
        if self.documents && crate::extract::is_document(path) {
            return Some(Kind::Document);
        }
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.images.contains(&ext).then_some(Kind::Image)
    }
}

/// Where vinoWhisper keeps its sessions (karanshukla/vinoWhisper#123),
/// canonical as roots are.
fn sessions_dir() -> Option<PathBuf> {
    let dir = data_home().ok()?.join("vinowhisper/transcripts");
    Some(fs::canonicalize(&dir).unwrap_or(dir))
}

fn data_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".local/share"),
    })
}

/// Log entries kept. A Vectors further behind than this reloads in full,
/// which at that many changes is no slower than applying them.
const CHUNK_LOG_KEEP: i64 = 100_000;

pub fn default_db() -> Result<PathBuf> {
    Ok(data_home()?.join("vinosearch/index.sqlite"))
}

/// The index holds copies of file contents, so only the user may read it:
/// the directory 0700, the database 0600 (SQLite gives its -wal and -shm
/// files the database's mode). Keyed with its sealed key when it has one; a
/// key that won't unseal is an error (`encryption::Unusable`), never an
/// empty or plaintext index.
pub fn open(path: &Path) -> Result<Connection> {
    let key = crate::encryption::key_for(path)?;
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
    crate::encryption::unlock(&conn, key.as_deref())?;
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
         BEGIN DELETE FROM chunk_log WHERE seq <= NEW.seq - {CHUNK_LOG_KEEP}; END;
         -- Facts about the index as a whole: which model made each kind's
         -- vectors (#80), under model.<kind>.
         CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);"
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
         END;
         -- Long documents indexed in part (#108): `done` of their `total`
         -- parts have chunks, the rest wait for `index_rest`. Gone once the
         -- last part is in, and with the file. A copy takes it over with
         -- the chunks.
         CREATE TABLE IF NOT EXISTS partial (
             file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
             done INTEGER NOT NULL,
             total INTEGER NOT NULL
         );
         CREATE TRIGGER IF NOT EXISTS partial_hand_over BEFORE DELETE ON files
         WHEN OLD.sha256 IS NOT NULL
         BEGIN
             UPDATE partial
             SET file_id = (SELECT id FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id LIMIT 1)
             WHERE file_id = OLD.id
               AND EXISTS (SELECT 1 FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id);
         END;",
    )?;
    words_table(&conn)?;
    code_apart(&conn)?;
    Ok(conn)
}

/// Code indexed as text before it was a kind of its own (#110), moved over
/// once: relabelled, not embedded again, since the model and the text it
/// embedded are the same. What indexing code leaves out now goes, though:
/// files git ignores or named as generated go at the next check of their
/// root, as anything no longer wanted does; those holding a key or token,
/// or generated or minified by their content, go here, since nothing reads
/// an unchanged file again. With code off (the default) the rest go at the
/// next `purge_types`, as images do when turned off.
fn code_apart(conn: &Connection) -> Result<()> {
    const KEY: &str = "kind.code";
    let done = |c: &Connection| -> rusqlite::Result<bool> {
        c.query_row("SELECT count(*) FROM meta WHERE key = ?1", [KEY], |r| {
            r.get(0)
        })
    };
    if done(conn)? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    if done(&tx)? {
        return Ok(());
    }
    // Files with text chunks, of a code extension.
    let mut files: Vec<(i64, String)> = tx
        .prepare(
            "SELECT DISTINCT f.id, f.path FROM files f JOIN chunks c ON c.file_id = f.id
             WHERE c.kind = ?1",
        )?
        .query_map([Kind::Text as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    files.retain(|(_, p)| crate::code::is_code(Path::new(p)));
    let mut text = tx.prepare("SELECT text FROM chunks WHERE file_id = ?1 ORDER BY id")?;
    let mut relabel = tx.prepare("UPDATE chunks SET kind = ?1 WHERE file_id = ?2")?;
    // So loaded Vectors read these chunks' new kind (`Vectors::catch_up`).
    let mut log =
        tx.prepare("INSERT INTO chunk_log (chunk_id) SELECT id FROM chunks WHERE file_id = ?1")?;
    let mut dropped = Vec::new();
    for (id, path) in &files {
        let all: String = text
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        if let Some(why) = crate::code::unwanted(&all) {
            eprintln!("dropping {path} from the index: {why}");
            dropped.push(*id);
            continue;
        }
        relabel.execute(params![Kind::Code as i64, id])?;
        log.execute([id])?;
    }
    drop((text, relabel, log));
    for id in &dropped {
        // Its chunks first, so they aren't handed to a copy, then the copies.
        tx.execute("DELETE FROM chunks WHERE file_id = ?1", [id])?;
        tx.execute(
            "DELETE FROM files WHERE sha256 = (SELECT sha256 FROM files WHERE id = ?1) OR id = ?1",
            [id],
        )?;
    }
    tx.execute("INSERT INTO meta (key, value) VALUES (?1, '1')", [KEY])?;
    tx.commit()?;
    if !files.is_empty() {
        eprintln!(
            "{} code files are a kind of their own now (#110): searched with code: while code is on, dropped while it's off",
            files.len() - dropped.len()
        );
    }
    Ok(())
}

/// A chunk's file name, from `files f`: what follows the last `/`.
const FILE_NAME: &str = "substr(f.path, length(rtrim(f.path, replace(f.path, '/', ''))) + 1)";

/// The word index (#104): FTS5 over each chunk's text and its file's name,
/// rowid = chunk id, for the exact strings embeddings lose (invoice numbers,
/// error codes, names). Contentless, so the text is not stored a second time;
/// `contentless_delete` lets the triggers drop a row by its id alone. A chunk
/// handed to a copy (files_hand_over) is reindexed under the copy's name. An
/// index from before it is filled once, in the transaction that creates it.
fn words_table(conn: &Connection) -> Result<()> {
    let has = |c: &Connection| -> rusqlite::Result<bool> {
        c.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'chunk_words'",
            [],
            |r| r.get(0),
        )
    };
    // The table and its triggers come into being together, so with one
    // there is nothing to do, and no write lock to wait for.
    if has(conn)? {
        return Ok(());
    }
    // Immediate, so two connections opening an old index at once can't both
    // fill it; the second waits out the first's fill (16 s for 113k chunks
    // in the VM) rather than the usual 5 s.
    conn.busy_timeout(std::time::Duration::from_secs(300))?;
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let had = has(&tx)?;
    tx.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunk_words USING fts5(
             name, text, content = '', contentless_delete = 1,
             tokenize = 'unicode61 remove_diacritics 2'
         );
         CREATE TRIGGER IF NOT EXISTS chunk_words_insert AFTER INSERT ON chunks
         BEGIN
             INSERT INTO chunk_words (rowid, name, text)
             SELECT NEW.id, {FILE_NAME}, NEW.text FROM files f WHERE f.id = NEW.file_id;
         END;
         CREATE TRIGGER IF NOT EXISTS chunk_words_delete AFTER DELETE ON chunks
         BEGIN DELETE FROM chunk_words WHERE rowid = OLD.id; END;
         CREATE TRIGGER IF NOT EXISTS chunk_words_update AFTER UPDATE OF file_id, text ON chunks
         BEGIN
             DELETE FROM chunk_words WHERE rowid = OLD.id;
             INSERT INTO chunk_words (rowid, name, text)
             SELECT NEW.id, {FILE_NAME}, NEW.text FROM files f WHERE f.id = NEW.file_id;
         END;"
    ))?;
    if !had {
        tx.execute(
            &format!(
                "INSERT INTO chunk_words (rowid, name, text)
                 SELECT c.id, {FILE_NAME}, c.text FROM chunks c JOIN files f ON f.id = c.file_id"
            ),
            [],
        )?;
    }
    tx.commit()?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// Whether `path` is indexed, as `types` takes it.
pub(crate) fn wanted(path: &Path, types: &FileTypes) -> bool {
    types.kind(path).is_some()
}

fn is_prose(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PROSE.contains(&e.to_ascii_lowercase().as_str()))
}

fn text_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| TEXT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

pub(crate) fn skipped_dir(name: &str) -> bool {
    name.starts_with('.')
        || SKIP_DIRS.contains(&name)
        || SKIP_DIR_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Line-aligned chunks of up to CHUNK_CHARS, with the line each starts on.
/// A single longer line is split between words, or failing that on char
/// boundaries.
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
            // Between words if there is a space in the back half: a PDF
            // page's text layer is often a few very long lines.
            if let Some(space) = rest[..cut].rfind(' ').filter(|&i| i >= CHUNK_CHARS / 2) {
                cut = space + 1;
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

#[derive(Default)]
pub struct Stats {
    pub files_seen: usize,
    pub files_embedded: usize,
    pub chunks: usize,
    pub removed: usize,
    /// Files whose content was already indexed, so nothing was embedded.
    pub duplicates: usize,
    /// Images skipped for want of a decoder this build lacks (HEIF, #79).
    pub no_decoder: usize,
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
    /// Recorded with no chunks: an image that doesn't decode, so it isn't
    /// tried again until it changes. `no_decoder` when a decoder this build
    /// lacks would have read it (#79).
    Unreadable {
        no_decoder: bool,
    },
    /// Not a wanted file (type, size, encoding); any old entry for it is dropped.
    Skipped,
}

/// Index one file if it is wanted and changed since it was last indexed.
/// A long file gets its first PARTS_AT_ONCE parts (pages, say) now and the
/// rest from `index_rest` once everything else is in (#108). `between` runs
/// after each chunk is embedded, so a waiting query is not held for the
/// whole run. An error from it stops the file unwritten.
pub fn index_file<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    path: &Path,
    types: &FileTypes,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Outcome> {
    let kind = types.kind(path);
    let (meta, kind) = match (fs::symlink_metadata(path), kind) {
        (Ok(m), Some(kind)) if m.is_file() && m.len() <= kind.max_bytes(path) => (m, kind),
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
        Kind::Text | Kind::Code => fs::read_to_string(path).map(String::into_bytes),
        Kind::Image | Kind::Document | Kind::Session => fs::read(path),
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
    // A file indexed with no chunks of its own.
    let record = |conn: &mut Connection| -> Result<()> {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
        tx.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, ?4)",
            params![key, mtime, size, hash],
        )?;
        Ok(tx.commit()?)
    };
    if holder.is_some() {
        record(conn)?;
        return Ok(Outcome::Duplicate);
    }
    let title = title_of(path);
    let (first, vectors, kinds) = match kind {
        Kind::Text | Kind::Code => {
            let text = String::from_utf8(bytes).expect("read as a string");
            // Kept as a row with no chunks, so it isn't read again until it
            // changes.
            if let Some(why) = (kind == Kind::Code)
                .then(|| crate::code::unwanted(&text))
                .flatten()
            {
                eprintln!("skip {}: {why}", path.display());
                record(conn)?;
                return Ok(Outcome::Unreadable { no_decoder: false });
            }
            let first = text_parts(path, &text, 0, PARTS_AT_ONCE);
            let vectors = embed_chunks(conn, emb, &title, &first.chunks, between)?;
            let kinds = vec![kind; first.chunks.len()];
            (first, vectors, kinds)
        }
        // A file that can't be read, or has no text (a scan), keeps a row with
        // no chunks, so it isn't parsed again until it changes. So does a
        // session in a format this build doesn't read.
        Kind::Document | Kind::Session => {
            // Recorded first: the parser runs in a child with limits, but a
            // file the child cannot parse must not be parsed again on every
            // restart, and a stop while embedding must leave it to be indexed
            // next time, so the record goes once it parses.
            record(conn)?;
            let mut first = document_parts(path, &bytes, 0, parser(kind)).unwrap_or_else(|e| {
                eprintln!("skip {}: {e:#}", path.display());
                Extract::default()
            });
            // No text layer: a scan, read by OCR when it can be (#106).
            let scan = first.chunks.is_empty() && is_scannable(path) && emb.can_read_text();
            if !first.chunks.is_empty() || scan {
                conn.execute("DELETE FROM files WHERE path = ?1", [key])?;
            }
            if scan {
                // Read whole, up to ocr::MAX_SCAN_PAGES: never left in part.
                first = Extract {
                    parts: 1,
                    chunks: scan_chunks(conn, emb, path, &bytes, between)?,
                };
            }
            let vectors = embed_chunks(conn, emb, &title, &first.chunks, between)?;
            let kinds = vec![kind; first.chunks.len()];
            (first, vectors, kinds)
        }
        // A chunk with no text and line 0 from the pixels, then the text OCR
        // reads in them as document chunks (#106). One that doesn't decode
        // keeps a row with no chunks, like a document with no text, so it
        // isn't decoded again at every check.
        Kind::Image => {
            // From the bytes hashed, so the file can't change in between.
            let decoded = crate::vision::load(&bytes)
                .and_then(|img| Ok((crate::vision::prepare(&img)?, img)));
            let (pixels, img) = match decoded {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("skip {}: {e:#}", path.display());
                    record(conn)?;
                    return Ok(Outcome::Unreadable {
                        no_decoder: e.downcast_ref::<crate::vision::NoDecoder>().is_some(),
                    });
                }
            };
            let v = emb.image(&pixels)?;
            drop(pixels);
            between(conn, emb)?;
            let lines = emb.read_text(&img)?;
            let lines = if crate::ocr::text_size(&lines) >= crate::ocr::MIN_IMAGE_TEXT {
                lines
            } else {
                Vec::new()
            };
            let text = chunk(&lines.join("\n"));
            drop(img);
            between(conn, emb)?;
            let mut chunks = vec![(0, String::new())];
            let mut vectors = vec![v];
            vectors.extend(embed_chunks(conn, emb, &title, &text, between)?);
            let mut kinds = vec![Kind::Image];
            kinds.extend(std::iter::repeat_n(Kind::Document, text.len()));
            chunks.extend(text);
            (Extract { parts: 1, chunks }, vectors, kinds)
        }
    };

    let tx = conn.transaction()?;
    tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
    tx.execute(
        "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, ?4)",
        params![key, mtime, size, hash],
    )?;
    let file_id = tx.last_insert_rowid();
    insert_chunks(&tx, file_id, &first.chunks, &vectors, &kinds)?;
    // The rest of a long one waits for `index_rest`.
    let total = first.parts.min(MAX_PARTS);
    if total > PARTS_AT_ONCE {
        tx.execute(
            "INSERT INTO partial (file_id, done, total) VALUES (?1, ?2, ?3)",
            params![file_id, PARTS_AT_ONCE as i64, total as i64],
        )?;
    }
    tx.commit()?;
    Ok(Outcome::Embedded(first.chunks.len()))
}

fn insert_chunks(
    tx: &Connection,
    file_id: i64,
    chunks: &[(usize, String)],
    vectors: &[Vec<f32>],
    kinds: &[Kind],
) -> Result<()> {
    for (((line, text), v), kind) in chunks.iter().zip(vectors).zip(kinds) {
        tx.execute(
            "INSERT INTO chunks (file_id, start_line, text, embedding, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![file_id, *line as i64, text, to_blob(v), *kind as i64],
        )?;
    }
    Ok(())
}

/// Each of `chunks` embedded as a document titled `title`, letting `between`
/// run after each.
fn embed_chunks<E: Embed>(
    conn: &Connection,
    emb: &mut E,
    title: &str,
    chunks: &[(usize, String)],
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Vec<Vec<f32>>> {
    let mut vectors = Vec::with_capacity(chunks.len());
    for (_, c) in chunks {
        vectors.extend(emb.documents(&[document_text(title, c)])?);
        between(conn, emb)?;
    }
    Ok(vectors)
}

/// A document with no text layer that OCR can read: a PDF, with poppler there
/// to render its pages.
fn is_scannable(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        && crate::ocr::poppler()
}

/// A scanned PDF's pages as OCR reads them, a chunk a page numbered by page,
/// up to `ocr::MAX_SCAN_PAGES` and within `ocr::SCAN_WALL` of rendering.
/// The first page that won't render ends the scan with the pages read so
/// far: a file that stalls one page likely stalls them all.
fn scan_chunks<E: Embed>(
    conn: &Connection,
    emb: &mut E,
    path: &Path,
    bytes: &[u8],
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Vec<(usize, String)>> {
    // Rendering only: OCR and embedding are the runner's own work, paced by
    // `between`.
    let mut deadline = std::time::Instant::now() + crate::ocr::SCAN_WALL;
    let pages = match crate::ocr::pdf_pages(bytes, deadline) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("skip {}: {e:#}", path.display());
            return Ok(Vec::new());
        }
    };
    if pages > crate::ocr::MAX_SCAN_PAGES {
        eprintln!(
            "{}: reading the first {} of {pages} scanned pages",
            path.display(),
            crate::ocr::MAX_SCAN_PAGES
        );
    }
    let mut out = Vec::new();
    for page in 1..=pages.min(crate::ocr::MAX_SCAN_PAGES) {
        let started = std::time::Instant::now();
        let img = match crate::ocr::render_page(bytes, page, deadline) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("{} page {page}: {e:#}; stopping there", path.display());
                break;
            }
        };
        let rendered = started.elapsed();
        let text = emb.read_text(&img)?.join("\n");
        between(conn, emb)?;
        // The deadline counts poppler's time, not the OCR's or a query's.
        deadline += started.elapsed() - rendered;
        if !text.trim().is_empty() {
            out.push((page, text));
        }
    }
    Ok(out)
}

/// The name a file's chunks are embedded under.
fn title_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Parts `from..from + count` of a text file's chunks.
fn text_parts(path: &Path, text: &str, from: usize, count: usize) -> Extract {
    let chunks = if crate::extract::is_html(path) {
        chunk(&crate::extract::html(text))
    } else if crate::code::is_code(path) {
        crate::code::chunk(text)
    } else {
        chunk(text)
    };
    crate::extract::select(chunks, false, from, count)
}

/// Parts `from..from + PARTS_AT_ONCE` of `path` through `parse` (the child
/// process, #84). Err if it doesn't parse, the child was stopped, or the
/// run's text is over MAX_FILE_BYTES.
/// Documents are parsed in a child with limits (#84). A session is plain
/// text in a zip, nothing in it needs a parser that could crash or hang,
/// and each part it unzips is held to 1 MiB, so it's read here.
fn parser(kind: Kind) -> Parser {
    match kind {
        Kind::Session => crate::extract::parts,
        _ => crate::extract::parts_in_child,
    }
}

fn document_parts(path: &Path, bytes: &[u8], from: usize, parse: Parser) -> Result<Extract> {
    let run = parse(path, bytes, from, PARTS_AT_ONCE)?;
    let size: usize = run.chunks.iter().map(|(_, c)| c.len()).sum();
    if size as u64 > MAX_FILE_BYTES {
        bail!("{size} bytes of text in {PARTS_AT_ONCE} parts");
    }
    Ok(run)
}

/// What `index_rest` does next.
#[derive(Debug, PartialEq)]
enum Next {
    /// The parts of file `id` after the first `from`, of `total`.
    Parts {
        id: i64,
        path: String,
        from: usize,
        total: usize,
    },
    /// The file changed or went since its first run: index it afresh.
    Again(String),
}

/// The long document indexed in part longest ago, and what to do with it.
fn next_rest(conn: &Connection) -> Result<Option<Next>> {
    let row: Option<(i64, String, i64, i64, i64, i64)> = conn
        .query_row(
            "SELECT f.id, f.path, f.mtime, f.size, p.done, p.total
             FROM partial p JOIN files f ON f.id = p.file_id
             ORDER BY p.file_id LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((id, path, mtime, size, done, total)) = row else {
        return Ok(None);
    };
    let now = fs::symlink_metadata(&path)
        .ok()
        .filter(|m| m.is_file())
        .as_ref()
        .and_then(stamp_of);
    if now != Some((mtime, size)) {
        return Ok(Some(Next::Again(path)));
    }
    Ok(Some(Next::Parts {
        id,
        path,
        from: done as usize,
        total: total as usize,
    }))
}

/// Store the chunks of parts `from..done` of file `id`, and move its partial
/// row on to `done`, or drop it once that is all `total`.
fn store_rest(
    conn: &mut Connection,
    id: i64,
    kind: Kind,
    done: usize,
    total: usize,
    chunks: &[(usize, String)],
    vectors: &[Vec<f32>],
) -> Result<()> {
    let tx = conn.transaction()?;
    insert_chunks(&tx, id, chunks, vectors, &vec![kind; chunks.len()])?;
    if done >= total {
        tx.execute("DELETE FROM partial WHERE file_id = ?1", [id])?;
    } else {
        tx.execute(
            "UPDATE partial SET done = ?1 WHERE file_id = ?2",
            params![done as i64, id],
        )?;
    }
    Ok(tx.commit()?)
}

/// What one `index_rest` call did.
#[derive(Debug)]
pub enum Rest {
    /// `path` has `done` of its `total` parts in now, `chunks` of them just
    /// embedded. Stopped short when a run would not parse: `done` stays.
    Parts {
        path: String,
        done: usize,
        total: usize,
        chunks: usize,
    },
    /// The file had changed or gone, and was indexed afresh (its first run)
    /// or dropped.
    Again(String),
}

/// The next run of parts of the long document indexed in part longest ago
/// (#108), or None when every document is in whole. The parser's limits
/// apply to the run, as in `index_file`, and `between` after each chunk.
pub fn index_rest<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    types: &FileTypes,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Option<Rest>> {
    let Some(next) = next_rest(conn)? else {
        return Ok(None);
    };
    let again = |conn: &mut Connection,
                 emb: &mut E,
                 path: String,
                 between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>|
     -> Result<Option<Rest>> {
        // Whatever the stamp says, index_file must take it as changed.
        conn.execute("UPDATE files SET mtime = -1 WHERE path = ?1", [&path])?;
        index_file(conn, emb, Path::new(&path), types, between)?;
        Ok(Some(Rest::Again(path)))
    };
    let (id, key, from, total) = match next {
        Next::Again(path) => return again(conn, emb, path, between),
        Next::Parts {
            id,
            path,
            from,
            total,
        } => (id, path, from, total),
    };
    let path = Path::new(&key);
    let hash: Option<Vec<u8>> =
        conn.query_row("SELECT sha256 FROM files WHERE id = ?1", [id], |r| r.get(0))?;
    let (kind, bytes) = match (types.kind(path), fs::read(path)) {
        (Some(kind @ (Kind::Text | Kind::Document | Kind::Code | Kind::Session)), Ok(bytes))
            if hash.as_deref() == Some(&Sha256::digest(&bytes)[..]) =>
        {
            (kind, bytes)
        }
        _ => return again(conn, emb, key, between),
    };
    let run = match kind {
        Kind::Document | Kind::Session => document_parts(path, &bytes, from, parser(kind)),
        _ => match String::from_utf8(bytes) {
            Ok(text) => Ok(text_parts(path, &text, from, PARTS_AT_ONCE)),
            Err(_) => return again(conn, emb, key, between),
        },
    };
    let mut done = (from + PARTS_AT_ONCE).min(total);
    let mut chunks = match run {
        Ok(run) => run.chunks,
        // What is in stays; the rest isn't tried again until it changes.
        Err(e) => {
            eprintln!(
                "{}: stopped at part {from} of {total}: {e:#}",
                path.display()
            );
            conn.execute("DELETE FROM partial WHERE file_id = ?1", [id])?;
            return Ok(Some(Rest::Parts {
                path: key,
                done: from,
                total,
                chunks: 0,
            }));
        }
    };
    let have: i64 = conn.query_row(
        "SELECT coalesce(sum(length(CAST(text AS BLOB))), 0) FROM chunks WHERE file_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    if text_budget(&mut chunks, have as u64) {
        eprintln!(
            "{}: over {MAX_TEXT_BYTES} bytes of text, the rest is left out",
            path.display()
        );
        done = total;
    }
    let vectors = embed_chunks(conn, emb, &title_of(path), &chunks, between)?;
    store_rest(conn, id, kind, done, total, &chunks, &vectors)?;
    Ok(Some(Rest::Parts {
        path: key,
        done,
        total,
        chunks: chunks.len(),
    }))
}

/// Keep what of `chunks` fits in MAX_TEXT_BYTES with `have` bytes of the
/// document in already. True when some had to go.
fn text_budget(chunks: &mut Vec<(usize, String)>, have: u64) -> bool {
    let mut total = have;
    let fits = chunks
        .iter()
        .take_while(|(_, c)| {
            total += c.len() as u64;
            total <= MAX_TEXT_BYTES
        })
        .count();
    let cut = fits < chunks.len();
    chunks.truncate(fits);
    cut
}

/// Whether any document is indexed in part only.
pub fn has_partial(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM partial LIMIT 1", [], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Documents indexed in part so far (#108), the next in line first: (path,
/// parts in, parts in all). At most `limit`.
pub fn partial(conn: &Connection, limit: usize) -> Result<Vec<(String, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT f.path, p.done, p.total FROM partial p JOIN files f ON f.id = p.file_id
         ORDER BY p.file_id LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
pub(crate) fn stamp_of(meta: &fs::Metadata) -> Option<(i64, i64)> {
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((mtime.as_secs() as i64, meta.len() as i64))
}

/// The (mtime, size) recorded for every indexed file under `root`, by path.
pub fn stamps_under(conn: &Connection, root: &Path) -> Result<HashMap<String, (i64, i64)>> {
    let mut stmt =
        conn.prepare("SELECT path, mtime, size FROM files WHERE path LIKE ?1 ESCAPE '\\'")?;
    let rows = stmt.query_map([like_under(&root.to_string_lossy())], |r| {
        Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What a file `walk` found is indexed as, with its metadata: None if it
/// isn't a wanted type or is over its kind's size cap. Reads no contents.
pub fn indexable(entry: &walkdir::DirEntry, types: &FileTypes) -> Option<(Kind, fs::Metadata)> {
    let kind = types.kind(entry.path())?;
    let meta = entry.metadata().ok()?;
    (meta.len() <= kind.max_bytes(entry.path())).then_some((kind, meta))
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
    let mut known = stamps_under(conn, root)?;
    let mut out = Vec::new();
    for entry in walk(root, ex).filter(|e| e.file_type().is_file()) {
        let path = entry.path();
        let Some(key) = path.to_str() else { continue };
        let was = known.remove(key);
        let Some((_, meta)) = indexable(&entry, types) else {
            // An indexed file that is no longer wanted (grew too big, say).
            if was.is_some() {
                out.push(path.to_path_buf());
            }
            continue;
        };
        if was != stamp_of(&meta) {
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

/// Drop the vectors of every kind made by a model other than this build's,
/// and record this build's (#80). The files holding them go too (with their
/// copies), so the next check of each root indexes them afresh. An index
/// with no record for a kind is taken as this build's and tagged: nothing
/// else has shipped for it. Returns (kind, files dropped) per kind purged.
pub fn purge_other_models(conn: &Connection) -> Result<Vec<(Kind, usize)>> {
    let tags: Vec<(Kind, String)> = Kind::ALL.iter().map(|k| (*k, k.model_tag())).collect();
    purge_models_but(conn, &tags)
}

fn purge_models_but(conn: &Connection, tags: &[(Kind, String)]) -> Result<Vec<(Kind, usize)>> {
    let mut out = Vec::new();
    let tx = conn.unchecked_transaction()?;
    for (kind, tag) in tags {
        let was: Option<String> = tx
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [kind.meta_key()],
                |r| r.get(0),
            )
            .optional()?;
        if was.as_deref().is_some_and(|w| w != tag) {
            out.push((*kind, drop_kind(&tx, *kind)?));
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![kind.meta_key(), tag],
        )?;
    }
    tx.commit()?;
    if out.iter().any(|(_, n)| *n > 0) {
        compact(conn)?;
    }
    Ok(out)
}

/// Every chunk of `kind`, and the files holding them and their copies.
/// Returns files removed.
fn drop_kind(tx: &Connection, kind: Kind) -> Result<usize> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS dropped (id INTEGER, sha BLOB); DELETE FROM dropped;",
    )?;
    tx.execute(
        "INSERT INTO dropped SELECT DISTINCT f.id, f.sha256 FROM files f
         JOIN chunks c ON c.file_id = f.id WHERE c.kind = ?1",
        [kind as i64],
    )?;
    // Chunks first: deleting a file with chunks hands them to a copy
    // (files_hand_over), and these must go.
    tx.execute("DELETE FROM chunks WHERE kind = ?1", [kind as i64])?;
    let n = tx.execute(
        "DELETE FROM files WHERE id IN (SELECT id FROM dropped)
         OR sha256 IN (SELECT sha FROM dropped WHERE sha IS NOT NULL)",
        [],
    )?;
    tx.execute_batch("DROP TABLE dropped")?;
    Ok(n)
}

/// `LIKE` patterns for files with a HEIF extension (#79).
fn heif_patterns() -> Vec<String> {
    crate::vision::HEIF_EXTENSIONS
        .iter()
        .map(|e| format!("%.{e}"))
        .collect()
}

/// Images recorded without chunks for want of a HEIF decoder: by extension,
/// leaving out copies of a file that did decode. What the Index page counts
/// while heif-convert is missing (#79).
pub fn undecodable(conn: &Connection) -> Result<i64> {
    let mut n = 0;
    for pattern in heif_patterns() {
        n += conn.query_row(
            "SELECT count(*) FROM files f
             WHERE lower(f.path) LIKE ?1
               AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = f.id)
               AND NOT EXISTS (SELECT 1 FROM files g JOIN chunks c ON c.file_id = g.id
                               WHERE g.sha256 = f.sha256)",
            [pattern],
            |r| r.get::<_, i64>(0),
        )?;
    }
    Ok(n)
}

/// Forget the images recorded as undecodable once a decoder is there, so the
/// next check of each root indexes them. Returns files dropped.
pub fn retry_undecodable(conn: &Connection) -> Result<usize> {
    if crate::vision::heif_convert().is_none() {
        return Ok(0);
    }
    let mut n = 0;
    for pattern in heif_patterns() {
        n += conn.execute(
            "DELETE FROM files WHERE lower(path) LIKE ?1
               AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = files.id)
               AND NOT EXISTS (SELECT 1 FROM files g JOIN chunks c ON c.file_id = g.id
                               WHERE g.sha256 = files.sha256)",
            [pattern],
        )?;
    }
    Ok(n)
}

/// Forget the PDFs recorded with no text once OCR could read them as scans
/// (#106): with this OCR model, and poppler installed. Recorded in `meta`, so
/// it happens once for each, not at every start. Returns files dropped.
pub fn retry_scans(conn: &Connection, ocr: bool) -> Result<usize> {
    if !ocr || !crate::ocr::poppler() {
        return Ok(0);
    }
    let key = "reading.scans";
    let now = format!("{}/{}", crate::model::OCR_REVISION, crate::ocr::READING);
    let now = now.as_str();
    let was: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    if was.as_deref() == Some(now) {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    let n = tx.execute(
        "DELETE FROM files WHERE lower(path) LIKE '%.pdf'
           AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = files.id)
           AND NOT EXISTS (SELECT 1 FROM files g JOIN chunks c ON c.file_id = g.id
                           WHERE g.sha256 = files.sha256)",
        [],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, now],
    )?;
    tx.commit()?;
    Ok(n)
}

/// Formats whose text is read differently now, with the reading's version:
/// HTML through `extract::html` since 1 (#97). Mtime and size don't show the
/// change, so files read the old way are dropped once instead.
const READINGS: &[(&str, &[&str], u32)] = &[("html", &["html", "htm"], 1)];

/// Forget the files of each format in `READINGS` indexed under an older
/// reading, so the next check of each root indexes them again. Returns files
/// dropped.
pub fn reread_formats(conn: &Connection) -> Result<usize> {
    let mut n = 0;
    let tx = conn.unchecked_transaction()?;
    for (format, extensions, version) in READINGS {
        let key = format!("reading.{format}");
        let was: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| {
                r.get(0)
            })
            .optional()?;
        if was == Some(version.to_string()) {
            continue;
        }
        for e in *extensions {
            n += tx.execute(
                "DELETE FROM files WHERE lower(path) LIKE ?1",
                [format!("%.{e}")],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, version.to_string()],
        )?;
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

/// `root` as `index` walks it: canonical, and an error if it is excluded.
pub fn resolve_root(root: &Path, ex: &Excludes) -> Result<PathBuf> {
    let root = fs::canonicalize(root).with_context(|| root.display().to_string())?;
    if ex.contains(&root) {
        bail!(
            "{} is excluded; remove it from `exclude` in the config to index it",
            root.display()
        );
    }
    Ok(root)
}

/// Called after each file `index` looks at, with the connection and model
/// lent back: for progress, and for the runner to answer queries mid-index.
/// An error from it stops indexing; every file finished so far is kept.
pub type AfterFile<'a, E = Embedder> = dyn FnMut(&Stats, &Connection, &mut E) -> Result<()> + 'a;

/// Remember `root` (canonical, from `resolve_root`) as a root to watch.
pub fn add_root(conn: &Connection, root: &Path) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO roots (path) VALUES (?1)",
        [root.to_string_lossy()],
    )?;
    Ok(())
}

/// Index everything wanted under `root` and remember it as a root to watch.
/// Unchanged files (same mtime and size) are skipped; files under `root`
/// that are gone are removed.
pub fn index<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    root: &Path,
    ex: &Excludes,
    types: &FileTypes,
    after: &mut AfterFile<E>,
) -> Result<Stats> {
    let root = resolve_root(root, ex)?;
    let purged = purge(conn, ex)?;
    add_root(conn, &root)?;
    let mut stats = Stats::default();
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
            Outcome::Unreadable { no_decoder } => {
                stats.files_seen += 1;
                stats.no_decoder += usize::from(no_decoder);
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
    } else if crate::code::is_code(path) {
        Some(Kind::Code)
    } else if crate::extract::is_document(path) {
        Some(Kind::Document)
    } else if crate::extract::is_session(path) {
        Some(Kind::Session)
    } else {
        None
    }
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
pub fn counts_by_kind(conn: &Connection) -> Result<[(i64, i64); 5]> {
    let mut out = [(0, 0); 5];
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
           AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = f.id)",
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

/// The chunks holding every word of a query (#104), best (bm25) first,
/// looked up once per query for every kind.
pub struct Words {
    ids: Vec<i64>,
    /// The query is one identifier-like word (`INV-20431`, `E0502`, `TODO`)
    /// or wholly quoted: exact matches lead instead of nudging.
    exact: bool,
}

/// Added to the cosine of a chunk holding every word of the query. About the
/// gap between a good result and the next few, so exact words reorder near
/// ties and lift a chunk just under its kind's `min_score` over it, without
/// carrying an unrelated chunk past a related one.
const WORDS_BOOST: f32 = 0.05;
/// A word in more chunks than this is no exact string to find, and ranking
/// it costs more than the vectors: bm25 over 2,856 chunks took 14 ms, and
/// "the" (81k of 113k chunks) 250 ms. Such words are left out of the lookup,
/// and counting up to this stops early.
const WORDS_MAX_CHUNKS: usize = 2000;
/// Chunks taken from the word index, across every kind.
const WORDS_TAKE: usize = 200;

impl Words {
    /// None for a query with no words, or only words in too many chunks.
    /// A failed lookup is None too: exact words add to a search, they never
    /// stop one.
    pub fn find(conn: &Connection, query: &str) -> Option<Self> {
        let (terms, exact) = expression(query)?;
        match ranked(conn, &terms) {
            Ok(ids) => ids.map(|ids| Self { ids, exact }),
            Err(e) => {
                eprintln!("words {terms:?}: {e}");
                None
            }
        }
    }
}

/// `query`'s terms as FTS5 strings: every word, or each quoted part as a
/// phrase. With whether exact matches should lead.
fn expression(query: &str) -> Option<(Vec<String>, bool)> {
    let query = query.trim();
    let mut terms = Vec::new();
    let mut rest = query;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
        } else if c == '"' {
            // A phrase, to the closing quote or the end.
            let body = &rest[1..];
            let end = body.find('"').unwrap_or(body.len());
            terms.push(&body[..end]);
            rest = body.get(end + 1..).unwrap_or("");
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '"')
                .unwrap_or(rest.len());
            terms.push(&rest[..end]);
            rest = &rest[end..];
        }
    }
    // Everything is quoted, so no word is read as an operator (AND, NEAR),
    // a column filter (`:`), a prefix (`*`) or a syntax error; the tokenizer
    // then splits each on punctuation as it split the chunks. A term with no
    // letters or digits would be an empty phrase.
    let expr: Vec<String> = terms
        .iter()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if expr.is_empty() {
        return None;
    }
    let quoted =
        query.len() > 1 && query.starts_with('"') && query[1..].find('"') == Some(query.len() - 2);
    Some((expr, quoted || (terms.len() == 1 && identifier(query))))
}

/// A word an embedding has no sense of: one with a digit, inner punctuation
/// (`foo_bar`, `v2.1`, `INV-20431`) or all capitals (`TODO`, `ENOENT`).
fn identifier(word: &str) -> bool {
    let inner = word.trim_matches(|c: char| !c.is_alphanumeric());
    let letters = inner.chars().filter(|c| c.is_alphabetic()).count();
    inner.chars().any(|c| c.is_ascii_digit())
        || inner.contains(|c: char| !c.is_alphanumeric() && c != '\'')
        || (letters >= 2 && !inner.chars().any(char::is_lowercase))
}

/// Chunks holding every term in no more than WORDS_MAX_CHUNKS chunks, best
/// first, at most WORDS_TAKE; None if every term is in more. Without the
/// common ones a long query costs little more than a short one: "how is the
/// NPU driver version checked" took 30 ms with them.
fn ranked(conn: &Connection, terms: &[String]) -> rusqlite::Result<Option<Vec<i64>>> {
    let mut count = conn.prepare(
        "SELECT count(*) FROM (SELECT 1 FROM chunk_words WHERE chunk_words MATCH ?1 LIMIT ?2)",
    )?;
    let mut rare = Vec::new();
    for t in terms {
        let n: i64 = count.query_row(params![t, WORDS_MAX_CHUNKS as i64 + 1], |r| r.get(0))?;
        if n as usize <= WORDS_MAX_CHUNKS {
            rare.push(t.as_str());
        }
    }
    if rare.is_empty() {
        return Ok(None);
    }
    conn.prepare("SELECT rowid FROM chunk_words WHERE chunk_words MATCH ?1 ORDER BY rank LIMIT ?2")?
        .query_map(params![rare.join(" "), WORDS_TAKE as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()
        .map(Some)
}

/// What `query` searches: code alone, for a query starting `code:` (#110),
/// or everything but code, text first; with the query to embed and look up,
/// less its `code:`.
pub fn scope(query: &str) -> (&str, &'static [Kind]) {
    match crate::code::code_query(query) {
        Some(q) => (q, &[Kind::Code]),
        None => (
            query,
            &[Kind::Text, Kind::Document, Kind::Image, Kind::Session],
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

#[cfg(test)]
mod pipeline_tests;

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
    fn long_lines_split_between_words() {
        let text = "word ".repeat(CHUNK_CHARS);
        let chunks = chunk(&text);
        assert!(chunks.len() >= 5);
        for (_, c) in &chunks {
            assert!(c.len() <= CHUNK_CHARS + 1);
            assert!(c.trim_end().split(' ').all(|w| w == "word"), "{c:?}");
        }
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

    /// `chunks` as (kind, line) on a new file at `path`.
    fn file_with(conn: &Connection, path: &str, chunks: &[(Kind, i64)]) {
        conn.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, 0, 0, ?1)",
            [path],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        for (kind, line) in chunks {
            conn.execute(
                "INSERT INTO chunks (file_id, start_line, text, embedding, kind) VALUES (?1, ?2, '', x'00', ?3)",
                params![id, line, *kind as i64],
            )
            .unwrap();
        }
    }

    #[test]
    fn an_images_text_counts_under_images() {
        let dir = std::env::temp_dir().join(format!("vinosearch-ocrcount-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        file_with(
            &conn,
            "/shot.png",
            &[(Kind::Image, 0), (Kind::Document, 1), (Kind::Document, 9)],
        );
        file_with(
            &conn,
            "/report.pdf",
            &[(Kind::Document, 1), (Kind::Document, 2)],
        );
        file_with(&conn, "/notes.md", &[(Kind::Text, 1)]);
        let counts = counts_by_kind(&conn).unwrap();
        // Kind::ALL's order: text, documents, images.
        assert_eq!(counts, [(1, 1), (1, 2), (1, 3), (0, 0), (0, 0)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pdfs_with_no_text_are_read_again_once_ocr_can() {
        let dir = std::env::temp_dir().join(format!("vinosearch-scans-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        file_with(&conn, "/scan.PDF", &[]);
        file_with(&conn, "/text.pdf", &[(Kind::Document, 1)]);
        file_with(&conn, "/broken.jpg", &[]);
        let paths = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT path FROM files ORDER BY path")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(retry_scans(&conn, false).unwrap(), 0);
        if crate::ocr::poppler() {
            assert_eq!(retry_scans(&conn, true).unwrap(), 1);
            assert_eq!(paths(&conn), ["/broken.jpg", "/text.pdf"]);
            // Once per OCR model: a scan recorded again isn't dropped at every start.
            file_with(&conn, "/scan.PDF", &[]);
            assert_eq!(retry_scans(&conn, true).unwrap(), 0);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_files_are_text_and_documents_newest_first() {
        let dir = std::env::temp_dir().join(format!("vinosearch-recent-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        for (p, mtime) in [
            ("/old.md", 1),
            ("/report.pdf", 5),
            ("/photo.jpg", 9),
            ("/gone.txt", 8),
            ("/notes.md", 7),
            ("/deck.pptx", 3),
        ] {
            conn.execute(
                "INSERT INTO files (path, mtime, size) VALUES (?1, ?2, 0)",
                params![p, mtime],
            )
            .unwrap();
        }
        let exists = |p: &Path| p != Path::new("/gone.txt");
        let recent = recent_files(&conn, 3, exists).unwrap();
        assert_eq!(
            recent,
            [
                ("/notes.md".to_owned(), Kind::Text, 7),
                ("/report.pdf".to_owned(), Kind::Document, 5),
                ("/deck.pptx".to_owned(), Kind::Document, 3),
            ]
        );
        assert_eq!(recent_files(&conn, 10, exists).unwrap().len(), 4);
        assert!(recent_files(&conn, 0, exists).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
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
        let hits = search(
            &conn,
            &Vectors::load(&conn).unwrap(),
            &v,
            None,
            Kind::Text,
            5,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/a/x.md");
        assert_eq!(hits[0].copies, ["/b/x.md"]);

        // The holder goes: the copy takes its chunks, same ids.
        remove(&conn, Path::new("/a")).unwrap();
        let owner: i64 = conn
            .query_row("SELECT file_id FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(owner, b);
        let hits = search(
            &conn,
            &Vectors::load(&conn).unwrap(),
            &v,
            None,
            Kind::Text,
            5,
        )
        .unwrap();
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
        // Its chunks are in the word index, under their file's name too.
        assert_eq!(matching(&conn, "x"), [1]);
        assert_eq!(matching(&conn, "old"), [1]);
        drop(conn);
        open(&db).unwrap(); // and opening it again is a no-op
        assert_eq!(matching(&open(&db).unwrap(), "x"), [1]);
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
        let code = FileTypes {
            code: true,
            ..FileTypes::default()
        };
        assert_eq!(code.kind(Path::new("/p/app.min.js")), None);
        assert_eq!(code.kind(Path::new("/p/vendor.bundle.js")), None);
        assert_eq!(code.kind(Path::new("/p/app.js")), Some(Kind::Code));
        assert_eq!(code.kind(Path::new("/p/README.md")), Some(Kind::Text));
        // Code is off unless turned on (#110).
        assert_eq!(FileTypes::default().kind(Path::new("/p/app.js")), None);
    }

    #[test]
    fn sessions_are_taken_anywhere_and_only_sessions_from_vinowhispers_folder() {
        let dir = PathBuf::from("/h/.local/share/vinowhisper/transcripts");
        let on = FileTypes::default().with_sessions(true, Some(dir.clone()));
        let off = FileTypes::default().with_sessions(false, Some(dir.clone()));
        let s = "2026-10-27-140512.vwsession";
        assert_eq!(on.kind(&dir.join(s)), Some(Kind::Session));
        assert_eq!(
            on.kind(&Path::new("/h/Documents").join(s)),
            Some(Kind::Session)
        );
        // A live transcript, and a bundle being written.
        assert_eq!(on.kind(&dir.join("2026-10-28-090000.txt")), None);
        assert_eq!(
            on.kind(&dir.join(".2026-10-27-140512.vwsession.k3j.tmp")),
            None
        );
        assert_eq!(off.kind(&dir.join(s)), None);
        assert_eq!(off.kind(&Path::new("/h/Documents").join(s)), None);
        assert_eq!(on.sessions_dir(), Some((dir.as_path(), true)));
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
        let lines: Vec<i64> = search(&conn, &vectors, &q, None, Kind::Text, 3)
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
    fn documents_are_wanted_only_while_on() {
        let doc = Path::new("/p/statement.PDF");
        assert_eq!(FileTypes::default().kind(doc), None);
        let mut f = Formats::default();
        f.documents.enabled = Some(true);
        let on = FileTypes::new(&f).unwrap();
        assert_eq!(on.kind(doc), Some(Kind::Document));
        assert_eq!(on.kind(Path::new("/p/a.csv")), Some(Kind::Document));
        assert_eq!(on.kind(Path::new("/p/a.md")), Some(Kind::Text));
        assert_eq!(on.kind(Path::new("/p/a.mobi")), None);
    }

    #[test]
    fn documents_are_searched_apart_from_text() {
        let dir = std::env::temp_dir().join(format!("vinosearch-docs-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        add_file(&conn, "/notes.md", &[0]);
        add_file(&conn, "/a.pdf", &[0]);
        conn.execute("UPDATE chunks SET kind = 2 WHERE file_id = (SELECT id FROM files WHERE path = '/a.pdf')", []).unwrap();
        let v = Vectors::load(&conn).unwrap();
        let mut q = vec![0f32; DIM];
        q[0] = 1.0;
        let paths = |kind| -> Vec<String> {
            search(&conn, &v, &q, None, kind, 5)
                .unwrap()
                .into_iter()
                .map(|h| h.path)
                .collect()
        };
        assert_eq!(paths(Kind::Text), ["/notes.md"]);
        assert_eq!(paths(Kind::Document), ["/a.pdf"]);
        assert!(paths(Kind::Image).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_document_that_does_not_parse_has_no_chunks() {
        let parse = crate::extract::parts;
        assert!(document_parts(Path::new("/p/a.pdf"), b"not a pdf", 0, parse).is_err());
        // A run of parts with over MAX_FILE_BYTES of text: one slide will do.
        let slide = format!(
            "<draw:page><text:p>{}</text:p></draw:page>",
            "word ".repeat(250_000)
        );
        assert!(document_parts(Path::new("/p/a.fodp"), slide.as_bytes(), 0, parse).is_err());
    }

    /// `path` indexed as it is on disk now, with `chunks` chunks and the
    /// first `done` of `total` parts in.
    fn add_partial(conn: &Connection, path: &Path, done: i64, total: i64) -> i64 {
        let (mtime, size) = stamp_of(&fs::metadata(path).unwrap()).unwrap();
        conn.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, x'01')",
            params![path.to_str().unwrap(), mtime, size],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (?1, 1, 'first', ?2)",
            params![id, axis(0)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO partial (file_id, done, total) VALUES (?1, ?2, ?3)",
            params![id, done, total],
        )
        .unwrap();
        id
    }

    #[test]
    fn the_rest_of_a_long_document_resumes_where_it_stopped() {
        let dir = std::env::temp_dir().join(format!("vinosearch-rest-{}", std::process::id()));
        let db = dir.join("i.sqlite");
        let mut conn = open(&db).unwrap();
        let book = dir.join("book.txt");
        fs::write(&book, "a long book").unwrap();
        let key = book.to_str().unwrap().to_owned();
        let id = add_partial(&conn, &book, 30, 70);
        assert!(has_partial(&conn).unwrap());
        assert_eq!(partial(&conn, 10).unwrap(), [(key.clone(), 30, 70)]);
        let parts = |from| Next::Parts {
            id,
            path: key.clone(),
            from,
            total: 70,
        };
        assert_eq!(next_rest(&conn).unwrap(), Some(parts(30)));
        let run = [(31, "second".to_owned())];
        store_rest(&mut conn, id, Kind::Text, 60, 70, &run, &[vec![0.0; DIM]]).unwrap();
        // A restart picks it up from there.
        drop(conn);
        let mut conn = open(&db).unwrap();
        assert_eq!(next_rest(&conn).unwrap(), Some(parts(60)));
        let run = [(61, "third".to_owned())];
        store_rest(&mut conn, id, Kind::Text, 70, 70, &run, &[vec![0.0; DIM]]).unwrap();
        assert_eq!(next_rest(&conn).unwrap(), None);
        assert!(!has_partial(&conn).unwrap());
        // Still one file, with every run's chunks.
        assert_eq!(counts(&conn).unwrap(), (1, 3));
        assert_eq!(counts_by_kind(&conn).unwrap()[0], (1, 3));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_long_document_that_changed_or_went_is_indexed_again() {
        let dir = std::env::temp_dir().join(format!("vinosearch-again-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        let book = dir.join("book.txt");
        fs::write(&book, "a long book").unwrap();
        let key = book.to_str().unwrap().to_owned();
        let id = add_partial(&conn, &book, 30, 70);
        fs::write(&book, "a longer book").unwrap();
        assert_eq!(next_rest(&conn).unwrap(), Some(Next::Again(key.clone())));
        fs::remove_file(&book).unwrap();
        assert_eq!(next_rest(&conn).unwrap(), Some(Next::Again(key)));
        // Indexed again, the old row and its partial row go.
        conn.execute("DELETE FROM files WHERE id = ?1", [id])
            .unwrap();
        assert!(!has_partial(&conn).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_copy_takes_over_a_long_document_indexed_in_part() {
        let dir = std::env::temp_dir().join(format!("vinosearch-hand-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        let book = dir.join("book.pdf");
        fs::write(&book, "%PDF").unwrap();
        let id = add_partial(&conn, &book, 30, 640);
        conn.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES ('/copy.pdf', 0, 0, x'01')",
            [],
        )
        .unwrap();
        let copy = conn.last_insert_rowid();
        conn.execute("DELETE FROM files WHERE id = ?1", [id])
            .unwrap();
        assert_eq!(
            partial(&conn, 10).unwrap(),
            [("/copy.pdf".to_owned(), 30, 640)]
        );
        assert_eq!(counts(&conn).unwrap(), (1, 1));
        conn.execute("DELETE FROM files WHERE id = ?1", [copy])
            .unwrap();
        assert!(!has_partial(&conn).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_past_the_budget_is_left_out() {
        let mut run = vec![(1, "x".repeat(600)), (2, "y".repeat(600))];
        assert!(!text_budget(&mut run, 0));
        assert_eq!(run.len(), 2);
        assert!(text_budget(&mut run, MAX_TEXT_BYTES - 1000));
        assert_eq!(run.len(), 1);
        assert!(text_budget(&mut run, MAX_TEXT_BYTES));
        assert!(run.is_empty());
    }

    #[test]
    fn prose_may_be_a_book_but_code_may_not() {
        assert_eq!(
            Kind::Text.max_bytes(Path::new("/b/Moby Dick.TXT")),
            MAX_TEXT_BYTES
        );
        assert_eq!(
            Kind::Text.max_bytes(Path::new("/b/notes.md")),
            MAX_TEXT_BYTES
        );
        assert_eq!(Kind::Text.max_bytes(Path::new("/b/gen.rs")), MAX_FILE_BYTES);
        assert_eq!(
            Kind::Text.max_bytes(Path::new("/b/page.html")),
            MAX_FILE_BYTES
        );
    }

    #[test]
    fn html_indexed_before_it_was_read_as_html_is_indexed_again_once() {
        let dir = std::env::temp_dir().join(format!("vinosearch-reread-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        for p in ["/site/index.html", "/site/OLD.HTM", "/notes.md"] {
            add_file(&conn, p, &[0]);
        }
        assert_eq!(reread_formats(&conn).unwrap(), 2);
        assert!(is_indexed(&conn, "/notes.md").unwrap());
        assert!(!is_indexed(&conn, "/site/index.html").unwrap());
        add_file(&conn, "/site/index.html", &[0]);
        assert_eq!(reread_formats(&conn).unwrap(), 0);
        fs::remove_dir_all(&dir).unwrap();
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
            search(&conn, v, &q, None, kind, 5)
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

    #[test]
    fn counts_are_split_by_kind() {
        let dir = std::env::temp_dir().join(format!("vinosearch-bykind-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        add_file(&conn, "/notes.md", &[0, 1]);
        add_file(&conn, "/more.md", &[2]);
        add_file(&conn, "/cat.png", &[3]);
        conn.execute("UPDATE chunks SET kind = 1 WHERE file_id = (SELECT id FROM files WHERE path = '/cat.png')", []).unwrap();
        // A document recorded without text counts under nothing.
        conn.execute(
            "INSERT INTO files (path, mtime, size) VALUES ('/scan.pdf', 0, 0)",
            [],
        )
        .unwrap();
        assert_eq!(
            counts_by_kind(&conn).unwrap(),
            [(2, 3), (0, 0), (1, 1), (0, 0), (0, 0)]
        );
        assert_eq!(counts(&conn).unwrap(), (4, 4));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_indexed_image_gives_its_vector_and_so_does_a_copy() {
        let dir = std::env::temp_dir().join(format!("vinosearch-simv-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        conn.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES ('/p/a.jpg', 0, 0, x'aa')",
            [],
        )
        .unwrap();
        let a = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks (file_id, start_line, text, embedding, kind) VALUES (?1, 0, '', ?2, 1)",
            params![a, axis(3)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES ('/p/copy.jpg', 0, 0, x'aa')",
            [],
        )
        .unwrap();
        add_file(&conn, "/p/notes.md", &[4]);
        let mut want = vec![0f32; DIM];
        want[3] = 1.0;
        assert_eq!(image_vector(&conn, "/p/a.jpg").unwrap(), Some(want.clone()));
        assert_eq!(image_vector(&conn, "/p/copy.jpg").unwrap(), Some(want));
        assert_eq!(image_vector(&conn, "/p/notes.md").unwrap(), None);
        assert_eq!(image_vector(&conn, "/p/missing.jpg").unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn undecodable_photos_are_counted_but_not_their_decoded_copies() {
        let dir = std::env::temp_dir().join(format!("vinosearch-heif-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        let file = |path: &str, hash: &[u8]| {
            conn.execute(
                "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, 0, 0, ?2)",
                params![path, hash],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        file("/p/IMG_1.HEIC", b"a");
        file("/p/IMG_2.heic", b"b");
        file("/p/render.avif", b"c");
        // Decoded, and a byte-identical copy of it holding no chunks.
        let decoded = file("/p/IMG_3.heic", b"d");
        conn.execute(
            "INSERT INTO chunks (file_id, start_line, text, embedding, kind) VALUES (?1, 0, '', ?2, 1)",
            params![decoded, axis(0)],
        )
        .unwrap();
        file("/p/copy.heic", b"d");
        // Not HEIF: a document with no text.
        file("/p/scan.pdf", b"e");
        assert_eq!(undecodable(&conn).unwrap(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vectors_from_another_model_are_dropped_with_their_files() {
        let dir = std::env::temp_dir().join(format!("vinosearch-model-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        let tag = |kind: Kind, t: &str| (kind, t.to_owned());
        let v1 = [
            tag(Kind::Text, "text-1"),
            tag(Kind::Document, "text-1"),
            tag(Kind::Image, "text-1+vision-1/140"),
        ];
        // A fresh index has no record: nothing to drop, this build is recorded.
        assert!(purge_models_but(&conn, &v1).unwrap().is_empty());
        add_file(&conn, "/notes.md", &[0, 1]);
        add_file(&conn, "/cat.png", &[2]);
        conn.execute("UPDATE chunks SET kind = 1 WHERE file_id = (SELECT id FROM files WHERE path = '/cat.png')", []).unwrap();
        // A byte-identical copy of the photo, holding no chunks of its own.
        conn.execute(
            "UPDATE files SET sha256 = x'01' WHERE path = '/cat.png'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES ('/copy.png', 0, 0, x'01')",
            [],
        )
        .unwrap();
        // Same models again: nothing happens.
        assert!(purge_models_but(&conn, &v1).unwrap().is_empty());
        assert_eq!(counts(&conn).unwrap(), (3, 3));

        // A new vision encoder: the images go, the copy included; text stays.
        let v2 = [
            tag(Kind::Text, "text-1"),
            tag(Kind::Document, "text-1"),
            tag(Kind::Image, "text-1+vision-2/140"),
        ];
        assert_eq!(purge_models_but(&conn, &v2).unwrap(), [(Kind::Image, 2)]);
        assert_eq!(counts(&conn).unwrap(), (1, 2));
        assert!(is_indexed(&conn, "/notes.md").unwrap());
        assert!(purge_models_but(&conn, &v2).unwrap().is_empty());

        // A new text model: everything of that kind goes.
        let v3 = [
            tag(Kind::Text, "text-2"),
            tag(Kind::Document, "text-2"),
            tag(Kind::Image, "text-1+vision-2/140"),
        ];
        let purged = purge_models_but(&conn, &v3).unwrap();
        assert_eq!(purged, [(Kind::Text, 1), (Kind::Document, 0)]);
        assert_eq!(counts(&conn).unwrap(), (0, 0));
        let recorded: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'model.text'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(recorded, "text-2");
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

    /// Chunk ids whose name or text hold `query`'s words, in id order.
    fn matching(conn: &Connection, query: &str) -> Vec<i64> {
        let (terms, _) = expression(query).unwrap();
        let mut ids: Vec<i64> = conn
            .prepare("SELECT rowid FROM chunk_words WHERE chunk_words MATCH ?1")
            .unwrap()
            .query_map([terms.join(" ")], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn no_query_can_break_the_word_lookup() {
        let dir = std::env::temp_dir().join(format!("vinosearch-fts-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        add_file(&conn, "/notes/a.md", &[0]);
        conn.execute("UPDATE chunks SET text = 'near the end, or not'", [])
            .unwrap();
        for q in [
            "foo AND bar",
            "NEAR(a b)",
            "NEAR",
            "text:near",
            "pre*",
            "\"unbalanced",
            "a\"b",
            "\"\" \"\"",
            "-x",
            "^start",
            "(a OR b",
            "col : val + 1",
            "{name text}: x",
            "C++",
            "don't",
            "café 日本語 🙂",
            "OR",
            "NOT",
        ] {
            if let Some((terms, _)) = expression(q) {
                assert!(ranked(&conn, &terms).is_ok(), "{q} -> {terms:?}");
            }
        }
        // Operators are words like any other.
        assert_eq!(matching(&conn, "near or not"), [1]);
        assert_eq!(matching(&conn, "end NOT near"), [1]);
        assert!(matching(&conn, "text:end").is_empty());
        for q in ["", "  ", "***", "\"\"", "- : ()"] {
            assert!(expression(q).is_none(), "{q}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn identifiers_and_quotes_lean_on_exact_words() {
        for q in [
            "INV-20431",
            "E0502",
            "TODO",
            "foo_bar",
            "v2.1",
            "\"lunch with sam\"",
        ] {
            assert!(expression(q).unwrap().1, "{q}");
        }
        for q in [
            "invoice",
            "Karan",
            "don't",
            "invoice INV-20431",
            "\"a\" b \"c\"",
            "photos from 2019",
        ] {
            assert!(!expression(q).unwrap().1, "{q}");
        }
    }

    #[test]
    fn exact_words_find_what_vectors_miss() {
        let dir = std::env::temp_dir().join(format!("vinosearch-hybrid-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        // Axis 0 is "invoices in general"; the one with the number is off it.
        let general = add_file(&conn, "/inv/general.md", &[0])[0];
        let near = add_file(&conn, "/inv/near.md", &[0])[0];
        let numbered = add_file(&conn, "/inv/march.md", &[1])[0];
        let set = |id: i64, text: &str, v: &[f32]| {
            let mut e = vec![0f32; DIM];
            e[..v.len()].copy_from_slice(v);
            conn.execute(
                "UPDATE chunks SET text = ?1, embedding = ?2 WHERE id = ?3",
                params![text, to_blob(&e), id],
            )
            .unwrap();
        };
        set(general, "Invoices are due in 30 days.", &[0.8, 0.6]);
        set(near, "The march invoice total.", &[0.78, 0.6257]);
        set(numbered, "Invoice INV-20431, total 120 EUR", &[0.2, 0.98]);
        let vectors = Vectors::load(&conn).unwrap();
        let mut q = vec![0f32; DIM];
        q[0] = 1.0;
        let paths = |words: Option<&Words>, k: usize| -> Vec<String> {
            search(&conn, &vectors, &q, words, Kind::Text, k)
                .unwrap()
                .into_iter()
                .map(|h| h.path)
                .collect()
        };
        assert_eq!(paths(None, 1), ["/inv/general.md"]);
        // An identifier leads, whatever its cosine.
        let id = Words::find(&conn, "INV-20431").unwrap();
        assert_eq!(paths(Some(&id), 1), ["/inv/march.md"]);
        assert_eq!(
            paths(Some(&id), 3),
            ["/inv/march.md", "/inv/general.md", "/inv/near.md"]
        );
        // Plain words nudge a near tie, but don't lift a far chunk.
        let plain = Words::find(&conn, "march invoice").unwrap();
        assert_eq!(paths(Some(&plain), 2), ["/inv/near.md", "/inv/general.md"]);
        let plain = Words::find(&conn, "invoice total").unwrap();
        assert_eq!(paths(Some(&plain), 2), ["/inv/near.md", "/inv/general.md"]);
        // File names count as words: an image has no text but its name.
        let name = Words::find(&conn, "march.md").unwrap();
        assert!(name.ids.contains(&numbered));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn words_in_too_many_chunks_are_not_looked_up() {
        let dir = std::env::temp_dir().join(format!("vinosearch-common-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        add_file(&conn, "/f.md", &[]);
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..=WORDS_MAX_CHUNKS {
            let text = if i == 7 { "common rare" } else { "common" };
            tx.execute(
                "INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (1, 1, ?1, x'')",
                [text],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        assert!(Words::find(&conn, "common").is_none());
        // The common word is left out; the rare one still has to be there.
        assert_eq!(Words::find(&conn, "common rare").unwrap().ids.len(), 1);
        assert_eq!(Words::find(&conn, "rare common").unwrap().ids.len(), 1);
        assert!(Words::find(&conn, "absent").unwrap().ids.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_word_index_follows_chunks() {
        let dir = std::env::temp_dir().join(format!("vinosearch-words-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        conn.execute_batch(
            "INSERT INTO files (path, mtime, size, sha256) VALUES ('/a/first.md', 0, 0, x'01');
             INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (1, 1, 'zebra', x'');
             INSERT INTO files (path, mtime, size, sha256) VALUES ('/b/second.md', 0, 0, x'01');",
        )
        .unwrap();
        assert_eq!(matching(&conn, "zebra"), [1]);
        assert_eq!(matching(&conn, "first"), [1]);
        // The holder goes: its chunk is found under the copy's name instead.
        remove(&conn, Path::new("/a")).unwrap();
        assert_eq!(matching(&conn, "zebra second"), [1]);
        assert!(matching(&conn, "first").is_empty());
        // The last copy goes, and the chunk with it.
        remove(&conn, Path::new("/b")).unwrap();
        assert!(matching(&conn, "zebra").is_empty());
        fs::remove_dir_all(&dir).unwrap();
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
