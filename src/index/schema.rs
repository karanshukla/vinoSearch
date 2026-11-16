//! Opening the index: its file modes, key, tables, triggers and the
//! changes that bring an older database up to date.

use super::Kind;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Log entries kept. A Vectors further behind than this reloads in full,
/// which at that many changes is no slower than applying them.
const CHUNK_LOG_KEEP: i64 = 100_000;

/// The index holds copies of file contents, so only the user may read it:
/// the directory 0700, the database 0600 (SQLite gives its -wal and -shm
/// files the database's mode). Keyed with its sealed key when it has one; a
/// key that won't unseal is an error (`encryption::Unusable`), never an
/// empty or plaintext index.
pub fn open(path: &Path) -> Result<Connection> {
    let key = crate::index::encryption::key_for(path)?;
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
    crate::index::encryption::unlock(&conn, key.as_deref())?;
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
         -- Long documents indexed in part (#108): `done` of their `total`
         -- parts have chunks, the rest wait for `index_rest`. Gone once the
         -- last part is in, and with the file. A copy takes it over with
         -- the chunks (`hand_over`).
         CREATE TABLE IF NOT EXISTS partial (
             file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
             done INTEGER NOT NULL,
             total INTEGER NOT NULL
         );
         -- How long each audio or video file runs (#187), and whether it is
         -- left out for running over `formats.media.max_hours`, so a change
         -- to that setting reaches files already seen (`media::fit`).
         CREATE TABLE IF NOT EXISTS media (
             file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
             seconds REAL NOT NULL,
             too_long INTEGER NOT NULL DEFAULT 0
         );",
    )?;
    // Media in part (#187) waits for every document's rest, and only on AC.
    // `tries` counts the times the model failed on a file's next run: one
    // that keeps failing goes behind the others instead of ahead of them.
    for (column, sql) in [
        ("media", "media INTEGER NOT NULL DEFAULT 0"),
        ("tries", "tries INTEGER NOT NULL DEFAULT 0"),
    ] {
        let has: bool = conn.query_row(
            "SELECT count(*) FROM pragma_table_info('partial') WHERE name = ?1",
            [column],
            |r| r.get(0),
        )?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE partial ADD COLUMN {sql}"))?;
        }
    }
    hand_over(&conn)?;
    words_table(&conn)?;
    code_apart(&conn)?;
    Ok(conn)
}

/// A file whose content another file already has keeps no chunks of its
/// own. When the one holding them goes (deleted, changed, forgotten), they
/// pass to a copy instead of being dropped, and so does its partial row.
/// Chunk ids don't change, so loaded Vectors stay valid.
///
/// One trigger, so both go to the same copy, and only when no copy holds
/// chunks or a partial row of its own: the heir would hold every chunk
/// twice, or two partial rows. That can't come about any more, since a file
/// waiting in `partial` counts as holding its content (`holder_of`), but
/// before it did two copies of a film each got a row (#187), and deleting
/// either failed on `partial.file_id`. Those indexes have the two triggers
/// this replaces: they go, and of files with the same content that each
/// hold it, all but the first become plain copies.
fn hand_over(conn: &Connection) -> Result<()> {
    let old: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'trigger'
         AND name IN ('files_hand_over', 'partial_hand_over')",
        [],
        |r| r.get(0),
    )?;
    if old > 0 {
        let holds = |f: &str| {
            format!(
                "(EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = {f}.id)
                  OR EXISTS (SELECT 1 FROM partial p WHERE p.file_id = {f}.id))"
            )
        };
        conn.execute_batch(&format!(
            "BEGIN;
             DROP TRIGGER IF EXISTS files_hand_over;
             DROP TRIGGER IF EXISTS partial_hand_over;
             CREATE TEMP TABLE second_holders AS
                 SELECT f.id FROM files f
                 WHERE f.sha256 IS NOT NULL AND {}
                   AND EXISTS (SELECT 1 FROM files g
                               WHERE g.sha256 = f.sha256 AND g.id < f.id AND {});
             DELETE FROM chunks WHERE file_id IN (SELECT id FROM second_holders);
             DELETE FROM partial WHERE file_id IN (SELECT id FROM second_holders);
             DROP TABLE second_holders;
             COMMIT;",
            holds("f"),
            holds("g"),
        ))?;
    }
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS hand_over BEFORE DELETE ON files
         WHEN OLD.sha256 IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM files f WHERE f.sha256 = OLD.sha256 AND f.id != OLD.id
                 AND (EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = f.id)
                      OR EXISTS (SELECT 1 FROM partial p WHERE p.file_id = f.id)))
         BEGIN
             UPDATE chunks
             SET file_id = (SELECT min(id) FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id)
             WHERE file_id = OLD.id
               AND EXISTS (SELECT 1 FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id);
             UPDATE partial
             SET file_id = (SELECT min(id) FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id)
             WHERE file_id = OLD.id
               AND EXISTS (SELECT 1 FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id);
         END;",
    )?;
    Ok(())
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
    files.retain(|(_, p)| crate::index::code::is_code(Path::new(p)));
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
        if let Some(why) = crate::index::code::unwanted(&all) {
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
