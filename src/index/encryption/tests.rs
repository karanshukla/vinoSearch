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

    /// `apply`, retried while the stand-in tool is busy. `retry` looks at
    /// the error, so a busy tool reported in `waiting` (as one run on the
    /// host is) is named there too.
    fn apply(&self, on: bool, allow_host: bool, rekey: bool) -> State {
        retry(|| {
            let st = apply(&self.db(), Want { on, allow_host }, rekey, &self.sealer);
            if st.error.contains("Text file busy") || st.waiting.contains("Text file busy") {
                bail!("Text file busy: {} {}", st.error, st.waiting);
            }
            Ok(st)
        })
        .unwrap()
    }

    /// As a new process sees it: no key held.
    fn restart(&self) {
        hold(&self.db(), None);
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

/// The note indexed and encrypted.
fn encrypted(name: &str) -> Setup {
    let s = Setup::new(name, TPM_HEAD);
    s.index();
    let st = s.apply(true, false, false);
    assert_eq!(st.error, "");
    s
}

/// The note indexed and encrypted, its key in `mock`'s keyring.
fn encrypted_in_keyring(name: &str, mock: &Mock) -> Setup {
    let s = Setup::keyring(name, mock);
    s.index();
    let st = s.apply(true, false, false);
    assert_eq!((st.error.as_str(), st.seal), ("", "keyring"));
    s
}

fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// A sealer whose `systemd-creds` no longer unseals anything, as after the
/// TPM was cleared.
fn cleared_tpm(fake: &Fake) -> Sealer {
    Sealer::at(
        &fake.tool("echo 'Failed to unseal secret using TPM2: State not recoverable' >&2; exit 1"),
    )
}

/// The flock another process holding `db` open has: on an open file
/// description of its own.
fn held_by_another_process(db: &Path) -> File {
    let other = private_lock(db).unwrap();
    flock(&other, libc::LOCK_SH).unwrap();
    other
}

/// A sealer whose flatpak-spawn can't reach the host, failing with `body`.
fn host_out_of_reach(s: &Setup, body: &str) -> Sealer {
    let creds = s.fake.tool(&creds_body(TPM_HEAD));
    Sealer::on_host(&spawner(&s.fake, body), &creds, Duration::from_secs(10))
}

const NO_SESSION_HELPER: &str =
    "echo 'Portal call failed: org.freedesktop.DBus.Error.ServiceUnknown' >&2; exit 1";

const ON: Want = Want {
    on: true,
    allow_host: false,
};

const OFF: Want = Want {
    on: false,
    allow_host: false,
};

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn turning_it_on_leaves_no_text_name_or_header_on_disk() {
    let s = Setup::new("ondisk", TPM_HEAD);
    s.index();
    assert!(contains(&s.on_disk(), "QUOKKA"));
    let st = s.apply(true, false, false);
    assert_eq!(st.error, "");
    assert!(st.encrypted);
    assert_eq!(st.seal, "tpm");
    assert!(key_file(&s.db()).exists());
    let disk = s.on_disk();
    assert!(!contains(&disk, "QUOKKA"));
    assert!(!contains(&disk, "ledger.md"));
    assert!(!contains(&disk, "SQLite format 3"));
    assert!(!s.plain_sqlite_reads());
    assert!(!sibling(&s.db(), ".new").exists());
}

#[test]
fn turning_it_on_loses_nothing() {
    let s = Setup::new("lossless", TPM_HEAD);
    s.index();
    let before = index::counts(&index::open(&s.db()).unwrap()).unwrap();
    assert_eq!(s.apply(true, false, false).error, "");
    assert_eq!(
        index::counts(&index::open(&s.db()).unwrap()).unwrap(),
        before
    );
    assert!(s.finds_the_note());
}

#[test]
fn turning_it_on_keeps_the_folder_list_beside_it() {
    let s = encrypted("roots");
    let roots = fs::read_to_string(roots_file(&s.db())).unwrap();
    assert_eq!(roots.trim(), s.dir.join("root").to_str().unwrap());
}

#[test]
fn the_key_the_folder_list_and_the_index_are_the_users_alone() {
    let s = encrypted("modes");
    assert_eq!(mode(&key_file(&s.db())), 0o600);
    assert_eq!(mode(&roots_file(&s.db())), 0o600);
    assert_eq!(mode(&s.db()), 0o600);
}

#[test]
fn applying_the_same_again_changes_nothing() {
    let s = encrypted("same");
    let credential = fs::read(key_file(&s.db())).unwrap();
    assert_eq!(s.apply(true, false, false).error, "");
    assert_eq!(fs::read(key_file(&s.db())).unwrap(), credential);
}

#[test]
fn turning_it_off_decrypts_it() {
    let s = encrypted("off");
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
    let s = encrypted("rekey");
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
fn without_a_tpm_it_is_refused_and_left_plaintext() {
    let s = Setup::new("notpm", HOST_HEAD);
    s.index();
    let st = s.apply(true, false, false);
    assert!(st.error.contains("no TPM"), "{}", st.error);
    assert!(!st.encrypted);
    assert!(!key_file(&s.db()).exists());
    assert!(
        s.plain_sqlite_reads(),
        "not encrypted behind the user's back with something weaker"
    );
}

#[test]
fn without_a_tpm_the_host_key_seals_once_allowed() {
    let s = Setup::new("notpmhost", HOST_HEAD);
    s.index();
    let st = s.apply(true, true, false);
    assert_eq!(st.error, "");
    assert_eq!(st.seal, "host");
    assert!(!s.plain_sqlite_reads());
    assert!(s.finds_the_note());
}

#[test]
fn a_key_that_wont_unseal_rebuilds_the_index_encrypted() {
    let s = encrypted("broken");
    s.restart();
    let broken = cleared_tpm(&s.fake);
    let st = retry(|| {
        let st = apply(&s.db(), ON, false, &broken);
        if st.broken.contains("Text file busy") {
            bail!("busy");
        }
        Ok(st)
    })
    .unwrap();
    assert!(st.broken.contains("State not recoverable"), "{st:?}");
    assert_eq!(
        st.seal, "memory",
        "the new index's key won't seal either, so it is held in memory"
    );
    assert!(!s.plain_sqlite_reads());
    let conn = index::open(&s.db()).unwrap();
    assert_eq!(index::counts(&conn).unwrap(), (0, 0));
    assert_eq!(
        index::roots(&conn).unwrap(),
        [s.dir.join("root")],
        "the folders are kept, for the watcher to index again"
    );
}

#[test]
fn a_corrupt_key_file_rebuilds_with_a_new_sealed_key() {
    let s = encrypted("corrupt");
    s.restart();
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
    let s = encrypted("nokey");
    s.restart();
    fs::remove_file(key_file(&s.db())).unwrap();
    let e = index::open(&s.db()).unwrap_err();
    assert!(is_unusable(&e), "{e:#}");
    let st = s.apply(false, false, false);
    assert!(st.broken.contains("key file is gone"), "{st:?}");
    assert!(
        s.plain_sqlite_reads(),
        "rebuilt by apply, plaintext with encryption off"
    );
}

#[test]
fn live_connections_follow_the_conversion() {
    let s = Setup::new("live", TPM_HEAD);
    s.index();
    let live = Live::open(&s.db()).unwrap();
    assert_eq!(index::counts(&live.lock().unwrap()).unwrap().0, 1);
    s.apply(true, false, false);
    assert!(live.conn.lock().unwrap().is_none(), "closed by apply");
    assert_eq!(
        index::counts(&live.lock().unwrap()).unwrap().0,
        1,
        "opened again with the key"
    );
    s.apply(false, false, false);
    assert_eq!(index::counts(&live.lock().unwrap()).unwrap().0, 1);
}

#[test]
fn another_process_holding_the_index_blocks_a_conversion() {
    let s = Setup::new("lock", TPM_HEAD);
    s.index();
    let other = held_by_another_process(&s.db());
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
fn without_systemd_creds_the_key_is_in_the_keyring_alone() {
    let Some(mock) = Mock::start("enc-keyring") else {
        eprintln!("no dbus-daemon: skipped");
        return;
    };
    let s = encrypted_in_keyring("keyring", &mock);
    let file = fs::read(key_file(&s.db())).unwrap();
    assert!(file.starts_with(b"secret-service 1 "));
    assert_eq!(mock.state().items.len(), 1);
    assert!(!s.plain_sqlite_reads());
    assert!(s.finds_the_note());
}

#[test]
fn a_new_process_unseals_a_keyring_key_the_same_way() {
    let Some(mock) = Mock::start("enc-keyring-again") else {
        return;
    };
    let s = encrypted_in_keyring("keyringagain", &mock);
    s.restart();
    assert!(s.apply(true, false, false).encrypted);
    assert!(s.finds_the_note());
}

#[test]
fn a_new_keyring_key_replaces_the_item_and_turning_it_off_removes_it() {
    let Some(mock) = Mock::start("enc-keyring-rekey") else {
        return;
    };
    let s = encrypted_in_keyring("keyringrekey", &mock);
    let file = fs::read(key_file(&s.db())).unwrap();
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
    let s = encrypted_in_keyring("locked", &mock);
    let before = s.on_disk();
    s.restart();
    mock.state().locked = true;
    let st = apply(&s.db(), ON, false, &s.sealer.quiet());
    assert!(st.waiting.contains("locked"), "{st:?}");
    assert_eq!((st.error.as_str(), st.broken.as_str()), ("", ""));
    assert!(st.encrypted);
    assert_eq!(st.seal, "keyring");
    assert_eq!(s.on_disk(), before, "nothing dropped, nothing plaintext");
    let e = index::open(&s.db()).unwrap_err();
    assert!(seal::is_unavailable(&e) && !is_unusable(&e), "{e:#}");
    assert_eq!(
        mock.state().prompts,
        0,
        "every open fails at once, without asking the keyring again"
    );
    mock.state().answer = Answer::Dismiss;
    let st = s.apply(true, false, false);
    assert!(
        st.waiting.contains("dismissed"),
        "dismissed at the prompt: still waiting, {st:?}"
    );
    mock.state().answer = Answer::Unlock;
    let st = s.apply(true, false, false);
    assert_eq!(
        (st.waiting.as_str(), st.broken.as_str()),
        ("", ""),
        "unlocked: opens as it was"
    );
    assert!(s.finds_the_note());
}

#[test]
fn a_keyring_item_gone_is_waited_for_until_encryption_starts_over() {
    let Some(mock) = Mock::start("enc-gone") else {
        return;
    };
    let s = encrypted_in_keyring("gone", &mock);
    let before = s.on_disk();
    s.restart();
    mock.state().items.clear();
    let st = s.apply(true, false, false);
    assert!(st.waiting.contains("no item"), "{st:?}");
    assert!(st.waiting.contains("another keyring"), "{st:?}");
    assert_eq!((st.broken.as_str(), st.seal), ("", "keyring"));
    assert_eq!(s.on_disk(), before, "kept as it was");
    assert!(
        mock.state().items.is_empty(),
        "no new key made behind the user's back"
    );
    assert!(seal::is_unavailable(&index::open(&s.db()).unwrap_err()));

    for (when, sealer) in [
        ("a quiet try", s.sealer.quiet()),
        ("the start or `vinosearch index`", s.sealer.clone()),
    ] {
        let st = apply(&s.db(), OFF, false, &sealer);
        assert!(
            !st.waiting.is_empty() && st.broken.is_empty(),
            "turned off at {when}: still waiting, {st:?}"
        );
        assert_eq!(s.on_disk(), before, "turned off at {when}: kept");
    }

    let st = apply(&s.db(), OFF, false, &s.sealer.asked());
    assert!(
        st.broken.contains("no item"),
        "turned off by the user: started over, {st:?}"
    );
    assert_eq!((st.waiting.as_str(), st.encrypted), ("", false));
    assert!(s.plain_sqlite_reads());
    let conn = index::open(&s.db()).unwrap();
    assert_eq!(index::counts(&conn).unwrap(), (0, 0));
    assert_eq!(index::roots(&conn).unwrap(), [s.dir.join("root")]);
    drop(conn);

    let st = s.apply(true, false, false);
    assert_eq!(
        (st.error.as_str(), st.seal),
        ("", "keyring"),
        "on again: a new key in this keyring"
    );
    assert_eq!(mock.state().items.len(), 1);
    s.index();
    assert!(s.finds_the_note());
}

#[test]
fn from_the_flatpak_a_host_out_of_reach_is_waited_for_and_the_index_kept() {
    let mut s = encrypted("hostgone");
    let before = s.on_disk();
    let file = fs::read(key_file(&s.db())).unwrap();
    s.restart();
    let creds = s.fake.tool(&creds_body(TPM_HEAD));
    s.sealer = host_out_of_reach(&s, NO_SESSION_HELPER);
    let st = s.apply(true, false, false);
    assert!(st.waiting.contains("Portal call failed"), "{st:?}");
    assert_eq!((st.error.as_str(), st.broken.as_str()), ("", ""));
    assert_eq!(st.seal, "tpm");
    assert_eq!(s.on_disk(), before);
    assert_eq!(fs::read(key_file(&s.db())).unwrap(), file);
    let e = index::open(&s.db()).unwrap_err();
    assert!(seal::is_unavailable(&e) && !is_unusable(&e), "{e:#}");
    s.sealer = Sealer::on_host(&spawner(&s.fake, HOST), &creds, Duration::from_secs(10));
    let st = s.apply(true, false, false);
    assert_eq!(
        (st.waiting.as_str(), st.broken.as_str()),
        ("", ""),
        "the host back: opens as it was"
    );
    assert!(s.finds_the_note());
}

#[test]
fn from_the_flatpak_a_host_out_of_reach_leaves_a_plaintext_index_plaintext() {
    let mut s = Setup::new("hostoff", TPM_HEAD);
    s.index();
    s.sealer = host_out_of_reach(&s, "echo \"Can't find bus\" >&2; exit 1");
    let st = s.apply(true, true, false);
    assert!(
        st.error.contains("Can't find bus"),
        "refused, not sealed in the keyring or with the host key: {st:?}"
    );
    assert!(!st.encrypted);
    assert!(!key_file(&s.db()).exists());
    assert!(s.plain_sqlite_reads());
}

#[test]
fn a_tpm_sealed_index_is_never_moved_to_the_keyring() {
    let Some(mock) = Mock::start("enc-stay") else {
        return;
    };
    let mut s = encrypted("stay");
    let file = fs::read(key_file(&s.db())).unwrap();
    s.restart();
    s.sealer = Sealer::keyring_only(mock.keyring(Some(Duration::from_secs(5))));
    let st = s.apply(true, false, false);
    assert!(
        st.waiting.contains("systemd-creds"),
        "systemd-creds gone: waited for, {st:?}"
    );
    assert_eq!((st.broken.as_str(), st.seal), ("", "tpm"));
    assert_eq!(fs::read(key_file(&s.db())).unwrap(), file);
    assert!(
        mock.state().items.is_empty(),
        "not sealed again in the keyring"
    );
}
