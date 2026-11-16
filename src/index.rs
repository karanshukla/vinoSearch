//! Files -> chunks -> embeddings in SQLite, and brute-force search over them.
//! A text file is many chunks; an image is one, embedded from its pixels.
//!
//! Embeddings can be partly inverted back to their text, so the index is as
//! sensitive as the files it covers: encrypted at rest when `encryption` is
//! on (encryption.rs, #4), plaintext readable only by the user otherwise.

pub mod code;
pub mod encryption;
pub mod estimate;
pub mod media;
mod purge;
mod query;
mod schema;
pub mod seal;
mod vectors;
mod words;

pub use purge::*;
pub use query::*;
pub use schema::open;
pub use vectors::Vectors;
pub use words::Words;

use crate::config::{Excludes, Formats};
use crate::extract::{Extract, MAX_PARTS, PARTS_AT_ONCE, Parser};
use crate::ml::embed::{DIM, Embed, Embedder, document_text};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::ControlFlow;
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
/// Prose and markup; code is `code::EXTENSIONS`. No config formats: they
/// are where credentials live (pinned by `tests::config_files_are_not_indexed`).
pub(crate) const TEXT_EXTENSIONS: &[&str] = &["md", "txt", "rst", "org", "tex", "html", "htm"];
/// Directories that are always build output, caches, generated reports or
/// vendored dependencies (`obj` is .NET's). Generic names a person might use
/// for their own files (`bin`, `out`, `lib`) are left out (pinned by
/// `tests::generated_trees_and_files_are_skipped`).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    "dist",
    "target",
    "build",
    "_build",
    "obj",
    "__pycache__",
    "venv",
    "site-packages",
    "miniconda3",
    "anaconda3",
    "miniforge3",
    "mambaforge",
    "vendor",
    "Pods",
    "DerivedData",
    "elm-stuff",
    "coverage",
    "htmlcov",
];
/// Directory name prefixes with the same meaning (`cmake-build-debug`, ...).
const SKIP_DIR_PREFIXES: &[&str] = &["cmake-build-"];
/// `Kind::max_bytes` for media, which `formats.media.max_hours` holds by
/// duration instead, and which is never read whole.
const HELD_BY_DURATION: u64 = u64::MAX;

use vinosearch_shared::limits::MAX_IMAGE_BYTES;
use vinosearch_shared::xdg;
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
    /// An audio or video file's sound, 10 s windows numbered by their first
    /// second, and its frames, from `extract::media::FRAME_LINES` up
    /// (#187); `extract::media::Track::of` says which. Embedded after
    /// everything else, from `index_rest`.
    Media = 5,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Text,
        Kind::Document,
        Kind::Image,
        Kind::Code,
        Kind::Session,
        Kind::Media,
    ];

    fn from_db(v: i64) -> Self {
        match v {
            1 => Kind::Image,
            2 => Kind::Document,
            3 => Kind::Code,
            4 => Kind::Session,
            5 => Kind::Media,
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
            Kind::Media => HELD_BY_DURATION,
        }
    }

    /// What made this kind's vectors, as `meta` records it: the models'
    /// revisions, and whatever else changes the vectors or what a hit points
    /// at. Vectors with another tag score against this build's queries
    /// without erroring, just wrongly, so they are dropped.
    pub fn model_tag(self) -> String {
        match self {
            Kind::Text | Kind::Document | Kind::Code => crate::ml::model::REVISION.to_owned(),
            // The text model's vectors, but a hit's stamp needs paragraph chunks.
            Kind::Session => format!("{}+paragraphs", crate::ml::model::REVISION),
            Kind::Image => format!(
                "{}+{}/{}+ocr:{}/{}",
                crate::ml::model::REVISION,
                crate::ml::model::VISION_REVISION,
                crate::ml::vision::SOFT_TOKENS,
                crate::ml::model::OCR_REVISION,
                crate::ml::ocr::READING,
            ),
            Kind::Media => format!(
                "{}+{}/{}+audio:{}+{}",
                crate::ml::model::REVISION,
                crate::ml::model::VISION_REVISION,
                crate::ml::vision::SOFT_TOKENS,
                crate::ml::model::AUDIO_REVISION,
                crate::ml::model::AUDIO_TEXT_REVISION,
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
            Kind::Media => "model.media",
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
    /// vinoWhisper's own folders of sessions, where nothing else is taken:
    /// a native install's, and its Flatpak's.
    sessions_dirs: Vec<PathBuf>,
    /// Audio and video, while on and ffmpeg is installed (#187).
    media: bool,
    /// The longest media file indexed, in seconds.
    max_media_seconds: f64,
    ignores: SharedIgnores,
}

/// The ignore files read, shared by a `FileTypes` and its clones; no part
/// of what makes two equal.
#[derive(Debug, Clone, Default)]
struct SharedIgnores(std::sync::Arc<crate::index::code::Ignores>);

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
            sessions_dirs: sessions_dirs(),
            media: formats.media_enabled() && crate::extract::media::ffmpeg(),
            max_media_seconds: formats.media_max_seconds()?,
            ignores: SharedIgnores::default(),
        })
    }

    #[cfg(test)]
    pub fn with_media(mut self, on: bool, max_seconds: f64) -> Self {
        self.media = on;
        self.max_media_seconds = max_seconds;
        self
    }

    #[cfg(test)]
    pub fn with_sessions(mut self, on: bool, dir: Option<PathBuf>) -> Self {
        self.sessions = on;
        self.sessions_dirs = dir.into_iter().collect();
        self
    }

    /// vinoWhisper's folders of sessions, and whether sessions are on. While
    /// they are each is a root of its own: the walk of a home folder skips
    /// hidden directories.
    pub fn sessions_dirs(&self) -> (&[PathBuf], bool) {
        (&self.sessions_dirs, self.sessions)
    }

    /// The kind `path` is indexed as, by its name (and for code, the ignore
    /// files above it); None if it is not.
    pub fn kind(&self, path: &Path) -> Option<Kind> {
        let session = crate::extract::is_session(path);
        if !session && self.in_sessions_dir(path) {
            return None;
        }
        if session {
            return self.sessions.then_some(Kind::Session);
        }
        if text_file(path) {
            return Some(Kind::Text);
        }
        if crate::index::code::is_code(path) {
            return (self.code && !self.ignores.0.left_out(path)).then_some(Kind::Code);
        }
        if self.documents && crate::extract::is_document(path) {
            return Some(Kind::Document);
        }
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if self.media && crate::config::MEDIA_EXTENSIONS.contains(&ext.as_str()) {
            return Some(Kind::Media);
        }
        self.images.contains(&ext).then_some(Kind::Image)
    }

    /// Inside vinoWhisper's folders only sessions are taken: anything else
    /// there is a live transcript or a half-written bundle (pinned by
    /// `tests::a_live_transcript_or_half_written_bundle_in_vinowhispers_folder_is_not_taken`,
    /// `tests::a_live_transcript_in_the_flatpaks_folder_is_not_taken_either`).
    fn in_sessions_dir(&self, path: &Path) -> bool {
        self.sessions_dirs.iter().any(|d| path.starts_with(d))
    }
}

/// vinoWhisper's Flatpak, which keeps its data under `~/.var/app`.
pub const VINOWHISPER_FLATPAK: &str = "io.github.karanshukla.vinowhisper";

/// Where vinoWhisper keeps its sessions (karanshukla/vinoWhisper#123),
/// canonical as roots are: a native install's data, and its Flatpak's. In
/// our own Flatpak XDG_DATA_HOME is ours, so a native vinoWhisper's is the
/// real home's.
fn sessions_dirs() -> Vec<PathBuf> {
    let native = match vinosearch_shared::sandbox::flatpak_id() {
        Some(_) => vinosearch_shared::sandbox::host_data_home(),
        None => xdg::data_home().ok(),
    };
    sessions_dirs_in(native, xdg::home().ok())
}

fn sessions_dirs_in(native_data: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let flatpak_data = home.map(|h| h.join(".var/app").join(VINOWHISPER_FLATPAK).join("data"));
    let mut dirs: Vec<PathBuf> = Vec::new();
    for data in [native_data, flatpak_data].into_iter().flatten() {
        let dir = data.join("vinowhisper/transcripts");
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

pub fn default_db() -> Result<PathBuf> {
    Ok(xdg::data_home()?.join("vinosearch/index.sqlite"))
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
            let cut = long_line_cut(rest);
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

/// Where to end a chunk of a line over CHUNK_CHARS: after a space in the back
/// half if there is one (a PDF page's text layer is often a few very long
/// lines), else at the last char boundary that fits (pinned by
/// `tests::long_lines_split_between_words`,
/// `tests::long_lines_split_on_char_boundaries`).
fn long_line_cut(line: &str) -> usize {
    let mut cut = CHUNK_CHARS;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    match line[..cut].rfind(' ').filter(|&i| i >= CHUNK_CHARS / 2) {
        Some(space) => space + 1,
        None => cut,
    }
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

/// Whether a directory between `root` and `path` is hidden or build output.
/// `path`'s own name is for `wanted` or its own walk to judge (pinned by
/// `tests::skipped_under_judges_parent_directories_only`).
pub fn skipped_under(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(Path::parent)
        .is_some_and(|dirs| {
            dirs.components()
                .any(|c| skipped_dir(&c.as_os_str().to_string_lossy()))
        })
}

/// A directory, and not a symlink to one. Following a link would index its
/// target under the link's path, where exclusions (canonical paths) miss it.
pub fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Everything under `root` that indexing looks at, skipping hidden and build
/// directories and anything excluded. Follows no links, `root` included
/// (pinned by `tests::walk_does_not_follow_symlinks`,
/// `tests::a_link_given_as_the_root_is_not_followed`).
pub fn walk<'a>(root: &Path, ex: &'a Excludes) -> impl Iterator<Item = walkdir::DirEntry> + 'a {
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
    let seen = Seen {
        path,
        key,
        kind,
        mtime: meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs() as i64,
        size: meta.len() as i64,
    };
    let known = known_row(conn, key)?;
    let unchanged = known.as_ref().is_some_and(|k| k.is_stamped(&seen));
    if unchanged && known.as_ref().is_some_and(|k| k.sha256.is_some()) {
        return Ok(Outcome::Unchanged);
    }
    let probed = if kind == Kind::Media {
        match probe_media(conn, path, &seen, known.as_ref(), types.max_media_seconds)? {
            ControlFlow::Break(outcome) => return Ok(outcome),
            ControlFlow::Continue(probed) => probed,
        }
    } else {
        None
    };
    let Some(content) = read_and_hash(conn, emb, path, kind, between)? else {
        remove(conn, path)?;
        return Ok(Outcome::Skipped);
    };
    let holder = holder_of(conn, &content.hash, key)?;
    if let Some(known) = known
        && let Some(outcome) = settle_known(conn, &seen, &known, &content.hash, holder.is_some())?
    {
        return Ok(outcome);
    }
    if holder.is_some() {
        seen.record(conn, &content.hash)?;
        if let Some(p) = &probed {
            media::note(conn, key, p)?;
        }
        return Ok(Outcome::Duplicate);
    }
    if kind == Kind::Media {
        return media::start(conn, (key, seen.mtime, seen.size, &content.hash), probed);
    }
    let built = match kind {
        Kind::Media => unreachable!("started above"),
        Kind::Text | Kind::Code => text_chunks(conn, emb, &seen, &content, between)?,
        Kind::Document | Kind::Session => document_chunks(conn, emb, &seen, &content, between)?,
        Kind::Image => image_chunks(conn, emb, &seen, &content, between)?,
    };
    let chunked = match built {
        ControlFlow::Break(outcome) => return Ok(outcome),
        ControlFlow::Continue(chunked) => chunked,
    };
    write_file(conn, &seen, &content.hash, &chunked)
}

/// The file's row, its chunks and, for a long one, what is left for
/// `index_rest`, in one transaction.
fn write_file(
    conn: &mut Connection,
    seen: &Seen,
    hash: &[u8],
    chunked: &Chunked,
) -> Result<Outcome> {
    let Chunked {
        first,
        vectors,
        kinds,
    } = chunked;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM files WHERE path = ?1", [seen.key])?;
    tx.execute(
        "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, ?4)",
        params![seen.key, seen.mtime, seen.size, hash],
    )?;
    let file_id = tx.last_insert_rowid();
    insert_chunks(&tx, file_id, &first.chunks, vectors, kinds)?;
    leave_rest_for_later(&tx, file_id, first.parts)?;
    tx.commit()?;
    Ok(Outcome::Embedded(first.chunks.len()))
}

/// A `partial` row for `index_rest` when `parts` is more than the first run.
fn leave_rest_for_later(tx: &Connection, file_id: i64, parts: usize) -> Result<()> {
    let total = parts.min(MAX_PARTS);
    if total > PARTS_AT_ONCE {
        tx.execute(
            "INSERT INTO partial (file_id, done, total) VALUES (?1, ?2, ?3)",
            params![file_id, PARTS_AT_ONCE as i64, total as i64],
        )?;
    }
    Ok(())
}

/// A file as the walk found it: its path and what the filesystem says of it.
struct Seen<'a> {
    path: &'a Path,
    key: &'a str,
    kind: Kind,
    mtime: i64,
    size: i64,
}

impl Seen<'_> {
    /// A row with no chunks of its own, replacing any earlier one.
    fn record(&self, conn: &mut Connection, hash: &[u8]) -> Result<()> {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM files WHERE path = ?1", [self.key])?;
        tx.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, ?2, ?3, ?4)",
            params![self.key, self.mtime, self.size, hash],
        )?;
        Ok(tx.commit()?)
    }
}

/// The row an earlier run left for a path.
struct Known {
    id: i64,
    mtime: i64,
    size: i64,
    sha256: Option<Vec<u8>>,
}

impl Known {
    fn is_stamped(&self, seen: &Seen) -> bool {
        (self.mtime, self.size) == (seen.mtime, seen.size)
    }
}

fn known_row(conn: &Connection, key: &str) -> Result<Option<Known>> {
    Ok(conn
        .query_row(
            "SELECT id, mtime, size, sha256 FROM files WHERE path = ?1",
            [key],
            |r| {
                Ok(Known {
                    id: r.get(0)?,
                    mtime: r.get(1)?,
                    size: r.get(2)?,
                    sha256: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// What is left to do for a file with a row, once its content is hashed:
/// None when the content is new and has to be indexed.
fn settle_known(
    conn: &Connection,
    seen: &Seen,
    known: &Known,
    hash: &[u8],
    has_holder: bool,
) -> Result<Option<Outcome>> {
    if known.sha256.as_deref() == Some(hash) {
        conn.execute(
            "UPDATE files SET mtime = ?1, size = ?2 WHERE id = ?3",
            params![seen.mtime, seen.size, known.id],
        )?;
        return Ok(Some(Outcome::Unchanged));
    }
    if !known.is_stamped(seen) {
        return Ok(None);
    }
    backfill_hash(conn, known, hash, has_holder).map(Some)
}

/// A row indexed before content hashes gets its hash, and gives its chunks
/// up if another file already holds the same ones (pinned by
/// `index::pipeline_tests::a_file_indexed_before_hashes_gets_its_hash_and_is_not_embedded_again`,
/// `index::pipeline_tests::a_file_indexed_before_hashes_gives_up_its_chunks_to_an_older_copy`).
fn backfill_hash(
    conn: &Connection,
    known: &Known,
    hash: &[u8],
    has_holder: bool,
) -> Result<Outcome> {
    conn.execute(
        "UPDATE files SET sha256 = ?1 WHERE id = ?2",
        params![hash, known.id],
    )?;
    if has_holder && conn.execute("DELETE FROM chunks WHERE file_id = ?1", [known.id])? > 0 {
        return Ok(Outcome::Duplicate);
    }
    Ok(Outcome::Unchanged)
}

/// Media is probed before it is hashed, so a film over the longest indexed is
/// never read whole: it keeps a row with no hash, and that row stands while
/// the file and the limit do.
fn probe_media(
    conn: &mut Connection,
    path: &Path,
    seen: &Seen,
    known: Option<&Known>,
    max_seconds: f64,
) -> Result<ControlFlow<Outcome, Option<crate::extract::media::Probe>>> {
    if let Some(known) = known
        && known.is_stamped(seen)
        && media::left_out(conn, known.id, max_seconds)?
    {
        return Ok(ControlFlow::Break(Outcome::Unchanged));
    }
    Ok(match media::probe(path, max_seconds) {
        media::Probed::TooLong(seconds) => {
            media::leave_out(conn, (seen.key, seen.mtime, seen.size), seconds)?;
            ControlFlow::Break(Outcome::Unreadable { no_decoder: false })
        }
        media::Probed::Ok(p) => ControlFlow::Continue(Some(p)),
        media::Probed::Unreadable => ControlFlow::Continue(None),
    })
}

/// What a file held when it was read, and its sha256.
struct Content {
    bytes: Vec<u8>,
    hash: Vec<u8>,
}

/// None if the file can't be read now. Media is hashed as it streams, and
/// its bytes are left empty; an Err is `between` stopping that, which
/// leaves the file as it was.
fn read_and_hash<E: Embed>(
    conn: &Connection,
    emb: &mut E,
    path: &Path,
    kind: Kind,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Option<Content>> {
    let read = match kind {
        Kind::Text | Kind::Code => fs::read_to_string(path).map(String::into_bytes),
        Kind::Image | Kind::Document | Kind::Session => fs::read(path),
        Kind::Media => Ok(Vec::new()),
    };
    let Ok(bytes) = read else {
        return Ok(None);
    };
    let hash = match kind {
        Kind::Media => match media::hash(path, &mut || between(conn, emb))? {
            Some(h) => h,
            None => return Ok(None),
        },
        _ => Sha256::digest(&bytes).to_vec(),
    };
    Ok(Some(Content { bytes, hash }))
}

/// A file's first parts as chunks, each with its vector and the kind it is
/// stored as.
struct Chunked {
    first: Extract,
    vectors: Vec<Vec<f32>>,
    kinds: Vec<Kind>,
}

impl Chunked {
    fn all_of(kind: Kind, first: Extract, vectors: Vec<Vec<f32>>) -> Self {
        let kinds = vec![kind; first.chunks.len()];
        Self {
            first,
            vectors,
            kinds,
        }
    }
}

type Built = ControlFlow<Outcome, Chunked>;

/// Generated or minified code keeps a row with no chunks, so it isn't read
/// again until it changes (pinned by
/// `index::pipeline_tests::generated_code_is_kept_as_a_row_with_no_chunks_and_not_read_again`).
fn text_chunks<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    seen: &Seen,
    content: &Content,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Built> {
    let text = std::str::from_utf8(&content.bytes).expect("read as a string");
    if let Some(why) = (seen.kind == Kind::Code)
        .then(|| crate::index::code::unwanted(text))
        .flatten()
    {
        eprintln!("skip {}: {why}", seen.path.display());
        seen.record(conn, &content.hash)?;
        return Ok(ControlFlow::Break(Outcome::Unreadable {
            no_decoder: false,
        }));
    }
    let first = text_parts(seen.path, text, 0, PARTS_AT_ONCE);
    let vectors = embed_chunks(conn, emb, &title_of(seen.path), &first.chunks, between)?;
    Ok(ControlFlow::Continue(Chunked::all_of(
        seen.kind, first, vectors,
    )))
}

/// A document that can't be read, or has no text OCR can read, keeps a row
/// with no chunks, so it isn't parsed again until it changes. So does a
/// session in a format this build doesn't read (pinned by
/// `tests::a_document_that_does_not_parse_has_no_chunks`,
/// `index::pipeline_tests::a_session_that_does_not_parse_keeps_a_row_and_is_not_parsed_again`,
/// `index::pipeline_tests::a_session_of_an_unknown_format_is_kept_unread_and_counted`).
fn document_chunks<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    seen: &Seen,
    content: &Content,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Built> {
    // Recorded before parsing, so a file that crashes the parser isn't
    // parsed again at every restart; dropped once it parses, so a stop while
    // embedding leaves it to be indexed next time.
    seen.record(conn, &content.hash)?;
    let mut first =
        document_parts(seen.path, &content.bytes, 0, parser(seen.kind)).unwrap_or_else(|e| {
            eprintln!("skip {}: {e:#}", seen.path.display());
            Extract::default()
        });
    let scan = first.chunks.is_empty() && is_scannable(seen.path) && emb.can_read_text();
    if !first.chunks.is_empty() || scan {
        conn.execute("DELETE FROM files WHERE path = ?1", [seen.key])?;
    }
    if scan {
        first = Extract {
            parts: 1,
            chunks: scan_chunks(conn, emb, seen.path, &content.bytes, between)?,
        };
    }
    let vectors = embed_chunks(conn, emb, &title_of(seen.path), &first.chunks, between)?;
    Ok(ControlFlow::Continue(Chunked::all_of(
        seen.kind, first, vectors,
    )))
}

/// A chunk with no text and line 0 from the pixels, then the text OCR reads
/// in them as document chunks. One that doesn't decode keeps a row with no
/// chunks, so it isn't decoded again at every check (pinned by
/// `index::pipeline_tests::an_image_is_one_chunk_of_its_own_with_no_text`,
/// `index::pipeline_tests::an_image_that_does_not_decode_is_kept_as_a_row_with_no_chunks`).
fn image_chunks<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    seen: &Seen,
    content: &Content,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Built> {
    // From the bytes hashed, so the file can't change in between.
    let decoded = crate::ml::vision::load(&content.bytes)
        .and_then(|img| Ok((crate::ml::vision::prepare(&img)?, img)));
    let (pixels, img) = match decoded {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: {e:#}", seen.path.display());
            seen.record(conn, &content.hash)?;
            return Ok(ControlFlow::Break(Outcome::Unreadable {
                no_decoder: e.downcast_ref::<crate::ml::vision::NoDecoder>().is_some(),
            }));
        }
    };
    let picture = emb.image(&pixels)?;
    drop(pixels);
    between(conn, emb)?;
    let lines = emb.read_text(&img)?;
    let lines = if crate::ml::ocr::text_size(&lines) >= crate::ml::ocr::MIN_IMAGE_TEXT {
        lines
    } else {
        Vec::new()
    };
    let text = chunk(&lines.join("\n"));
    drop(img);
    between(conn, emb)?;
    let mut chunks = vec![(0, String::new())];
    let mut vectors = vec![picture];
    vectors.extend(embed_chunks(
        conn,
        emb,
        &title_of(seen.path),
        &text,
        between,
    )?);
    let mut kinds = vec![Kind::Image];
    kinds.extend(std::iter::repeat_n(Kind::Document, text.len()));
    chunks.extend(text);
    Ok(ControlFlow::Continue(Chunked {
        first: Extract { parts: 1, chunks },
        vectors,
        kinds,
    }))
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
        && crate::ml::ocr::poppler()
}

/// A scanned PDF's pages as OCR reads them, a chunk a page numbered by page,
/// up to `ocr::MAX_SCAN_PAGES` and never left in part. `ocr::SCAN_WALL`
/// bounds poppler's rendering only: OCR and embedding are paced by
/// `between`. The first page that won't render ends the scan with the pages
/// read so far: a file that stalls one page likely stalls them all.
fn scan_chunks<E: Embed>(
    conn: &Connection,
    emb: &mut E,
    path: &Path,
    bytes: &[u8],
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Vec<(usize, String)>> {
    let mut render_deadline = std::time::Instant::now() + crate::ml::ocr::SCAN_WALL;
    let pages = match crate::ml::ocr::pdf_pages(bytes, render_deadline) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("skip {}: {e:#}", path.display());
            return Ok(Vec::new());
        }
    };
    if pages > crate::ml::ocr::MAX_SCAN_PAGES {
        eprintln!(
            "{}: reading the first {} of {pages} scanned pages",
            path.display(),
            crate::ml::ocr::MAX_SCAN_PAGES
        );
    }
    let mut out = Vec::new();
    for page in 1..=pages.min(crate::ml::ocr::MAX_SCAN_PAGES) {
        let started = std::time::Instant::now();
        let img = match crate::ml::ocr::render_page(bytes, page, render_deadline) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("{} page {page}: {e:#}; stopping there", path.display());
                break;
            }
        };
        let rendered = started.elapsed();
        let text = emb.read_text(&img)?.join("\n");
        between(conn, emb)?;
        render_deadline += started.elapsed() - rendered;
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
    } else if crate::index::code::is_code(path) {
        crate::index::code::chunk(text)
    } else {
        chunk(text)
    };
    crate::extract::select(chunks, false, from, count)
}

/// Documents are parsed in a child with limits. A session is plain text in
/// a zip, nothing in it needs a parser that could crash or hang, and each
/// part it unzips is held to 1 MiB, so it's read here.
fn parser(kind: Kind) -> Parser {
    match kind {
        Kind::Session => crate::extract::parts,
        _ => crate::extract::parts_in_child,
    }
}

/// Parts `from..from + PARTS_AT_ONCE` of `path` through `parse`. Err if it
/// doesn't parse, the child was stopped, or the run's text is over
/// MAX_FILE_BYTES (pinned by
/// `tests::a_run_of_parts_over_max_file_bytes_of_text_is_refused`).
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

/// The long document indexed in part longest ago, and what to do with it:
/// documents before media, which is taken only if `media`, and a file the
/// model failed on after those it hasn't.
fn next_rest(conn: &Connection, media: bool) -> Result<Option<Next>> {
    let row = conn
        .query_row(
            "SELECT f.id, f.path, f.mtime, f.size, p.done, p.total
             FROM partial p JOIN files f ON f.id = p.file_id
             WHERE p.media = 0 OR ?1
             ORDER BY p.media, p.tries, p.file_id LIMIT 1",
            [media],
            |r| {
                Ok(Waiting {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    stamp: (r.get(2)?, r.get(3)?),
                    done: r.get(4)?,
                    total: r.get(5)?,
                })
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    let now = fs::symlink_metadata(&row.path)
        .ok()
        .filter(|m| m.is_file())
        .as_ref()
        .and_then(stamp_of);
    if now != Some(row.stamp) {
        return Ok(Some(Next::Again(row.path)));
    }
    Ok(Some(Next::Parts {
        id: row.id,
        path: row.path,
        from: row.done as usize,
        total: row.total as usize,
    }))
}

/// A `partial` row and the stamp its file had when it was recorded.
struct Waiting {
    id: i64,
    path: String,
    stamp: (i64, i64),
    done: i64,
    total: i64,
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
            "UPDATE partial SET done = ?1, tries = 0 WHERE file_id = ?2",
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
/// Media is left waiting, as if there were none, while the models it needs
/// are not fetched: every slice would be decoded only to fail.
pub fn index_rest<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    types: &FileTypes,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Option<Rest>> {
    let Some(next) = next_rest(conn, emb.can_embed_media())? else {
        return Ok(None);
    };
    let (id, key, from, total) = match next {
        Next::Again(path) => return index_again(conn, emb, path, types, between).map(Some),
        Next::Parts {
            id,
            path,
            from,
            total,
        } => (id, path, from, total),
    };
    let path = Path::new(&key);
    // Media isn't hashed again for each slice: next_rest's stamp check
    // stands for that.
    match types.kind(path) {
        Some(Kind::Media) => {
            return media::rest(conn, emb, id, key, from, total, between).map(Some);
        }
        None => return index_again(conn, emb, key, types, between).map(Some),
        _ => {}
    }
    let hash: Option<Vec<u8>> =
        conn.query_row("SELECT sha256 FROM files WHERE id = ?1", [id], |r| r.get(0))?;
    let (kind, bytes) = match (types.kind(path), fs::read(path)) {
        (Some(kind @ (Kind::Text | Kind::Document | Kind::Code | Kind::Session)), Ok(bytes))
            if hash.as_deref() == Some(&Sha256::digest(&bytes)[..]) =>
        {
            (kind, bytes)
        }
        _ => return index_again(conn, emb, key, types, between).map(Some),
    };
    let run = match kind {
        Kind::Document | Kind::Session => document_parts(path, &bytes, from, parser(kind)),
        _ => match String::from_utf8(bytes) {
            Ok(text) => Ok(text_parts(path, &text, from, PARTS_AT_ONCE)),
            Err(_) => return index_again(conn, emb, key, types, between).map(Some),
        },
    };
    let mut done = (from + PARTS_AT_ONCE).min(total);
    let mut chunks = match run {
        Ok(run) => run.chunks,
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

/// `path` indexed afresh, as changed whatever its stamp says.
fn index_again<E: Embed>(
    conn: &mut Connection,
    emb: &mut E,
    path: String,
    types: &FileTypes,
    between: &mut dyn FnMut(&Connection, &mut E) -> Result<()>,
) -> Result<Rest> {
    conn.execute("UPDATE files SET mtime = -1 WHERE path = ?1", [&path])?;
    index_file(conn, emb, Path::new(&path), types, between)?;
    Ok(Rest::Again(path))
}

/// Trim `chunks` to MAX_TEXT_BYTES with `have` already in; true if some went.
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

/// Whether any document is indexed in part only, or any media file if
/// `media`. `index_rest` takes media last, so with documents waiting it
/// takes those whatever this says.
pub fn has_partial(conn: &Connection, media: bool) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM partial WHERE media = 0 OR ?1 LIMIT 1",
            [media],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Audio and video files registered but not fully embedded yet.
pub fn media_partial(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT count(*) FROM partial WHERE media = 1", [], |r| {
            r.get(0)
        })?,
    )
}

/// Documents indexed in part so far (#108), the next in line first: (path,
/// parts in, parts in all). At most `limit`.
pub fn partial(conn: &Connection, limit: usize) -> Result<Vec<(String, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT f.path, p.done, p.total FROM partial p JOIN files f ON f.id = p.file_id
         ORDER BY p.media, p.tries, p.file_id LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Another file with content `hash` that holds its chunks, if there is one,
/// or that waits in `partial` to: a media file has no chunks until
/// `index_rest` gets to it, and a copy found meanwhile is still a copy.
fn holder_of(conn: &Connection, hash: &[u8], except: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT f.id FROM files f WHERE f.sha256 = ?1 AND f.path != ?2
             AND (EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = f.id)
                  OR EXISTS (SELECT 1 FROM partial p WHERE p.file_id = f.id)) LIMIT 1",
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

/// What a file `walk` found is indexed as, with its metadata, or None. Reads no contents.
pub fn indexable(entry: &walkdir::DirEntry, types: &FileTypes) -> Option<(Kind, fs::Metadata)> {
    let kind = types.kind(entry.path())?;
    let meta = entry.metadata().ok()?;
    (meta.len() <= kind.max_bytes(entry.path())).then_some((kind, meta))
}

/// Paths under `root` the index is behind on: wanted files that are new or
/// changed (by mtime and size), and indexed files that are gone, excluded,
/// skipped or no longer wanted. Needs no model, so a runner can check at
/// start whether it has anything to do (pinned by
/// `tests::stale_finds_new_changed_and_gone_files`).
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
            if was.is_some() {
                out.push(path.to_path_buf());
            }
            continue;
        };
        if was != stamp_of(&meta) {
            out.push(path.to_path_buf());
        }
    }
    out.extend(known.into_keys().map(PathBuf::from));
    Ok(out)
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
        let outcome = index_file(conn, emb, entry.path(), types, &mut |c, e| {
            after(&stats, c, e)
        })?;
        if matches!(outcome, Outcome::Skipped)
            && let Some(p) = entry.path().to_str()
        {
            seen.remove(p);
        }
        stats.add(outcome);
        after(&stats, conn, emb)?;
    }
    stats.removed = remove_unseen_under(conn, &root, &seen)? + purged;
    Ok(stats)
}

impl Stats {
    fn add(&mut self, outcome: Outcome) {
        if !matches!(outcome, Outcome::Skipped) {
            self.files_seen += 1;
        }
        match outcome {
            Outcome::Embedded(n) => {
                self.files_embedded += 1;
                self.chunks += n;
            }
            Outcome::Duplicate => self.duplicates += 1,
            Outcome::Unreadable { no_decoder } => self.no_decoder += usize::from(no_decoder),
            Outcome::Unchanged | Outcome::Skipped => {}
        }
    }
}

/// Drop the indexed files under `root` that the walk didn't see; how many.
fn remove_unseen_under(conn: &Connection, root: &Path, seen: &HashSet<String>) -> Result<usize> {
    let gone: Vec<String> = conn
        .prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\'")?
        .query_map([like_under(&root.to_string_lossy())], |r| r.get(0))?
        .filter_map(Result::ok)
        .filter(|p: &String| !seen.contains(p))
        .collect();
    for p in &gone {
        conn.execute("DELETE FROM files WHERE path = ?1", [p])?;
    }
    if !gone.is_empty() {
        compact(conn)?;
    }
    Ok(gone.len())
}

/// Stop treating `root` as a root and drop what was indexed under it, except
/// files another root still walks. Returns files removed (pinned by
/// `tests::forgetting_an_inner_root_keeps_what_the_outer_one_walks`,
/// `tests::forgetting_the_outer_root_keeps_the_roots_inside_it`,
/// `tests::forgetting_a_root_under_a_hidden_directory_drops_all_of_it`).
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

#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::Scratch;
    use super::vectors::{dot, quantize};
    use super::words::{Expression, WORDS_MAX_CHUNKS, expression, ranked};
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
        let dir = Scratch::new("private");
        let db = dir.join("sub/index.sqlite");
        open(&db).unwrap();
        assert_eq!(mode(db.parent().unwrap()), 0o700);
        assert_eq!(mode(&db), 0o600);
    }

    #[test]
    fn a_looser_index_is_tightened_on_open() {
        let dir = Scratch::new("loose");
        let db = dir.join("index.sqlite");
        open(&db).unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        open(&db).unwrap();
        assert_eq!(mode(&db), 0o600);
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
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
    pub(super) fn file_with(conn: &Connection, path: &str, chunks: &[(Kind, i64)]) {
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
        let dir = Scratch::new("ocrcount");
        let conn = dir.db();
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
        let counts: Vec<_> = Kind::ALL
            .into_iter()
            .zip(counts_by_kind(&conn).unwrap())
            .collect();
        assert_eq!(
            counts,
            [
                (Kind::Text, (1, 1)),
                (Kind::Document, (1, 2)),
                (Kind::Image, (1, 3)),
                (Kind::Code, (0, 0)),
                (Kind::Session, (0, 0)),
                (Kind::Media, (0, 0)),
            ]
        );
    }

    #[test]
    fn recent_files_are_text_and_documents_newest_first() {
        let dir = Scratch::new("recent");
        let conn = dir.db();
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
    }

    #[test]
    fn remove_drops_a_file_or_a_whole_directory() {
        let dir = Scratch::new("rm");
        let conn = dir.db();
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
    }

    /// `/a/x.md` holding one chunk, `/b/x.md` its copy and `/c/y.md` other
    /// content: (holder id, copy id, the chunk's vector).
    fn a_file_and_its_copy(conn: &Connection) -> (i64, i64, Vec<f32>) {
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
        (a, b, v)
    }

    fn top_text_hits(conn: &Connection, v: &[f32]) -> Vec<Hit> {
        search(conn, &Vectors::load(conn).unwrap(), v, None, Kind::Text, 5).unwrap()
    }

    #[test]
    fn the_holder_of_a_copys_content_is_the_other_file() {
        let dir = Scratch::new("dup-holder");
        let conn = dir.db();
        let (a, _, _) = a_file_and_its_copy(&conn);
        assert_eq!(holder_of(&conn, b"same", "/b/x.md").unwrap(), Some(a));
        assert_eq!(holder_of(&conn, b"same", "/a/x.md").unwrap(), None);
    }

    #[test]
    fn copies_are_one_hit_naming_the_copy() {
        let dir = Scratch::new("dup-hit");
        let conn = dir.db();
        let (_, _, v) = a_file_and_its_copy(&conn);
        let hits = top_text_hits(&conn, &v);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/a/x.md");
        assert_eq!(hits[0].copies, ["/b/x.md"]);
    }

    #[test]
    fn when_the_holder_goes_its_copy_takes_the_same_chunks() {
        let dir = Scratch::new("dup-inherit");
        let conn = dir.db();
        let (_, b, v) = a_file_and_its_copy(&conn);
        remove(&conn, Path::new("/a")).unwrap();
        let owner: i64 = conn
            .query_row("SELECT file_id FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(owner, b);
        let hits = top_text_hits(&conn, &v);
        assert_eq!(
            (hits[0].path.as_str(), hits[0].copies.len()),
            ("/b/x.md", 0)
        );
    }

    #[test]
    fn when_the_last_copy_goes_so_do_the_chunks() {
        let dir = Scratch::new("dup-last");
        let conn = dir.db();
        a_file_and_its_copy(&conn);
        remove(&conn, Path::new("/a")).unwrap();
        remove(&conn, Path::new("/b/x.md")).unwrap();
        assert_eq!(counts(&conn).unwrap(), (1, 0));
    }

    /// An index from before content hashes and kinds: `/old.md`, one chunk.
    fn old_index(dir: &Path) -> PathBuf {
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
        db
    }

    #[test]
    fn an_old_index_gains_the_hash_and_kind_columns() {
        let dir = Scratch::new("mig");
        let conn = open(&old_index(&dir)).unwrap();
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
    }

    #[test]
    fn an_old_index_gains_its_chunks_in_the_word_index_by_text_and_file_name() {
        let dir = Scratch::new("mig-words");
        let conn = open(&old_index(&dir)).unwrap();
        assert_eq!(matching(&conn, "x"), [1]);
        assert_eq!(matching(&conn, "old"), [1]);
    }

    #[test]
    fn opening_a_migrated_index_again_changes_nothing() {
        let dir = Scratch::new("mig-again");
        let db = old_index(&dir);
        open(&db).unwrap();
        open(&db).unwrap();
        assert_eq!(matching(&open(&db).unwrap(), "x"), [1]);
    }

    /// Roots `/dev`, `/dev/proj` and `/dev/.hidden/p2`, a file in each.
    fn nested_roots(dir: &Scratch) -> Connection {
        let conn = dir.db();
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
        conn
    }

    fn file_paths(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT path FROM files ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn forgetting_an_inner_root_keeps_what_the_outer_one_walks() {
        let dir = Scratch::new("forget-inner");
        let conn = nested_roots(&dir);
        assert_eq!(forget(&conn, Path::new("/dev/proj")).unwrap(), 0);
        assert_eq!(
            file_paths(&conn),
            ["/dev/.hidden/p2/c.md", "/dev/a.md", "/dev/proj/b.md"]
        );
    }

    #[test]
    fn forgetting_the_outer_root_keeps_the_roots_inside_it() {
        let dir = Scratch::new("forget-outer");
        let conn = nested_roots(&dir);
        assert_eq!(forget(&conn, Path::new("/dev")).unwrap(), 1);
        assert_eq!(
            file_paths(&conn),
            ["/dev/.hidden/p2/c.md", "/dev/proj/b.md"]
        );
    }

    #[test]
    fn forgetting_a_root_under_a_hidden_directory_drops_all_of_it() {
        let dir = Scratch::new("forget-hidden");
        let conn = nested_roots(&dir);
        assert_eq!(forget(&conn, Path::new("/dev/.hidden/p2")).unwrap(), 1);
        assert_eq!(file_paths(&conn), ["/dev/a.md", "/dev/proj/b.md"]);
    }

    #[test]
    fn forgetting_a_root_nothing_overlaps_drops_everything_under_it() {
        let dir = Scratch::new("forget-alone");
        let conn = nested_roots(&dir);
        conn.execute_batch("DELETE FROM roots WHERE path != '/dev/proj'")
            .unwrap();
        assert_eq!(forget(&conn, Path::new("/dev/proj")).unwrap(), 1);
    }

    #[test]
    fn largest_folders_name_where_the_bulk_is() {
        let dir = Scratch::new("largest");
        let conn = dir.db();
        conn.execute_batch("INSERT INTO roots VALUES ('/h'), ('/h/dev/app');")
            .unwrap();
        let mut v = vec![0f32; DIM];
        v[0] = 1.0;
        for (path, n) in [
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
    }

    #[test]
    fn config_files_are_not_indexed() {
        for f in ["a.toml", "b.yaml", "c.json", "d.ini", "e.conf", ".env"] {
            assert!(!text_file(Path::new(f)), "{f}");
        }
        assert!(text_file(Path::new("notes.md")));
    }

    /// `root/real/a.md`, and `root/link` to a directory outside `root`.
    fn a_tree_with_a_link_out(name: &str) -> Scratch {
        let dir = Scratch::new(name);
        fs::create_dir_all(dir.join("root/real")).unwrap();
        fs::create_dir_all(dir.join("secret")).unwrap();
        fs::write(dir.join("root/real/a.md"), "a").unwrap();
        fs::write(dir.join("secret/tax.md"), "s").unwrap();
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("root/link")).unwrap();
        dir
    }

    fn walked_files(root: &Path) -> Vec<PathBuf> {
        walk(root, &Excludes::default())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .collect()
    }

    #[test]
    fn walk_does_not_follow_symlinks() {
        let dir = a_tree_with_a_link_out("links");
        assert_eq!(
            walked_files(&dir.join("root")),
            [dir.join("root/real/a.md")]
        );
        assert!(!is_real_dir(&dir.join("root/link")));
        assert!(is_real_dir(&dir.join("root/real")));
    }

    /// As the watcher walks a created path.
    #[test]
    fn a_link_given_as_the_root_is_not_followed() {
        let dir = a_tree_with_a_link_out("link-root");
        assert!(walked_files(&dir.join("root/link")).is_empty());
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
    }

    #[test]
    fn code_is_off_unless_turned_on() {
        assert_eq!(FileTypes::default().kind(Path::new("/p/app.js")), None);
    }

    const TRANSCRIPTS: &str = "/h/.local/share/vinowhisper/transcripts";
    const SESSION: &str = "2026-10-27-140512.vwsession";

    #[test]
    fn sessions_are_taken_anywhere_while_on() {
        let dir = PathBuf::from(TRANSCRIPTS);
        let on = FileTypes::default().with_sessions(true, Some(dir.clone()));
        let off = FileTypes::default().with_sessions(false, Some(dir.clone()));
        let s = SESSION;
        assert_eq!(on.kind(&dir.join(s)), Some(Kind::Session));
        assert_eq!(
            on.kind(&Path::new("/h/Documents").join(s)),
            Some(Kind::Session)
        );
        assert_eq!(off.kind(&dir.join(s)), None);
        assert_eq!(off.kind(&Path::new("/h/Documents").join(s)), None);
        assert_eq!(on.sessions_dirs(), (&[dir][..], true));
    }

    #[test]
    fn a_live_transcript_or_half_written_bundle_in_vinowhispers_folder_is_not_taken() {
        let dir = PathBuf::from(TRANSCRIPTS);
        let on = FileTypes::default().with_sessions(true, Some(dir.clone()));
        assert_eq!(on.kind(&dir.join("2026-10-28-090000.txt")), None);
        assert_eq!(on.kind(&dir.join(format!(".{SESSION}.k3j.tmp"))), None);
    }

    #[test]
    fn sessions_are_looked_for_in_a_native_install_and_in_the_flatpak() {
        let dirs = sessions_dirs_in(Some("/h/.local/share".into()), Some("/h".into()));
        assert_eq!(
            dirs,
            [
                PathBuf::from("/h/.local/share/vinowhisper/transcripts"),
                PathBuf::from(
                    "/h/.var/app/io.github.karanshukla.vinowhisper/data/vinowhisper/transcripts"
                ),
            ]
        );
        assert!(sessions_dirs_in(None, None).is_empty());
    }

    #[test]
    fn a_live_transcript_in_the_flatpaks_folder_is_not_taken_either() {
        let dirs = sessions_dirs_in(Some("/h/.local/share".into()), Some("/h".into()));
        let types = FileTypes {
            sessions: true,
            sessions_dirs: dirs.clone(),
            ..FileTypes::default()
        };
        assert_eq!(types.kind(&dirs[1].join("2026-11-14-090000.txt")), None);
        assert_eq!(
            types.kind(&dirs[1].join("2026-11-14-090000.vwsession")),
            Some(Kind::Session)
        );
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
        let dir = Scratch::new("stale");
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
        let dir = Scratch::new("search");
        let conn = dir.db();
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
        let dir = Scratch::new("docs");
        let conn = dir.db();
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
    }

    #[test]
    fn a_document_that_does_not_parse_has_no_chunks() {
        let parse = crate::extract::parts;
        assert!(document_parts(Path::new("/p/a.pdf"), b"not a pdf", 0, parse).is_err());
    }

    #[test]
    fn a_run_of_parts_over_max_file_bytes_of_text_is_refused() {
        let one_slide_over = format!(
            "<draw:page><text:p>{}</text:p></draw:page>",
            "word ".repeat(250_000)
        );
        let parse = crate::extract::parts;
        assert!(
            document_parts(Path::new("/p/a.fodp"), one_slide_over.as_bytes(), 0, parse).is_err()
        );
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
    fn media_partial_counts_audio_and_video_only() {
        let dir = Scratch::new("media-wait");
        let conn = dir.db();
        let book = dir.join("book.txt");
        let clip = dir.join("clip.mp4");
        fs::write(&book, "a long book").unwrap();
        fs::write(&clip, "frames").unwrap();
        add_partial(&conn, &book, 30, 70);
        assert_eq!(media_partial(&conn).unwrap(), 0);
        let id = add_partial(&conn, &clip, 0, 22);
        conn.execute("UPDATE partial SET media = 1 WHERE file_id = ?1", [id])
            .unwrap();
        assert_eq!(media_partial(&conn).unwrap(), 1);
    }

    #[test]
    fn the_rest_of_a_long_document_resumes_where_it_stopped() {
        let dir = Scratch::new("rest");
        let db = dir.join("i.sqlite");
        let mut conn = open(&db).unwrap();
        let book = dir.join("book.txt");
        fs::write(&book, "a long book").unwrap();
        let key = book.to_str().unwrap().to_owned();
        let id = add_partial(&conn, &book, 30, 70);
        assert!(has_partial(&conn, true).unwrap());
        assert_eq!(partial(&conn, 10).unwrap(), [(key.clone(), 30, 70)]);
        let parts = |from| Next::Parts {
            id,
            path: key.clone(),
            from,
            total: 70,
        };
        assert_eq!(next_rest(&conn, true).unwrap(), Some(parts(30)));
        let run = [(31, "second".to_owned())];
        store_rest(&mut conn, id, Kind::Text, 60, 70, &run, &[vec![0.0; DIM]]).unwrap();
        drop(conn);
        let mut conn = open(&db).unwrap();
        assert_eq!(next_rest(&conn, true).unwrap(), Some(parts(60)));
        let run = [(61, "third".to_owned())];
        store_rest(&mut conn, id, Kind::Text, 70, 70, &run, &[vec![0.0; DIM]]).unwrap();
        assert_eq!(next_rest(&conn, true).unwrap(), None);
        assert!(!has_partial(&conn, true).unwrap());
        assert_eq!(counts(&conn).unwrap(), (1, 3));
        assert_eq!(counts_by_kind(&conn).unwrap()[0], (1, 3));
    }

    #[test]
    fn a_long_document_that_changed_or_went_is_indexed_again() {
        let dir = Scratch::new("again");
        let conn = dir.db();
        let book = dir.join("book.txt");
        fs::write(&book, "a long book").unwrap();
        let key = book.to_str().unwrap().to_owned();
        let id = add_partial(&conn, &book, 30, 70);
        fs::write(&book, "a longer book").unwrap();
        assert_eq!(
            next_rest(&conn, true).unwrap(),
            Some(Next::Again(key.clone()))
        );
        fs::remove_file(&book).unwrap();
        assert_eq!(next_rest(&conn, true).unwrap(), Some(Next::Again(key)));
        conn.execute("DELETE FROM files WHERE id = ?1", [id])
            .unwrap();
        assert!(!has_partial(&conn, true).unwrap());
    }

    #[test]
    fn a_copy_takes_over_a_long_document_indexed_in_part() {
        let dir = Scratch::new("hand");
        let conn = dir.db();
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
        assert!(!has_partial(&conn, true).unwrap());
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
    fn text_exactly_at_the_budget_is_kept() {
        let mut run = vec![(1, "x".repeat(600))];
        assert!(!text_budget(&mut run, MAX_TEXT_BYTES - 600));
        assert_eq!(run.len(), 1);
    }

    #[test]
    fn text_one_byte_past_the_budget_is_left_out() {
        let mut run = vec![(1, "x".repeat(600))];
        assert!(text_budget(&mut run, MAX_TEXT_BYTES - 599));
        assert!(run.is_empty());
    }

    fn indexable_html_of(name: &str, len: u64) -> Option<Kind> {
        let dir = Scratch::new(name);
        let file = fs::File::create(dir.join("page.html")).unwrap();
        file.set_len(len).unwrap();
        let entry = WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .find(|e| e.file_type().is_file())
            .unwrap();
        indexable(&entry, &FileTypes::default()).map(|(k, _)| k)
    }

    #[test]
    fn a_file_exactly_at_its_kinds_size_cap_is_indexable() {
        assert_eq!(
            indexable_html_of("cap-at", MAX_FILE_BYTES),
            Some(Kind::Text)
        );
    }

    #[test]
    fn a_file_one_byte_over_its_kinds_size_cap_is_not_indexable() {
        assert_eq!(indexable_html_of("cap-over", MAX_FILE_BYTES + 1), None);
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
    fn kinds_are_searched_apart_and_purged_when_turned_off() {
        let dir = Scratch::new("kinds");
        let conn = dir.db();
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
        remove(&conn, Path::new("/notes.md")).unwrap();
        add_file(&conn, "/b.md", &[0]);
        v.refresh(&conn).unwrap();
        assert_eq!(paths(&v, Kind::Image), ["/cat.png"]);
        assert_eq!(paths(&v, Kind::Text), ["/b.md"]);

        assert_eq!(purge_types(&conn, &images_on()).unwrap(), 0);
        assert_eq!(purge_types(&conn, &FileTypes::default()).unwrap(), 1);
        v.refresh(&conn).unwrap();
        assert!(paths(&v, Kind::Image).is_empty());
    }

    #[test]
    fn counts_are_split_by_kind() {
        let dir = Scratch::new("bykind");
        let conn = dir.db();
        add_file(&conn, "/notes.md", &[0, 1]);
        add_file(&conn, "/more.md", &[2]);
        add_file(&conn, "/cat.png", &[3]);
        conn.execute("UPDATE chunks SET kind = 1 WHERE file_id = (SELECT id FROM files WHERE path = '/cat.png')", []).unwrap();
        assert_eq!(
            counts_by_kind(&conn).unwrap(),
            [(2, 3), (0, 0), (1, 1), (0, 0), (0, 0), (0, 0)]
        );
        assert_eq!(counts(&conn).unwrap(), (3, 4));
    }

    #[test]
    fn a_document_recorded_without_text_counts_under_no_kind() {
        let dir = Scratch::new("bykind-empty");
        let conn = dir.db();
        conn.execute(
            "INSERT INTO files (path, mtime, size) VALUES ('/scan.pdf', 0, 0)",
            [],
        )
        .unwrap();
        assert_eq!(counts_by_kind(&conn).unwrap(), [(0, 0); 6]);
        assert_eq!(counts(&conn).unwrap(), (1, 0));
    }

    #[test]
    fn an_indexed_image_gives_its_vector_and_so_does_a_copy() {
        let dir = Scratch::new("simv");
        let conn = dir.db();
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
    }

    /// A unit vector along `axis`, as an embedding blob.
    pub(super) fn axis(axis: usize) -> Vec<u8> {
        let mut v = vec![0f32; DIM];
        v[axis] = 1.0;
        to_blob(&v)
    }

    pub(super) fn add_file(conn: &Connection, path: &str, axes: &[usize]) -> Vec<i64> {
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
        let terms = expression(query).unwrap().terms;
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
        let dir = Scratch::new("fts");
        let conn = dir.db();
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
            if let Some(Expression { terms, .. }) = expression(q) {
                assert!(ranked(&conn, &terms).is_ok(), "{q} -> {terms:?}");
            }
        }
        assert!(matching(&conn, "text:end").is_empty());
        for q in ["", "  ", "***", "\"\"", "- : ()"] {
            assert!(expression(q).is_none(), "{q}");
        }
    }

    #[test]
    fn operators_are_words_like_any_other() {
        let dir = Scratch::new("fts-ops");
        let conn = dir.db();
        add_file(&conn, "/a.md", &[0]);
        conn.execute("UPDATE chunks SET text = 'near the end, or not'", [])
            .unwrap();
        assert_eq!(matching(&conn, "near or not"), [1]);
        assert_eq!(matching(&conn, "end NOT near"), [1]);
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
            assert!(expression(q).unwrap().exact, "{q}");
        }
        for q in [
            "invoice",
            "Karan",
            "don't",
            "invoice INV-20431",
            "\"a\" b \"c\"",
            "photos from 2019",
        ] {
            assert!(!expression(q).unwrap().exact, "{q}");
        }
    }

    /// Three invoice notes: along the query's axis 0, "invoices in general"
    /// leads, then the march one; the one with the number is far off it.
    /// The numbered chunk's id comes back.
    fn invoices(conn: &Connection) -> i64 {
        let general = add_file(conn, "/inv/general.md", &[0])[0];
        let near = add_file(conn, "/inv/near.md", &[0])[0];
        let numbered = add_file(conn, "/inv/march.md", &[1])[0];
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
        numbered
    }

    /// The top `k` text hits along axis 0, boosted by `words`.
    fn along_axis_0(conn: &Connection, words: &str, k: usize) -> Vec<String> {
        let vectors = Vectors::load(conn).unwrap();
        let words = (!words.is_empty()).then(|| Words::find(conn, words).unwrap());
        let mut q = vec![0f32; DIM];
        q[0] = 1.0;
        search(conn, &vectors, &q, words.as_ref(), Kind::Text, k)
            .unwrap()
            .into_iter()
            .map(|h| h.path)
            .collect()
    }

    #[test]
    fn without_words_the_closest_vector_leads() {
        let dir = Scratch::new("hybrid-none");
        let conn = dir.db();
        invoices(&conn);
        assert_eq!(along_axis_0(&conn, "", 1), ["/inv/general.md"]);
    }

    #[test]
    fn an_identifier_leads_whatever_its_cosine() {
        let dir = Scratch::new("hybrid-id");
        let conn = dir.db();
        invoices(&conn);
        assert_eq!(along_axis_0(&conn, "INV-20431", 1), ["/inv/march.md"]);
        assert_eq!(
            along_axis_0(&conn, "INV-20431", 3),
            ["/inv/march.md", "/inv/general.md", "/inv/near.md"]
        );
    }

    #[test]
    fn plain_words_nudge_a_near_tie_but_do_not_lift_a_far_chunk() {
        let dir = Scratch::new("hybrid-plain");
        let conn = dir.db();
        invoices(&conn);
        for words in ["march invoice", "invoice total"] {
            assert_eq!(
                along_axis_0(&conn, words, 2),
                ["/inv/near.md", "/inv/general.md"],
                "{words}"
            );
        }
    }

    /// An image has no text but its name.
    #[test]
    fn file_names_count_as_words() {
        let dir = Scratch::new("hybrid-name");
        let conn = dir.db();
        let numbered = invoices(&conn);
        let name = Words::find(&conn, "march.md").unwrap();
        assert!(name.ids.contains(&numbered));
    }

    #[test]
    fn words_in_too_many_chunks_are_not_looked_up() {
        let dir = Scratch::new("common");
        let conn = dir.db();
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
        assert_eq!(Words::find(&conn, "common rare").unwrap().ids.len(), 1);
        assert_eq!(Words::find(&conn, "rare common").unwrap().ids.len(), 1);
        assert!(Words::find(&conn, "absent").unwrap().ids.is_empty());
    }

    #[test]
    fn the_word_index_follows_chunks() {
        let dir = Scratch::new("words");
        let conn = dir.db();
        conn.execute_batch(
            "INSERT INTO files (path, mtime, size, sha256) VALUES ('/a/first.md', 0, 0, x'01');
             INSERT INTO chunks (file_id, start_line, text, embedding) VALUES (1, 1, 'zebra', x'');
             INSERT INTO files (path, mtime, size, sha256) VALUES ('/b/second.md', 0, 0, x'01');",
        )
        .unwrap();
        assert_eq!(matching(&conn, "zebra"), [1]);
        assert_eq!(matching(&conn, "first"), [1]);
        remove(&conn, Path::new("/a")).unwrap();
        assert_eq!(matching(&conn, "zebra second"), [1]);
        assert!(matching(&conn, "first").is_empty());
        remove(&conn, Path::new("/b")).unwrap();
        assert!(matching(&conn, "zebra").is_empty());
    }

    /// A reader's vectors, loaded, and a second connection that writes as
    /// `vinosearch index` or the watcher would: `/base.md` with 20 chunks,
    /// `/a.md` with 3.
    fn reader_and_writer(dir: &Scratch) -> (Connection, Connection, Vectors) {
        let conn = dir.db();
        let writer = dir.db();
        add_file(&writer, "/base.md", &(0..20).collect::<Vec<_>>());
        add_file(&writer, "/a.md", &[20, 21, 22]);
        let v = Vectors::load(&conn).unwrap();
        assert_eq!(v.len(), 23);
        (conn, writer, v)
    }

    /// Caught up incrementally, to what loading from scratch gives.
    fn assert_caught_up(conn: &Connection, v: &mut Vectors) {
        assert!(v.catch_up(conn).unwrap(), "fell back to a full reload");
        assert_eq!(contents(v), contents(&Vectors::load(conn).unwrap()));
    }

    #[test]
    fn refresh_applies_new_chunks_and_a_replaced_file() {
        let dir = Scratch::new("refresh-new");
        let (conn, writer, mut v) = reader_and_writer(&dir);
        add_file(&writer, "/c.md", &[23]);
        remove(&writer, Path::new("/a.md")).unwrap();
        add_file(&writer, "/a.md", &[24, 25]);
        assert_caught_up(&conn, &mut v);
        assert_eq!(v.len(), 23);
        assert_caught_up(&conn, &mut v);
    }

    /// SQLite hands out max(id) + 1, so a new chunk can get a deleted one's id.
    #[test]
    fn refresh_applies_a_reused_id_with_its_new_vector() {
        let dir = Scratch::new("refresh-reuse");
        let (conn, writer, mut v) = reader_and_writer(&dir);
        let max: i64 = writer
            .query_row("SELECT max(id) FROM chunks", [], |r| r.get(0))
            .unwrap();
        remove(&writer, Path::new("/a.md")).unwrap();
        assert_eq!(add_file(&writer, "/d.md", &[26, 27, 28]).last(), Some(&max));
        assert_caught_up(&conn, &mut v);
    }

    #[test]
    fn refresh_reloads_in_full_once_the_log_is_pruned_past_it() {
        let dir = Scratch::new("refresh-pruned");
        let (conn, writer, mut v) = reader_and_writer(&dir);
        add_file(&writer, "/e.md", &[28]);
        writer.execute("DELETE FROM chunk_log", []).unwrap();
        assert!(!v.catch_up(&conn).unwrap());
        v.refresh(&conn).unwrap();
        assert_eq!(contents(&v), contents(&Vectors::load(&conn).unwrap()));
    }

    #[test]
    fn refresh_reloads_in_full_when_most_of_the_index_changed() {
        let dir = Scratch::new("refresh-most");
        let (conn, writer, mut v) = reader_and_writer(&dir);
        remove(&writer, Path::new("/base.md")).unwrap();
        assert!(!v.catch_up(&conn).unwrap());
    }
}
