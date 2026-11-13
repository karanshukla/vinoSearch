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
//! session is `plain`: the key crosses the session bus unencrypted. Only this
//! user's processes can connect to that bus, and any of them can ask the
//! unlocked keyring for the item itself, so an encrypted session would hide
//! it from no one this backend defends against. zbus's message buffers
//! aren't wiped; the key is, on our side, once dropped. The Flatpak talks to
//! the same Secret Service (`--talk-name=org.freedesktop.secrets`), not the
//! Secret portal.

use super::{Unavailable, random};
use anyhow::{Context, Result, anyhow, bail};
use futures_lite::StreamExt;
use std::collections::HashMap;
use std::future::Future;
use std::sync::mpsc;
use std::time::Duration;
use zbus::{OwnedObjectPath, OwnedVariant, Variant};
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
const ITEM: &str = "org.freedesktop.Secret.Item";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
const CONTENT_TYPE: &str = "application/octet-stream";
/// A call the Secret Service doesn't answer in this long (activating
/// included) is taken as no Secret Service.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The Secret Service on the session bus (or `address`, for tests).
#[derive(Clone, Debug, Default)]
pub struct Keyring {
    pub address: Option<String>,
    /// How long a prompt may wait for the user; None never shows one, so a
    /// locked keyring is `Unavailable` at once.
    pub prompt: Option<Duration>,
}

/// `(session, parameters, value, content type)`; parameters are empty in a
/// `plain` session.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

impl Keyring {
    /// Store `secret` as a new item in the default collection; what
    /// `index.key` holds for it.
    pub fn store(&self, secret: &[u8]) -> Result<Vec<u8>> {
        let mut id = [0u8; 16];
        random(&mut id)?;
        let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
        self.block(async |c: &Client| {
            let collection: OwnedObjectPath = c.service("ReadAlias", &("default",)).await?;
            if collection.as_str() == "/" {
                bail!(Unavailable(
                    "the keyring has no default collection to keep the key in".into()
                ));
            }
            c.unlock(&collection).await?;
            let attributes = HashMap::from([("xdg:schema", SCHEMA), ("id", id.as_str())]);
            let props = HashMap::from([
                ("org.freedesktop.Secret.Item.Label", Variant::from(LABEL)),
                (
                    "org.freedesktop.Secret.Item.Attributes",
                    Variant::from(attributes),
                ),
            ]);
            let secret: (&OwnedObjectPath, &[u8], &[u8], &str) =
                (&c.session, &[], secret, CONTENT_TYPE);
            let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = c
                .call(
                    collection.as_str(),
                    "org.freedesktop.Secret.Collection",
                    "CreateItem",
                    &(props, secret, false),
                )
                .await?;
            if item.as_str() == "/" {
                c.prompt(&prompt).await?;
            }
            Ok(())
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
            let (_, _, value, _): Secret = c
                .call(item.as_str(), ITEM, "GetSecret", &(&c.session,))
                .await?;
            Ok(Zeroizing::new(value))
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
                let _prompt: OwnedObjectPath = c.call(item.as_str(), ITEM, "Delete", &()).await?;
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
                    let r = f(&c).await;
                    let _: Result<()> = c
                        .call(
                            c.session.as_str(),
                            "org.freedesktop.Secret.Session",
                            "Close",
                            &(),
                        )
                        .await;
                    r
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
    session: OwnedObjectPath,
    prompt: Option<Duration>,
}

/// Errors that say the Secret Service isn't there or didn't answer: worth
/// waiting out, never a reason to drop the index.
fn transient(e: zbus::Error) -> anyhow::Error {
    use zbus::fdo::Error as Fdo;
    const NONE: &str =
        "no Secret Service (a keyring such as gnome-keyring or KWallet) on the session bus";
    let why = match &e {
        zbus::Error::FDO(f) => match &**f {
            Fdo::ServiceUnknown(_)
            | Fdo::NameHasNoOwner(_)
            | Fdo::SpawnServiceNotFound(_)
            | Fdo::SpawnChildExited(_) => NONE.into(),
            Fdo::TimedOut(_) | Fdo::NoReply(_) => "the keyring didn't answer".into(),
            Fdo::NotSupported(_) => "the keyring refused a plain session".into(),
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
        let reply = conn
            .call_method(
                Some(BUS),
                SERVICE_PATH,
                Some(SERVICE),
                "OpenSession",
                &("plain", Variant::from("")),
            )
            .await
            .map_err(transient)?;
        let (_, session): (OwnedVariant, OwnedObjectPath) =
            reply.body().deserialize().map_err(transient)?;
        Ok(Client {
            conn,
            session,
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

    async fn service<B, R>(&self, method: &str, body: &B) -> Result<R>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        self.call(SERVICE_PATH, SERVICE, method, body).await
    }

    /// The item for `id`, unlocked (prompting if allowed); None if there is
    /// none.
    async fn find(&self, id: &str) -> Result<Option<OwnedObjectPath>> {
        let attributes = HashMap::from([("xdg:schema", SCHEMA), ("id", id)]);
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            self.service("SearchItems", &(attributes,)).await?;
        if let Some(item) = unlocked.into_iter().next() {
            return Ok(Some(item));
        }
        let Some(item) = locked.into_iter().next() else {
            return Ok(None);
        };
        self.unlock(&item).await?;
        Ok(Some(item))
    }

    async fn unlock(&self, object: &OwnedObjectPath) -> Result<()> {
        let (unlocked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
            self.service("Unlock", &(vec![object],)).await?;
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
                    "the keyring went away during its prompt".into()
                )),
            }
        })
        .await;
        match answer {
            Some(Ok(false)) => Ok(()),
            Some(Ok(true)) => bail!(Unavailable("the keyring's prompt was dismissed".into())),
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
    fn a_keyring_refusing_a_plain_session_is_unavailable() {
        let Some(m) = Mock::start("noplain") else {
            return;
        };
        let k = m.keyring(Some(Duration::from_secs(5)));
        let b = k.store(&[6; 32]).unwrap();
        m.state().refuse_plain = true;
        let e = k.fetch(&b).unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        assert!(format!("{e:#}").contains("plain session"), "{e:#}");
        assert!(is_unavailable(&k.store(&[7; 32]).unwrap_err()));
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
