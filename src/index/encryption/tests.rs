//! Turning encryption on and off over an index built through `FakeEmbed`,
//! with a stand-in `systemd-creds`: what's on disk, what still searches, and
//! what a lost key does.

use super::*;
use crate::config::{Excludes, Formats};
use crate::index::seal::keyring::mock::{Answer, Mock};
use crate::index::seal::tests::{Fake, HOST, HOST_HEAD, TPM_HEAD, creds_body, retry, spawner};
use crate::index::{FileTypes, Kind, Vectors, Words};
use crate::ml::embed::FakeEmbed;
use std::time::Duration;

const SECRET: &str = "the ledger code is QUOKKA-7731 for the harbour invoice";

/// A tree with one note in it, its index under the same directory.
struct Setup {
    dir: PathBuf,
    fake: Fake,
    sealer: Sealer,
}

impl Setup {
    fn new(name: &str, head: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "vinosearch-encryption-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("root")).unwrap();
        let dir = fs::canonicalize(&dir).unwrap();
        fs::write(dir.join("root/ledger.md"), format!("{SECRET}\n")).unwrap();
        let fake = Fake::new(name);
        let sealer = Sealer::at(&fake.tool(&creds_body(head)));
        Setup { dir, fake, sealer }
    }

    /// The same, with no `systemd-creds`: the key in `mock`'s keyring.
    fn keyring(name: &str, mock: &Mock) -> Self {
        let mut s = Setup::new(name, TPM_HEAD);
        s.sealer = Sealer::keyring_only(mock.keyring(Some(Duration::from_secs(5))));
        s
    }

    fn db(&self) -> PathBuf {
        self.dir.join("index.sqlite")
    }

    fn index(&self) {
        let mut conn = index::open(&self.db()).unwrap();
        let mut types = Formats::default();
        types.documents.enabled = Some(false);
        index::index(
            &mut conn,
            &mut FakeEmbed::default(),
            &self.dir.join("root"),
            &Excludes::default(),
            &FileTypes::new(&types).unwrap(),
            &mut |_, _, _| Ok(()),
        )
        .unwrap();
    }

    fn apply(&self, on: bool, allow_host: bool, rekey: bool) -> State {
        retry(|| {
            let st = apply(&self.db(), Want { on, allow_host }, rekey, &self.sealer);
            // Named in the error, which is what `retry` looks at: a busy
            // tool run on the host is reported as waiting, with no error.
            if st.error.contains("Text file busy") || st.waiting.contains("Text file busy") {
                bail!("Text file busy: {} {}", st.error, st.waiting);
            }
            Ok(st)
        })
        .unwrap()
    }

    /// Every byte of the database and its journal.
    fn on_disk(&self) -> Vec<u8> {
        let mut all = Vec::new();
        for ext in ["", "-wal", "-shm"] {
            all.extend(fs::read(sibling(&self.db(), ext)).unwrap_or_default());
        }
        all
    }

    fn plain_sqlite_reads(&self) -> bool {
        let conn = Connection::open(self.db()).unwrap();
        conn.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .is_ok()
    }

    /// The note, found by vector and by its exact code.
    fn finds_the_note(&self) -> bool {
        let conn = index::open(&self.db()).unwrap();
        let v = Vectors::load(&conn).unwrap();
        let words = Words::find(&conn, "QUOKKA-7731");
        let hits = index::search(
            &conn,
            &v,
            &FakeEmbed::vector("harbour invoice ledger"),
            words.as_ref(),
            Kind::Text,
            5,
        )
        .unwrap();
        words.is_some() && hits.first().is_some_and(|h| h.path.ends_with("ledger.md"))
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        hold(&self.db(), None);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn turning_it_on_encrypts_what_is_there_and_off_decrypts_it() {
    let s = Setup::new("onoff", TPM_HEAD);
    s.index();
    assert!(contains(&s.on_disk(), "QUOKKA"));
    let before = index::counts(&index::open(&s.db()).unwrap()).unwrap();

    let st = s.apply(true, false, false);
    assert_eq!(st.error, "");
    assert!(st.encrypted);
    assert_eq!(st.seal, "tpm");
    assert!(key_file(&s.db()).exists());
    // Not the text, not a file name, not SQLite's header, and plain SQLite
    // can't read it.
    let disk = s.on_disk();
    assert!(!contains(&disk, "QUOKKA"));
    assert!(!contains(&disk, "ledger.md"));
    assert!(!contains(&disk, "SQLite format 3"));
    assert!(!s.plain_sqlite_reads());
    assert!(!sibling(&s.db(), ".new").exists());
    // Nothing lost: the same counts, and both searches still find it.
    assert_eq!(
        index::counts(&index::open(&s.db()).unwrap()).unwrap(),
        before
    );
    assert!(s.finds_the_note());
    // The folder list is kept beside it for a rebuild.
    let roots = fs::read_to_string(roots_file(&s.db())).unwrap();
    assert_eq!(roots.trim(), s.dir.join("root").to_str().unwrap());
    let mode = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    };
    assert_eq!(mode(&key_file(&s.db())), 0o600);
    assert_eq!(mode(&roots_file(&s.db())), 0o600);
    assert_eq!(mode(&s.db()), 0o600);

    // Applying the same again changes nothing.
    let credential = fs::read(key_file(&s.db())).unwrap();
    assert_eq!(s.apply(true, false, false).error, "");
    assert_eq!(fs::read(key_file(&s.db())).unwrap(), credential);

    let st = s.apply(false, false, false);
    assert_eq!(st, State::default());
    assert!(!key_file(&s.db()).exists());
    assert!(!roots_file(&s.db()).exists());
    assert!(s.plain_sqlite_reads());
    assert!(s.finds_the_note());
}

#[test]
fn a_new_index_is_encrypted_from_the_start() {
    let s = Setup::new("fresh", TPM_HEAD);
    assert!(s.apply(true, false, false).encrypted);
    s.index();
    assert!(!contains(&s.on_disk(), "QUOKKA"));
    assert!(!s.plain_sqlite_reads());
    assert!(s.finds_the_note());
}

#[test]
fn rekeying_replaces_the_key() {
    let s = Setup::new("rekey", TPM_HEAD);
    s.index();
    s.apply(true, false, false);
    let old = fs::read(key_file(&s.db())).unwrap();
    let old_key = s.sealer.unseal(&old).unwrap();
    let st = s.apply(true, false, true);
    assert_eq!(st.error, "");
    assert_ne!(fs::read(key_file(&s.db())).unwrap(), old);
    let conn = Connection::open(s.db()).unwrap();
    assert!(is_unusable(&unlock(&conn, Some(&old_key)).unwrap_err()));
    assert!(s.finds_the_note());
}

#[test]
fn without_a_tpm_it_is_refused_unless_the_host_key_is_allowed() {
    let s = Setup::new("notpm", HOST_HEAD);
    s.index();
    let st = s.apply(true, false, false);
    assert!(st.error.contains("no TPM"), "{}", st.error);
    assert!(!st.encrypted);
    assert!(!key_file(&s.db()).exists());
    // Left as it was, and said so: not encrypted behind the user's back
    // with something weaker.
    assert!(s.plain_sqlite_reads());

    let st = s.apply(true, true, false);
    assert_eq!(st.error, "");
    assert_eq!(st.seal, "host");
    assert!(!s.plain_sqlite_reads());
    assert!(s.finds_the_note());
}

#[test]
fn a_key_that_wont_unseal_rebuilds_the_index_encrypted() {
    let s = Setup::new("broken", TPM_HEAD);
    s.index();
    s.apply(true, false, false);
    // The TPM was cleared: this credential no longer unseals.
    hold(&s.db(), None);
    let broken = Sealer::at(
        &s.fake
            .tool("echo 'Failed to unseal secret using TPM2: State not recoverable' >&2; exit 1"),
    );
    let st = retry(|| {
        let st = apply(
            &s.db(),
            Want {
                on: true,
                allow_host: false,
            },
            false,
            &broken,
        );
        if st.broken.contains("Text file busy") {
            bail!("busy");
        }
        Ok(st)
    })
    .unwrap();
    assert!(st.broken.contains("State not recoverable"), "{st:?}");
    // Encrypting the new index failed too (the same stand-in), so its key
    // is in memory: still not plaintext.
    assert_eq!(st.seal, "memory");
    assert!(!s.plain_sqlite_reads());
    let conn = index::open(&s.db()).unwrap();
    assert_eq!(index::counts(&conn).unwrap(), (0, 0));
    // The folders are kept, for the watcher to index again.
    assert_eq!(index::roots(&conn).unwrap(), [s.dir.join("root")]);
}

#[test]
fn a_corrupt_key_file_rebuilds_with_a_new_sealed_key() {
    let s = Setup::new("corrupt", TPM_HEAD);
    s.index();
    s.apply(true, false, false);
    hold(&s.db(), None);
    fs::write(key_file(&s.db()), "AAAA garbage").unwrap();
    let st = s.apply(true, false, false);
    assert!(!st.broken.is_empty());
    assert_eq!(st.seal, "tpm");
    assert!(!s.plain_sqlite_reads());
    s.index();
    assert!(s.finds_the_note());
}

#[test]
fn an_encrypted_index_without_its_key_is_unusable_not_empty() {
    let s = Setup::new("nokey", TPM_HEAD);
    s.index();
    s.apply(true, false, false);
    hold(&s.db(), None);
    fs::remove_file(key_file(&s.db())).unwrap();
    let e = index::open(&s.db()).unwrap_err();
    assert!(is_unusable(&e), "{e:#}");
    // apply rebuilds it, and with encryption off now that is plaintext.
    let st = s.apply(false, false, false);
    assert!(st.broken.contains("key file is gone"), "{st:?}");
    assert!(s.plain_sqlite_reads());
}

#[test]
fn live_connections_follow_the_conversion() {
    let s = Setup::new("live", TPM_HEAD);
    s.index();
    let live = Live::open(&s.db()).unwrap();
    assert_eq!(index::counts(&live.lock().unwrap()).unwrap().0, 1);
    s.apply(true, false, false);
    // Closed by apply, opened again with the key.
    assert!(live.conn.lock().unwrap().is_none());
    assert_eq!(index::counts(&live.lock().unwrap()).unwrap().0, 1);
    s.apply(false, false, false);
    assert_eq!(index::counts(&live.lock().unwrap()).unwrap().0, 1);
}

#[test]
fn another_process_holding_the_index_blocks_a_conversion() {
    let s = Setup::new("lock", TPM_HEAD);
    s.index();
    // Another open file description, as another process would have.
    let other = private_lock(&s.db()).unwrap();
    flock(&other, libc::LOCK_SH).unwrap();
    let st = s.apply(true, false, false);
    assert!(st.error.contains("another vinoSearch process"), "{st:?}");
    assert!(s.plain_sqlite_reads());
    drop(other);
    assert!(s.apply(true, false, false).encrypted);
}

#[test]
fn a_memory_key_opens_a_throwaway_index() {
    let s = Setup::new("memory", TPM_HEAD);
    hold_in_memory(&s.db()).unwrap();
    s.index();
    assert!(!s.plain_sqlite_reads());
    assert!(s.finds_the_note());
    assert!(!key_file(&s.db()).exists());
}

#[test]
fn without_systemd_creds_the_key_is_in_the_keyring() {
    let Some(mock) = Mock::start("enc-keyring") else {
        eprintln!("no dbus-daemon: skipped");
        return;
    };
    let s = Setup::keyring("keyring", &mock);
    s.index();
    let st = s.apply(true, false, false);
    assert_eq!(st.error, "");
    assert_eq!(st.seal, "keyring");
    // The key file names the item; the key is in the keyring alone.
    let file = fs::read(key_file(&s.db())).unwrap();
    assert!(file.starts_with(b"secret-service 1 "));
    assert_eq!(mock.state().items.len(), 1);
    assert!(!s.plain_sqlite_reads());
    assert!(s.finds_the_note());
    // A new process unseals it the same way.
    hold(&s.db(), None);
    assert!(s.apply(true, false, false).encrypted);
    assert!(s.finds_the_note());
    // A new key replaces the item, and turning it off removes it.
    let st = s.apply(true, false, true);
    assert_eq!(st.error, "");
    assert_ne!(fs::read(key_file(&s.db())).unwrap(), file);
    assert_eq!(mock.state().items.len(), 1);
    assert!(s.finds_the_note());
    assert_eq!(s.apply(false, false, false), State::default());
    assert!(mock.state().items.is_empty());
    assert!(s.plain_sqlite_reads());
}

#[test]
fn a_locked_keyring_is_waited_for_and_the_index_kept() {
    let Some(mock) = Mock::start("enc-locked") else {
        return;
    };
    let s = Setup::keyring("locked", &mock);
    s.index();
    s.apply(true, false, false);
    let before = s.on_disk();
    // The next start: locked, and no prompt allowed.
    hold(&s.db(), None);
    mock.state().locked = true;
    let want = Want {
        on: true,
        allow_host: false,
    };
    let st = apply(&s.db(), want, false, &s.sealer.quiet());
    assert!(st.waiting.contains("locked"), "{st:?}");
    assert_eq!((st.error.as_str(), st.broken.as_str()), ("", ""));
    assert!(st.encrypted);
    assert_eq!(st.seal, "keyring");
    // Nothing dropped, nothing plaintext, and every open fails at once
    // without asking the keyring again.
    assert_eq!(s.on_disk(), before);
    let e = index::open(&s.db()).unwrap_err();
    assert!(seal::is_unavailable(&e) && !is_unusable(&e), "{e:#}");
    assert_eq!(mock.state().prompts, 0);
    // Dismissed at the prompt: still waiting.
    mock.state().answer = Answer::Dismiss;
    let st = s.apply(true, false, false);
    assert!(st.waiting.contains("dismissed"), "{st:?}");
    // Unlocked: opens as it was.
    mock.state().answer = Answer::Unlock;
    let st = s.apply(true, false, false);
    assert_eq!((st.waiting.as_str(), st.broken.as_str()), ("", ""));
    assert!(s.finds_the_note());
}

#[test]
fn a_keyring_item_gone_is_waited_for_until_encryption_starts_over() {
    let Some(mock) = Mock::start("enc-gone") else {
        return;
    };
    let s = Setup::keyring("gone", &mock);
    s.index();
    s.apply(true, false, false);
    let before = s.on_disk();
    // Another keyring serves the session now, or the item was deleted.
    hold(&s.db(), None);
    mock.state().items.clear();
    let st = s.apply(true, false, false);
    assert!(st.waiting.contains("no item"), "{st:?}");
    assert!(st.waiting.contains("another keyring"), "{st:?}");
    assert_eq!((st.broken.as_str(), st.seal), ("", "keyring"));
    // Kept as it was, and no new key made behind the user's back.
    assert_eq!(s.on_disk(), before);
    assert!(mock.state().items.is_empty());
    assert!(seal::is_unavailable(&index::open(&s.db()).unwrap_err()));
    // Not at a quiet try either, with encryption off.
    let off = Want {
        on: false,
        allow_host: false,
    };
    let st = apply(&s.db(), off, false, &s.sealer.quiet());
    assert!(!st.waiting.is_empty() && st.broken.is_empty(), "{st:?}");
    assert_eq!(s.on_disk(), before);
    // Nor at a try that may prompt but that nobody asked for: the start,
    // `vinosearch index`.
    let st = apply(&s.db(), off, false, &s.sealer);
    assert!(!st.waiting.is_empty() && st.broken.is_empty(), "{st:?}");
    assert_eq!(s.on_disk(), before);
    // Turned off (the Security page's switch): started over, plaintext,
    // without the old key, the folders kept.
    let st = apply(&s.db(), off, false, &s.sealer.asked());
    assert!(st.broken.contains("no item"), "{st:?}");
    assert_eq!((st.waiting.as_str(), st.encrypted), ("", false));
    assert!(s.plain_sqlite_reads());
    let conn = index::open(&s.db()).unwrap();
    assert_eq!(index::counts(&conn).unwrap(), (0, 0));
    assert_eq!(index::roots(&conn).unwrap(), [s.dir.join("root")]);
    drop(conn);
    // And on again: a new key in this keyring.
    let st = s.apply(true, false, false);
    assert_eq!((st.error.as_str(), st.seal), ("", "keyring"));
    assert_eq!(mock.state().items.len(), 1);
    s.index();
    assert!(s.finds_the_note());
}

#[test]
fn from_the_flatpak_a_host_out_of_reach_is_waited_for_and_the_index_kept() {
    let mut s = Setup::new("hostgone", TPM_HEAD);
    s.index();
    s.apply(true, false, false);
    let before = s.on_disk();
    let file = fs::read(key_file(&s.db())).unwrap();
    // The next start, in the Flatpak: flatpak-spawn can't reach the host.
    hold(&s.db(), None);
    let creds = s.fake.tool(&creds_body(TPM_HEAD));
    let gone = spawner(
        &s.fake,
        "echo 'Portal call failed: org.freedesktop.DBus.Error.ServiceUnknown' >&2; exit 1",
    );
    s.sealer = Sealer::on_host(&gone, &creds, Duration::from_secs(10));
    let st = s.apply(true, false, false);
    assert!(st.waiting.contains("Portal call failed"), "{st:?}");
    assert_eq!((st.error.as_str(), st.broken.as_str()), ("", ""));
    assert_eq!(st.seal, "tpm");
    assert_eq!(s.on_disk(), before);
    assert_eq!(fs::read(key_file(&s.db())).unwrap(), file);
    let e = index::open(&s.db()).unwrap_err();
    assert!(seal::is_unavailable(&e) && !is_unusable(&e), "{e:#}");
    // Back: opens as it was.
    s.sealer = Sealer::on_host(&spawner(&s.fake, HOST), &creds, Duration::from_secs(10));
    let st = s.apply(true, false, false);
    assert_eq!((st.waiting.as_str(), st.broken.as_str()), ("", ""));
    assert!(s.finds_the_note());
}

#[test]
fn from_the_flatpak_a_host_out_of_reach_leaves_a_plaintext_index_plaintext() {
    let mut s = Setup::new("hostoff", TPM_HEAD);
    s.index();
    let creds = s.fake.tool(&creds_body(TPM_HEAD));
    let gone = spawner(&s.fake, "echo \"Can't find bus\" >&2; exit 1");
    s.sealer = Sealer::on_host(&gone, &creds, Duration::from_secs(10));
    // Not in the keyring instead, nor with the host key: refused, and said so.
    let st = s.apply(true, true, false);
    assert!(st.error.contains("Can't find bus"), "{st:?}");
    assert!(!st.encrypted);
    assert!(!key_file(&s.db()).exists());
    assert!(s.plain_sqlite_reads());
}

#[test]
fn a_tpm_sealed_index_is_never_moved_to_the_keyring() {
    let Some(mock) = Mock::start("enc-stay") else {
        return;
    };
    let mut s = Setup::new("stay", TPM_HEAD);
    s.index();
    s.apply(true, false, false);
    let file = fs::read(key_file(&s.db())).unwrap();
    // systemd-creds gone (or not found): waited for, not dropped, and not
    // sealed again in the keyring.
    hold(&s.db(), None);
    s.sealer = Sealer::keyring_only(mock.keyring(Some(Duration::from_secs(5))));
    let st = s.apply(true, false, false);
    assert!(st.waiting.contains("systemd-creds"), "{st:?}");
    assert_eq!((st.broken.as_str(), st.seal), ("", "tpm"));
    assert_eq!(fs::read(key_file(&s.db())).unwrap(), file);
    assert!(mock.state().items.is_empty());
}
