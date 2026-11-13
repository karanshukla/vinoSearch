//! The index key in the desktop keyring, through the freedesktop Secret
//! Service (gnome-keyring, KWallet's ksecretd, oo7-daemon), for where
//! `systemd-creds` can't seal it.
//!
//! The keyring is unlocked at login and then readable by anything running as
//! this user in the session; it is not bound to a TPM. On disk it is
//! encrypted with the keyring's password, so a backup or a disk taken out
//! needs that password.
//!
//! `index.key` names the item (`HEAD` and a random id) and holds no key. The
//! `secret-service` crate opens the session as
//! `dh-ietf1024-sha256-aes128-cbc-pkcs7` (`EncryptionType::Dh`), so the key
//! crosses the bus encrypted. Unlocking is done here rather than by the
//! crate, so its prompt is bounded and dismissed when nobody answers. A
//! sandboxed build would go through `org.freedesktop.portal.Secret` instead,
//! under an `index.key` head of its own.

use super::{Unavailable, random};
use anyhow::{Context, Result, anyhow, bail};
use futures_lite::StreamExt;
use secret_service::{EncryptionType, Item, SecretService};
use std::collections::HashMap;
use std::future::Future;
use std::sync::mpsc;
use std::time::Duration;
use zbus::{OwnedObjectPath, OwnedVariant};
use zeroize::Zeroizing;

/// What `index.key` starts with for a key in the keyring. Not base64 (the
/// space), so never taken for a `systemd-creds` credential.
pub const HEAD: &str = "secret-service 1 ";
/// The item's `xdg:schema`, beside an `id` naming this key.
const SCHEMA: &str = "dev.vinosearch.IndexKey";
const LABEL: &str = "vinoSearch index key";

const BUS: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
/// A call the Secret Service doesn't answer in this long (activating
/// included) is taken as no Secret Service.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const NONE: &str =
    "no Secret Service (a keyring such as gnome-keyring or KWallet) on the session bus";

/// The Secret Service on the session bus (or `address`, for tests).
#[derive(Clone, Debug, Default)]
pub struct Keyring {
    pub address: Option<String>,
    /// How long an unlock prompt may wait for the user; None never shows one,
    /// so a locked keyring is `Unavailable` at once.
    pub prompt: Option<Duration>,
}

impl Keyring {
    /// Store `secret` as a new item in the default collection; what
    /// `index.key` holds for it.
    pub fn store(&self, secret: &[u8]) -> Result<Vec<u8>> {
        let mut id = [0u8; 16];
        random(&mut id)?;
        let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
        self.block(async |c: &Client| {
            let collection = c.ss.get_default_collection().await.map_err(failed)?;
            if collection.is_locked().await.map_err(failed)? {
                c.unlock(&collection.collection_path).await?;
            }
            let attributes = HashMap::from([("xdg:schema", SCHEMA), ("id", id.as_str())]);
            let create = collection.create_item(
                LABEL,
                attributes,
                secret,
                false,
                "application/octet-stream",
            );
            // A prompt here (only for a locked collection, unlocked above) is
            // the crate's: bounded, but it can't be dismissed from here.
            let wait = c.prompt.unwrap_or(CALL_TIMEOUT);
            match within(wait, create).await {
                Some(r) => r.map(drop).map_err(failed),
                None => bail!(Unavailable(format!(
                    "the keyring didn't store the key within {} s",
                    wait.as_secs()
                ))),
            }
        })?;
        Ok(format!("{HEAD}{id}\n").into_bytes())
    }

    /// The secret the item `blob` names. Every failure is `Unavailable`, an
    /// item that isn't there too: unlike a cleared TPM, that may be another
    /// keyring serving this session (another desktop), or one whose
    /// collection isn't open yet.
    pub fn fetch(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let id = id_of(blob).context("not a keyring key file")?;
        self.block(async |c: &Client| {
            let item = c.find(id).await?.ok_or_else(|| {
                Unavailable(
                    "this keyring has no item for the index's key; it may be in another \
                     keyring, or deleted"
                        .into(),
                )
            })?;
            // Decrypted by the crate into a Vec of its own, wiped from here on.
            Ok(Zeroizing::new(item.get_secret().await.map_err(failed)?))
        })
    }

    /// Remove the item `blob` names, if it's there and unlocked; never prompts.
    pub fn delete(&self, blob: &[u8]) -> Result<()> {
        let id = id_of(blob).context("not a keyring key file")?;
        let quiet = Keyring {
            prompt: None,
            ..self.clone()
        };
        quiet.block(async |c: &Client| {
            if let Some(item) = c.find(id).await? {
                within(CALL_TIMEOUT, item.delete())
                    .await
                    .context("the keyring didn't delete the old key in time")?
                    .map_err(failed)?;
            }
            Ok(())
        })
    }

    /// Run `f` against a fresh connection and session, on a thread of its
    /// own: `zbus::block_on` panics inside a task zbus runs (a served
    /// method), and the connection ends with it.
    fn block<T: Send>(&self, f: impl AsyncFnOnce(&Client) -> Result<T> + Send) -> Result<T> {
        std::thread::scope(|s| {
            s.spawn(|| {
                zbus::block_on(async {
                    let c = Client::open(self).await?;
                    f(&c).await
                })
            })
            .join()
            .unwrap_or_else(|_| Err(anyhow!("the keyring client panicked")))
        })
    }
}

/// The item id in a keyring `index.key`.
pub fn id_of(blob: &[u8]) -> Option<&str> {
    let id = std::str::from_utf8(blob.strip_prefix(HEAD.as_bytes())?)
        .ok()?
        .trim_end();
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

struct Client {
    conn: zbus::Connection,
    ss: SecretService<'static>,
    prompt: Option<Duration>,
}

/// Errors that say the Secret Service isn't there or didn't answer: worth
/// waiting out, never a reason to drop the index.
fn transient(e: zbus::Error) -> anyhow::Error {
    use zbus::fdo::Error as Fdo;
    let why = match &e {
        zbus::Error::FDO(f) => match &**f {
            Fdo::ServiceUnknown(_)
            | Fdo::NameHasNoOwner(_)
            | Fdo::SpawnServiceNotFound(_)
            | Fdo::SpawnChildExited(_) => NONE.into(),
            Fdo::TimedOut(_) | Fdo::NoReply(_) => "the keyring didn't answer".into(),
            f => format!("the keyring refused: {f}"),
        },
        zbus::Error::MethodError(name, msg, _) => match name.as_str() {
            "org.freedesktop.Secret.Error.IsLocked" => "the keyring is locked".into(),
            _ => format!(
                "the keyring refused: {name}{}",
                msg.as_deref().map(|m| format!(": {m}")).unwrap_or_default()
            ),
        },
        zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut => {
            "the keyring didn't answer".into()
        }
        _ => format!("the keyring: {e}"),
    };
    Unavailable(why).into()
}

/// The crate's errors, all `Unavailable` as `transient`'s are.
fn failed(e: secret_service::Error) -> anyhow::Error {
    use secret_service::Error as E;
    match e {
        E::Zbus(e) => transient(e),
        E::ZbusFdo(f) => transient(zbus::Error::FDO(Box::new(f))),
        E::Locked => Unavailable("the keyring is locked".into()).into(),
        E::Prompt => Unavailable("unlocking the keyring was dismissed".into()).into(),
        E::Unavailable => Unavailable(NONE.into()).into(),
        E::NoResult => {
            Unavailable("the keyring has no default collection to keep the key in".into()).into()
        }
        E::Crypto(why) => Unavailable(format!("the keyring's secret didn't decrypt: {why}")).into(),
        e => Unavailable(format!("the keyring: {e}")).into(),
    }
}

/// `fut`, or None once `wait` has passed. The timer is a thread: zbus's own
/// is private. Dropping `cancel` ends it early.
async fn within<T>(wait: Duration, fut: impl Future<Output = T>) -> Option<T> {
    let (cancel, timer) = mpsc::channel::<()>();
    let (fire, fired) = futures_channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        if timer.recv_timeout(wait) == Err(mpsc::RecvTimeoutError::Timeout) {
            let _ = fire.send(());
        }
    });
    let out = futures_lite::future::or(async { Some(fut.await) }, async {
        let _ = fired.await;
        None
    })
    .await;
    drop(cancel);
    out
}

impl Client {
    async fn open(k: &Keyring) -> Result<Client> {
        let builder = match &k.address {
            Some(a) => zbus::connection::Builder::address(a.as_str()),
            None => zbus::connection::Builder::session(),
        };
        let conn = builder
            .method_timeout(CALL_TIMEOUT)
            .build()
            .await
            .map_err(|e| Unavailable(format!("no session bus: {e}")))?;
        let ss = SecretService::connect_with_existing(EncryptionType::Dh, conn.clone())
            .await
            .map_err(failed)?;
        Ok(Client {
            conn,
            ss,
            prompt: k.prompt,
        })
    }

    async fn call<B, R>(&self, path: &str, iface: &str, method: &str, body: &B) -> Result<R>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let reply = self
            .conn
            .call_method(Some(BUS), path, Some(iface), method, body)
            .await
            .map_err(transient)?;
        reply.body().deserialize().map_err(transient)
    }

    /// The item for `id`, unlocked (prompting if allowed); None if there is
    /// none.
    async fn find(&self, id: &str) -> Result<Option<Item<'_>>> {
        let attributes = HashMap::from([("xdg:schema", SCHEMA), ("id", id)]);
        let found = self.ss.search_items(attributes).await.map_err(failed)?;
        if let Some(item) = found.unlocked.into_iter().next() {
            return Ok(Some(item));
        }
        let Some(item) = found.locked.into_iter().next() else {
            return Ok(None);
        };
        self.unlock(&item.item_path).await?;
        Ok(Some(item))
    }

    /// Unlock `object` with its prompt bounded: the crate's own unlock waits
    /// on the prompt for as long as it takes.
    async fn unlock(&self, object: &OwnedObjectPath) -> Result<()> {
        let (unlocked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = self
            .call(SERVICE_PATH, SERVICE, "Unlock", &(vec![object],))
            .await?;
        if unlocked.is_empty() {
            self.prompt(&prompt).await?;
        }
        Ok(())
    }

    /// Show `prompt` and wait for the user, at most `self.prompt`; dismissed
    /// when that runs out.
    async fn prompt(&self, prompt: &OwnedObjectPath) -> Result<()> {
        if prompt.as_str() == "/" {
            return Ok(());
        }
        let Some(wait) = self.prompt else {
            bail!(Unavailable("the keyring is locked".into()));
        };
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(PROMPT)
            .member("Completed")
            .path(prompt.as_str())
            .build()?;
        // Subscribed before the prompt shows, so its answer isn't missed.
        let mut completed = zbus::MessageStream::for_match_rule(rule, &self.conn, None)
            .await
            .map_err(transient)?;
        let () = self.call(prompt.as_str(), PROMPT, "Prompt", &("",)).await?;
        let answer = within(wait, async {
            match completed.next().await {
                Some(Ok(m)) => {
                    let (dismissed, _): (bool, OwnedVariant) =
                        m.body().deserialize().map_err(transient)?;
                    anyhow::Ok(dismissed)
                }
                _ => bail!(Unavailable(
                    "the keyring went away during its unlock prompt".into()
                )),
            }
        })
        .await;
        match answer {
            Some(Ok(false)) => Ok(()),
            Some(Ok(true)) => bail!(Unavailable("unlocking the keyring was dismissed".into())),
            Some(Err(e)) => Err(e),
            None => {
                let _: Result<()> = self.call(prompt.as_str(), PROMPT, "Dismiss", &()).await;
                bail!(Unavailable(format!(
                    "the keyring wasn't unlocked within {} s",
                    wait.as_secs()
                )))
            }
        }
    }
}

#[cfg(test)]
pub mod mock;

#[cfg(test)]
mod tests {
    use super::mock::{Answer, Mock, bus};
    use super::*;
    use crate::index::seal::{Seal, is_unavailable, sealed_with};
    use std::time::Instant;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn blob(id: &str) -> Vec<u8> {
        format!("{HEAD}{id}\n").into_bytes()
    }

    #[test]
    fn key_files_name_an_item_and_nothing_else() {
        assert_eq!(id_of(&blob(ID)), Some(ID));
        assert_eq!(id_of(b"secret-service 1 short\n"), None);
        assert_eq!(id_of(b"VbntHThZTUOoMZ0uuzMqxiAAAAAB"), None);
        // Never read as a systemd-creds credential, nor one as this.
        assert_eq!(sealed_with(&blob(ID)), Some(Seal::Keyring));
        assert_eq!(sealed_with(b"secret-service 1 junk"), None);
    }

    #[test]
    fn stores_and_fetches_through_an_encrypted_session() {
        let Some(m) = Mock::start("roundtrip") else {
            eprintln!("no dbus-daemon: skipped");
            return;
        };
        let k = m.keyring(Some(Duration::from_secs(5)));
        let secret = [0x5a; 32];
        let b = k.store(&secret).unwrap();
        // The file names the item, and holds nothing of the secret.
        let id = id_of(&b).unwrap().to_owned();
        assert!(!b.windows(4).any(|w| w == [0x5a; 4]));
        assert_eq!(m.state().items.len(), 1);
        assert_eq!(m.state().items[0].0, id);
        assert_eq!(*m.state().items[0].1, secret);
        assert_eq!(*k.fetch(&b).unwrap(), secret);
        // Another key is another item.
        let b2 = k.store(&[1; 32]).unwrap();
        assert_ne!(b, b2);
        assert_eq!(*k.fetch(&b2).unwrap(), [1; 32]);
        k.delete(&b).unwrap();
        assert_eq!(m.state().items.len(), 1);
        // Gone, or in another keyring: waited out, never a lost key.
        let e = k.fetch(&b).unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        assert!(format!("{e:#}").contains("no item"), "{e:#}");
    }

    #[test]
    fn a_locked_keyring_prompts_once_and_only_when_allowed() {
        let Some(m) = Mock::start("locked") else {
            return;
        };
        let k = m.keyring(Some(Duration::from_secs(5)));
        let b = k.store(&[3; 32]).unwrap();
        m.state().locked = true;
        // Quiet: no prompt, and Unavailable.
        let e = m.keyring(None).fetch(&b).unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        assert!(format!("{e:#}").contains("locked"), "{e:#}");
        assert_eq!(m.state().prompts, 0);
        // Dismissed by the user: Unavailable, still locked.
        m.state().answer = Answer::Dismiss;
        let e = k.fetch(&b).unwrap_err();
        assert!(
            is_unavailable(&e) && format!("{e:#}").contains("dismissed"),
            "{e:#}"
        );
        assert!(m.state().locked);
        // Unlocked at the prompt.
        m.state().answer = Answer::Unlock;
        assert_eq!(*k.fetch(&b).unwrap(), [3; 32]);
        assert_eq!(m.state().prompts, 2);
    }

    #[test]
    fn an_unanswered_prompt_is_dismissed_in_time() {
        let Some(m) = Mock::start("ignored") else {
            return;
        };
        let b = m.keyring(None).store(&[4; 32]).unwrap();
        {
            let mut st = m.state();
            st.locked = true;
            st.answer = Answer::Ignore;
        }
        let t = Instant::now();
        let e = m
            .keyring(Some(Duration::from_millis(300)))
            .fetch(&b)
            .unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(
            is_unavailable(&e) && format!("{e:#}").contains("within"),
            "{e:#}"
        );
        assert!(m.state().dismissed);
        // Storing into a locked keyring waits the same way.
        let e = m
            .keyring(Some(Duration::from_millis(300)))
            .store(&[5; 32])
            .unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        assert_eq!(m.state().items.len(), 1);
    }

    #[test]
    fn no_secret_service_is_unavailable() {
        let Some((mut daemon, address, dir)) = bus("empty") else {
            return;
        };
        let k = Keyring {
            address: Some(address),
            prompt: Some(Duration::from_secs(1)),
        };
        let e = k.fetch(&blob(ID)).unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        assert!(format!("{e:#}").contains("no Secret Service"), "{e:#}");
        assert!(is_unavailable(&k.store(&[0; 32]).unwrap_err()));
        let _ = daemon.kill();
        let _ = daemon.wait();
        let _ = std::fs::remove_dir_all(dir);
        // No bus at all.
        let k = Keyring {
            address: Some("unix:path=/nonexistent/bus".into()),
            prompt: None,
        };
        assert!(is_unavailable(&k.fetch(&blob(ID)).unwrap_err()));
    }

    /// Against a real Secret Service, on the session bus it is run under:
    /// `VINOSEARCH_REAL_KEYRING=1`, inside a throwaway `dbus-run-session`
    /// with its own keyring daemon, never the desktop's. Writes the stored
    /// secret's id and hex to `$VINOSEARCH_REAL_KEYRING_OUT` for
    /// `secret-tool` to check.
    #[test]
    fn real_secret_service() {
        if std::env::var_os("VINOSEARCH_REAL_KEYRING").is_none() {
            return;
        }
        let k = Keyring {
            address: None,
            prompt: Some(Duration::from_secs(5)),
        };
        let mut secret = [0u8; 32];
        random(&mut secret).unwrap();
        let t = Instant::now();
        let b = k.store(&secret).unwrap();
        let stored = t.elapsed();
        let t = Instant::now();
        assert_eq!(*k.fetch(&b).unwrap(), secret);
        eprintln!("store {stored:?}, fetch {:?}", t.elapsed());
        let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
        if let Some(out) = std::env::var_os("VINOSEARCH_REAL_KEYRING_OUT") {
            std::fs::write(out, format!("{} {hex}\n", id_of(&b).unwrap())).unwrap();
        }
        // One stored by libsecret, read back here.
        if let Some(id) = std::env::var_os("VINOSEARCH_REAL_KEYRING_ID") {
            let theirs = k.fetch(&blob(id.to_str().unwrap())).unwrap();
            eprintln!("libsecret's: {}", String::from_utf8_lossy(&theirs));
        }
    }
}
