//! The index encrypted at rest (#4): SQLCipher over the whole database, so
//! chunk text, vectors, file names and the word index's shadow tables (#104)
//! are all covered, with a random key sealed by `systemd-creds` (seal.rs)
//! in `index.key` beside it.
//!
//! The key file is what says the index is encrypted. Every open unseals it
//! (once per process, then held), and a key that won't unseal or doesn't open
//! the index is never answered with a plaintext index: `apply`, run at start
//! and when the setting changes, rebuilds the index from the files instead.
//! The folder list survives that in `index.roots`, beside the key, in
//! plaintext: folder paths, as `exclude` in config.toml already holds.
//!
//! The vector cache in memory is not locked or wiped on unload: anyone who
//! can read this process's memory is this user, who can unseal the key
//! anyway. Encryption here is for the index at rest: in a backup or synced
//! copy of the home folder, on a disk taken out, or read by another user.

use crate::index;
use crate::seal::{self, Key, Seal, Sealer};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, Weak};
use vinosearch_shared::config::Config;
use vinosearch_shared::dbus::seal as seal_name;

/// The sealed key, beside the database: `index.key` for `index.sqlite`.
pub fn key_file(db: &Path) -> PathBuf {
    db.with_extension("key")
}

/// The roots, one per line, kept while the index is encrypted so a lost key
/// costs the index and not the list of folders in it.
pub fn roots_file(db: &Path) -> PathBuf {
    db.with_extension("roots")
}

fn sibling(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// The index's key can't be used: it doesn't unseal (TPM cleared or
/// replaced, the host key changed), it's gone, or it doesn't open the index.
#[derive(Debug)]
pub struct Unusable(pub String);

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "the index's key can't be used: {}", self.0)
    }
}

impl std::error::Error for Unusable {}

pub fn is_unusable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Unusable>().is_some()
}

/// Keys this process holds, by database: unsealed once, then reused while
/// the key file is unchanged.
enum Held {
    Sealed {
        credential: Vec<u8>,
        key: Arc<Key>,
        seal: Seal,
    },
    /// Nothing could seal it: lost when the process ends.
    Memory(Arc<Key>),
}

static HELD: Mutex<Vec<(PathBuf, Held)>> = Mutex::new(Vec::new());

fn held() -> MutexGuard<'static, Vec<(PathBuf, Held)>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

fn hold(db: &Path, h: Option<Held>) {
    let mut all = held();
    all.retain(|(p, _)| p != db);
    if let Some(h) = h {
        all.push((db.to_owned(), h));
    }
}

/// Encrypt `db` with a key kept in memory only, for a throwaway index
/// (`vinosearch eval --encrypted`).
pub fn hold_in_memory(db: &Path) -> Result<()> {
    hold(db, Some(Held::Memory(Arc::new(Key::random()?))));
    Ok(())
}

/// The key `db` opens with; None while it is plaintext.
pub fn key_for(db: &Path) -> Result<Option<Arc<Key>>> {
    key_with(db, &Sealer::default())
}

fn key_with(db: &Path, sealer: &Sealer) -> Result<Option<Arc<Key>>> {
    let credential = match fs::read(key_file(db)) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(held().iter().find_map(|(p, h)| match h {
                Held::Memory(k) if p == db => Some(k.clone()),
                _ => None,
            }));
        }
        Err(e) => return Err(e).context(key_file(db).display().to_string()),
    };
    if let Some(k) = held().iter().find_map(|(p, h)| match h {
        Held::Sealed {
            credential: c, key, ..
        } if p == db && *c == credential => Some(key.clone()),
        _ => None,
    }) {
        return Ok(Some(k));
    }
    let key = Arc::new(
        sealer
            .unseal(&credential)
            .map_err(|e| Unusable(format!("{e:#}")))?,
    );
    let seal = seal::sealed_with(&credential).unwrap_or(Seal::Tpm);
    hold(
        db,
        Some(Held::Sealed {
            credential,
            key: key.clone(),
            seal,
        }),
    );
    Ok(Some(key))
}

/// Key `conn` (None leaves it plaintext) and check it reads: a wrong key, or
/// none for an encrypted file, is `Unusable`.
pub fn unlock(conn: &Connection, key: Option<&Key>) -> Result<()> {
    if let Some(key) = key {
        let raw = key.raw();
        // SAFETY: the handle is open, and SQLCipher copies the key.
        let rc = unsafe {
            rusqlite::ffi::sqlite3_key(
                conn.handle(),
                raw.as_ptr().cast(),
                raw.len() as std::ffi::c_int,
            )
        };
        if rc != rusqlite::ffi::SQLITE_OK {
            bail!("sqlite3_key failed ({rc})");
        }
    }
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::NotADatabase => {
            Err(Unusable(match key {
                Some(_) => "it doesn't open the index".into(),
                None => "the index is encrypted and its key file is gone".into(),
            })
            .into())
        }
        Err(e) => Err(e.into()),
    }
}

/// `db` read-only, as it is: not created, not converted.
pub fn open_read_only(db: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    unlock(&conn, key_for(db)?.as_deref())?;
    Ok(conn)
}

/// Encryption as the config file asks for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Want {
    pub on: bool,
    pub allow_host: bool,
}

impl Want {
    pub fn of(c: &Config) -> Self {
        Want {
            on: c.encryption.on(),
            allow_host: c.encryption.host_key_allowed(),
        }
    }
}

/// Where encryption stands, for Status.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub encrypted: bool,
    /// A `dbus::seal` value; empty while plaintext.
    pub seal: &'static str,
    /// Why the last change was refused or failed.
    pub error: String,
    /// Why the index was rebuilt from the files.
    pub broken: String,
}

fn state_of(db: &Path) -> State {
    let seal = held().iter().find_map(|(p, h)| match h {
        _ if p != db => None,
        Held::Sealed { seal, .. } => Some(seal.name()),
        Held::Memory(_) => Some(seal_name::MEMORY),
    });
    let seal = match seal {
        Some(s) if key_file(db).exists() || s == seal_name::MEMORY => s,
        _ => "",
    };
    State {
        encrypted: !seal.is_empty(),
        seal,
        ..State::default()
    }
}

/// Bring `db` to `want`: encrypt it, decrypt it, or with `rekey` re-encrypt
/// it under a new key, converting what is there (sqlcipher_export), so
/// nothing needs indexing again. A key that can't be used rebuilds the index
/// from the files instead. Never leaves plaintext where encryption was: a
/// lost key with nothing to seal a new one holds the new one in memory.
///
/// Connections opened through `Live` are closed meanwhile and reopen on
/// their next use; any other this process holds on `db` must be closed
/// first. Another process with the index open (`vinosearch index` beside the
/// runner) holds it shared, and the change is refused until it is done.
pub fn apply(db: &Path, want: Want, rekey: bool, sealer: &Sealer) -> State {
    let mut broken = String::new();
    let mut st = match apply_inner(db, want, rekey, sealer, &mut broken) {
        Ok(()) => state_of(db),
        Err(e) => State {
            error: format!("{e:#}"),
            ..state_of(db)
        },
    };
    st.broken = broken;
    st
}

fn apply_inner(
    db: &Path,
    want: Want,
    rekey: bool,
    sealer: &Sealer,
    broken: &mut String,
) -> Result<()> {
    if rekey && !want.on {
        bail!("encryption is off: there's no key to replace");
    }
    // Nothing to change: no lock, so `vinosearch index` beside the runner
    // just opens it.
    if !rekey && settled(db, want, sealer) {
        return Ok(());
    }
    let _lock = Lock::exclusive(db)?;
    let _closed = close_all(db);
    // What opens it now.
    let current = key_with(db, sealer).and_then(|k| {
        if db.exists() {
            let conn = Connection::open(db)?;
            unlock(&conn, k.as_deref())?;
        }
        Ok(k)
    });
    let current = match current {
        Ok(k) => k,
        Err(e) if is_unusable(&e) => {
            let roots = read_roots(db);
            wipe(db)?;
            hold(db, None);
            *broken = format!("{e:#}");
            eprintln!("{e:#}: the index is rebuilt from the files");
            // A fresh index as asked for, holding the folders it had.
            let key = match new_key(want, sealer) {
                Some(Ok((key, credential, seal))) => {
                    write_key(db, &credential)?;
                    hold(
                        db,
                        Some(Held::Sealed {
                            credential,
                            key,
                            seal,
                        }),
                    );
                    true
                }
                // Encrypted before, and wanted encrypted: never plaintext.
                Some(Err(e)) => {
                    hold(db, Some(Held::Memory(Arc::new(Key::random()?))));
                    eprintln!("{e:#}: the new index's key is kept in memory only");
                    true
                }
                None => false,
            };
            let conn = index::open(db)?;
            for r in &roots {
                conn.execute("INSERT OR IGNORE INTO roots (path) VALUES (?1)", [r])?;
            }
            if key {
                save_roots(db, &conn)?;
            }
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let memory = held()
        .iter()
        .any(|(p, h)| p == db && matches!(h, Held::Memory(_)));
    let sealed = current.is_some() && !memory;
    if want.on {
        if sealed && !rekey {
            return Ok(());
        }
        let (key, credential, seal) = new_key(want, sealer).expect("wanted on")?;
        convert(db, current.as_deref(), Some((&key, &credential)))?;
        hold(
            db,
            Some(Held::Sealed {
                credential,
                key,
                seal,
            }),
        );
        save_roots(db, &index::open(db)?)?;
        eprintln!("index encrypted, its key sealed with {}", seal.name());
    } else if current.is_some() {
        convert(db, current.as_deref(), None)?;
        hold(db, None);
        remove(&roots_file(db))?;
        eprintln!("index decrypted");
    }
    Ok(())
}

/// `db` is as `want` asks, and opens.
fn settled(db: &Path, want: Want, sealer: &Sealer) -> bool {
    let Ok(key) = key_with(db, sealer) else {
        return false;
    };
    let memory = held()
        .iter()
        .any(|(p, h)| p == db && matches!(h, Held::Memory(_)));
    let opens = !db.exists()
        || Connection::open(db)
            .map_err(anyhow::Error::from)
            .and_then(|c| unlock(&c, key.as_deref()))
            .is_ok();
    // A key in memory is never settled: sealed when it can be, or dropped.
    opens && !memory && want.on == key.is_some()
}

type Sealed = (Arc<Key>, Vec<u8>, Seal);

/// A new key, sealed as `want` allows; None when encryption is off.
fn new_key(want: Want, sealer: &Sealer) -> Option<Result<Sealed>> {
    want.on.then(|| {
        let key = Key::random()?;
        let (credential, seal) = sealer.seal(&key, want.allow_host)?;
        Ok((Arc::new(key), credential, seal))
    })
}

/// Copy `db` (keyed with `from`) into a new file keyed with `to`'s key, or
/// plaintext, then put the new key file and the new database in its place.
/// A crash part way leaves a key that doesn't open the database, which
/// `apply` rebuilds: never a plaintext index beside a key.
fn convert(db: &Path, from: Option<&Key>, to: Option<(&Key, &Vec<u8>)>) -> Result<()> {
    let new = sibling(db, ".new");
    remove(&new)?;
    if db.exists() {
        private(&new)?;
        let export = || -> Result<()> {
            let conn = Connection::open(db)?;
            unlock(&conn, from)?;
            // The WAL's pages into the file, so none of them is left behind.
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            let raw = to.map(|(k, _)| k.raw());
            conn.execute(
                "ATTACH DATABASE ?1 AS target KEY ?2",
                params![
                    new.to_str().context("index path is not UTF-8")?,
                    raw.as_deref().map_or("", |s| s.as_str())
                ],
            )?;
            conn.query_row("SELECT sqlcipher_export('target')", [], |_| Ok(()))?;
            conn.execute_batch("DETACH DATABASE target")?;
            Ok(())
        };
        if let Err(e) = export() {
            let _ = fs::remove_file(&new);
            return Err(e.context("converting the index"));
        }
        File::open(&new)?.sync_all()?;
    }
    match to {
        Some((_, credential)) => write_key(db, credential)?,
        None => remove(&key_file(db))?,
    }
    // The old file's journal must not be read as the new one's.
    for ext in ["-wal", "-shm"] {
        remove(&sibling(db, ext))?;
    }
    if db.exists() {
        fs::rename(&new, db)?;
    }
    sync_dir(db)
}

/// Remove the database, its journal and its key.
fn wipe(db: &Path) -> Result<()> {
    for p in [
        db.to_path_buf(),
        sibling(db, "-wal"),
        sibling(db, "-shm"),
        sibling(db, ".new"),
        key_file(db),
        sibling(&key_file(db), ".new"),
    ] {
        remove(&p)?;
    }
    sync_dir(db)
}

fn remove(p: &Path) -> Result<()> {
    match fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| p.display().to_string())
        }
        _ => Ok(()),
    }
}

/// Create `p` empty and readable by this user only.
fn private(p: &Path) -> Result<File> {
    if let Some(dir) = p.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(p)
        .with_context(|| p.display().to_string())
}

use std::os::unix::fs::DirBuilderExt;

/// Write `contents` to `p` whole: to a new file, synced, renamed over.
fn write_private(p: &Path, contents: &[u8]) -> Result<()> {
    let tmp = sibling(p, ".new");
    let mut f = private(&tmp)?;
    f.write_all(contents)?;
    f.sync_all()?;
    fs::rename(&tmp, p).with_context(|| p.display().to_string())
}

fn write_key(db: &Path, credential: &[u8]) -> Result<()> {
    write_private(&key_file(db), credential)?;
    sync_dir(db)
}

fn sync_dir(db: &Path) -> Result<()> {
    if let Some(dir) = db.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Keep `index.roots` up to date while the index is encrypted; called once
/// the roots change.
pub fn save_roots(db: &Path, conn: &Connection) -> Result<()> {
    if !key_file(db).exists() && !state_of(db).encrypted {
        return Ok(());
    }
    let mut text = String::new();
    for r in index::roots(conn)? {
        text.push_str(r.to_str().context("root is not UTF-8")?);
        text.push('\n');
    }
    write_private(&roots_file(db), text.as_bytes())
}

fn read_roots(db: &Path) -> Vec<String> {
    fs::read_to_string(roots_file(db))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `<db>.lock`, flocked: shared by every long-running process with the index
/// open, exclusive while `apply` converts it.
struct Lock {
    file: Arc<File>,
    /// Shared again when done, if this process held it shared before.
    keep: bool,
}

static HOLDING: Mutex<Vec<(PathBuf, Arc<File>)>> = Mutex::new(Vec::new());

/// Hold `db` shared for this process's lifetime, so another process's
/// `apply` doesn't convert it underneath.
pub fn hold_shared(db: &Path) -> Result<()> {
    let mut all = HOLDING.lock().unwrap_or_else(|e| e.into_inner());
    if all.iter().any(|(p, _)| p == db) {
        return Ok(());
    }
    let file = Arc::new(private_lock(db)?);
    flock(&file, libc::LOCK_SH)?;
    all.push((db.to_owned(), file));
    Ok(())
}

fn private_lock(db: &Path) -> Result<File> {
    let p = sibling(db, ".lock");
    if let Some(dir) = p.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&p)
        .with_context(|| p.display().to_string())
}

fn flock(f: &File, op: libc::c_int) -> std::io::Result<()> {
    // SAFETY: a valid fd.
    if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

impl Lock {
    fn exclusive(db: &Path) -> Result<Lock> {
        let mine = HOLDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(p, _)| p == db)
            .map(|(_, f)| f.clone());
        let keep = mine.is_some();
        let file = match mine {
            Some(f) => f,
            None => Arc::new(private_lock(db)?),
        };
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Lock { file, keep }),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => bail!(
                "another vinoSearch process has the index open (a `vinosearch index` run, or \
                 the runner); try again once it's done"
            ),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = flock(
            &self.file,
            if self.keep {
                libc::LOCK_SH
            } else {
                libc::LOCK_UN
            },
        );
    }
}

/// A connection to the index that `apply` can close under its user: opened
/// again, with whatever key is current, on the next `lock`.
pub struct Live {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

static LIVE: Mutex<Vec<Weak<Live>>> = Mutex::new(Vec::new());
/// Read while a Live connection is in use, written while `apply` closes
/// them, so none is closed mid-query.
static SWAP: RwLock<()> = RwLock::new(());

pub struct LiveGuard<'a> {
    conn: MutexGuard<'a, Option<Connection>>,
    _swap: RwLockReadGuard<'static, ()>,
}

impl Deref for LiveGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("opened in lock")
    }
}

impl Live {
    pub fn open(path: &Path) -> Result<Arc<Live>> {
        let live = Arc::new(Live {
            path: path.to_owned(),
            conn: Mutex::new(Some(index::open(path)?)),
        });
        let mut all = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&live));
        Ok(live)
    }

    pub fn lock(&self) -> Result<LiveGuard<'_>> {
        let swap = SWAP.read().unwrap_or_else(|e| e.into_inner());
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if conn.is_none() {
            *conn = Some(index::open(&self.path)?);
        }
        Ok(LiveGuard { conn, _swap: swap })
    }
}

/// Close every Live connection to `db`, and keep them closed while the
/// guard lives.
fn close_all(db: &Path) -> std::sync::RwLockWriteGuard<'static, ()> {
    let w = SWAP.write().unwrap_or_else(|e| e.into_inner());
    let all: Vec<Arc<Live>> = LIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(Weak::upgrade)
        .filter(|l| l.path == db)
        .collect();
    for live in all {
        live.conn.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
    w
}

#[cfg(test)]
mod tests;
