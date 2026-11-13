//! A stand-in Secret Service on a private `dbus-daemon` of its own, with no
//! service files, so nothing on it can start a real keyring: one collection,
//! locked or not, and a prompt that unlocks, is dismissed or is never
//! answered. Sessions are `plain` only, as the client asks for.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use zbus::object_server::SignalEmitter;
use zbus::{OwnedObjectPath, OwnedVariant, Variant};
use zeroize::Zeroizing;

const COLLECTION: &str = "/org/freedesktop/secrets/collection/login";
const PROMPT_PATH: &str = "/org/freedesktop/secrets/prompt/p";
/// What the prompt does when shown.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Answer {
    #[default]
    Unlock,
    Dismiss,
    /// Nothing, as a prompt nobody is at the screen for.
    Ignore,
}

#[derive(Default)]
pub struct State {
    pub locked: bool,
    pub answer: Answer,
    /// Every item: its `id` attribute, its secret, its path.
    pub items: Vec<(String, Zeroizing<Vec<u8>>, String)>,
    /// Prompts shown.
    pub prompts: usize,
    /// The client dismissed a prompt it gave up on.
    pub dismissed: bool,
    /// Refuses a `plain` session, as a keyring might.
    pub refuse_plain: bool,
    sessions: usize,
}

type Shared = Arc<Mutex<State>>;

fn state(s: &Shared) -> MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn failed(name: &str) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(name.into())
}

struct Service(Shared);

#[zbus::interface(name = "org.freedesktop.Secret.Service")]
impl Service {
    fn open_session(
        &self,
        algorithm: String,
        input: OwnedVariant,
    ) -> zbus::fdo::Result<(OwnedVariant, OwnedObjectPath)> {
        let mut st = state(&self.0);
        if algorithm != "plain" || st.refuse_plain {
            return Err(zbus::fdo::Error::NotSupported(algorithm));
        }
        if Vec::<u8>::try_from(input.clone()).is_ok() {
            return Err(failed("a plain session's input is an empty string"));
        }
        st.sessions += 1;
        let path = format!("/org/freedesktop/secrets/session/{}", st.sessions);
        Ok((
            OwnedVariant::try_from(Variant::from("")).unwrap(),
            OwnedObjectPath::try_from(path).unwrap(),
        ))
    }

    fn search_items(
        &self,
        attributes: HashMap<String, String>,
    ) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
        let st = state(&self.0);
        let found: Vec<OwnedObjectPath> = st
            .items
            .iter()
            .filter(|(id, _, _)| {
                attributes.get("xdg:schema").map(String::as_str) == Some(super::SCHEMA)
                    && attributes.get("id") == Some(id)
            })
            .map(|(_, _, p)| OwnedObjectPath::try_from(p.as_str()).unwrap())
            .collect();
        if st.locked {
            (Vec::new(), found)
        } else {
            (found, Vec::new())
        }
    }

    fn unlock(&self, objects: Vec<OwnedObjectPath>) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
        let st = state(&self.0);
        if !st.locked {
            return (objects, OwnedObjectPath::try_from("/").unwrap());
        }
        (Vec::new(), OwnedObjectPath::try_from(PROMPT_PATH).unwrap())
    }

    fn read_alias(&self, name: String) -> OwnedObjectPath {
        OwnedObjectPath::try_from(if name == "default" { COLLECTION } else { "/" }).unwrap()
    }
}

struct Collection(Shared);

type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

#[zbus::interface(name = "org.freedesktop.Secret.Collection")]
impl Collection {
    async fn create_item(
        &self,
        properties: HashMap<String, OwnedVariant>,
        secret: Secret,
        _replace: bool,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
        let attributes: HashMap<String, String> = properties
            .get("org.freedesktop.Secret.Item.Attributes")
            .and_then(|v| HashMap::try_from(v.clone()).ok())
            .ok_or_else(|| failed("no attributes"))?;
        let path = {
            let mut st = state(&self.0);
            if st.locked {
                return Err(failed("org.freedesktop.Secret.Error.IsLocked"));
            }
            if !secret.1.is_empty() {
                return Err(failed("a plain session's secret has no parameters"));
            }
            let plain = Zeroizing::new(secret.2);
            let path = format!("{COLLECTION}/{}", st.items.len());
            let id = attributes.get("id").cloned().unwrap_or_default();
            st.items.push((id, plain, path.clone()));
            path
        };
        server
            .at(path.as_str(), Item(self.0.clone(), path.clone()))
            .await?;
        Ok((
            OwnedObjectPath::try_from(path).unwrap(),
            OwnedObjectPath::try_from("/").unwrap(),
        ))
    }
}

struct Item(Shared, String);

#[zbus::interface(name = "org.freedesktop.Secret.Item")]
impl Item {
    fn get_secret(&self, session: OwnedObjectPath) -> zbus::fdo::Result<Secret> {
        let st = state(&self.0);
        if st.locked {
            return Err(failed("org.freedesktop.Secret.Error.IsLocked"));
        }
        let (_, plain, _) = st
            .items
            .iter()
            .find(|(_, _, p)| *p == self.1)
            .ok_or_else(|| failed("deleted"))?;
        Ok((
            session,
            Vec::new(),
            plain.to_vec(),
            "application/octet-stream".into(),
        ))
    }

    fn delete(&self) -> OwnedObjectPath {
        state(&self.0).items.retain(|(_, _, p)| *p != self.1);
        OwnedObjectPath::try_from("/").unwrap()
    }
}

struct Prompt(Shared);

#[zbus::interface(name = "org.freedesktop.Secret.Prompt")]
impl Prompt {
    async fn prompt(
        &self,
        _window_id: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let answer = {
            let mut st = state(&self.0);
            st.prompts += 1;
            st.answer
        };
        match answer {
            Answer::Unlock => {
                state(&self.0).locked = false;
                Self::completed(&emitter, false, Variant::from("")).await?;
            }
            Answer::Dismiss => Self::completed(&emitter, true, Variant::from("")).await?,
            Answer::Ignore => {}
        }
        Ok(())
    }

    async fn dismiss(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        state(&self.0).dismissed = true;
        Self::completed(&emitter, true, Variant::from("")).await?;
        Ok(())
    }

    #[zbus(signal)]
    async fn completed(
        emitter: &SignalEmitter<'_>,
        dismissed: bool,
        result: Variant<'_>,
    ) -> zbus::Result<()>;
}

/// The bus and the service on it; both stop when dropped.
pub struct Mock {
    pub address: String,
    shared: Shared,
    daemon: Child,
    stop: Option<futures_channel::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
}

/// A private bus with nothing on it, no service files: None where
/// `dbus-daemon` isn't installed.
pub fn bus(tag: &str) -> Option<(Child, String, PathBuf)> {
    let daemon = ["/usr/bin/dbus-daemon", "/bin/dbus-daemon"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())?;
    let dir = std::env::temp_dir().join(format!("vinosearch-bus-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let address = format!("unix:path={}", dir.join("bus").display());
    let conf = dir.join("bus.conf");
    std::fs::write(
        &conf,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
        ),
    )
    .unwrap();
    let child = Command::new(daemon)
        .arg(format!("--config-file={}", conf.display()))
        .arg("--nofork")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    for _ in 0..200 {
        if dir.join("bus").exists() {
            return Some((child, address, dir));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    None
}

impl Mock {
    pub fn start(tag: &str) -> Option<Mock> {
        let (daemon, address, dir) = bus(tag)?;
        let shared = Shared::default();
        let (stop, stopped) = futures_channel::oneshot::channel::<()>();
        let (ready, is_ready) = std::sync::mpsc::channel();
        let (s, a) = (shared.clone(), address.clone());
        let thread = std::thread::spawn(move || {
            zbus::block_on(async move {
                let conn = zbus::connection::Builder::address(a.as_str())
                    .name(super::BUS)
                    .serve_at(super::SERVICE_PATH, Service(s.clone()))
                    .serve_at(COLLECTION, Collection(s.clone()))
                    .serve_at(PROMPT_PATH, Prompt(s.clone()))
                    .build()
                    .await;
                let _ = ready.send(conn.is_ok());
                let _ = stopped.await;
                drop(conn);
            })
        });
        if !is_ready.recv().unwrap_or(false) {
            return None;
        }
        Some(Mock {
            address,
            shared,
            daemon,
            stop: Some(stop),
            thread: Some(thread),
            dir,
        })
    }

    pub fn keyring(&self, prompt: Option<std::time::Duration>) -> super::Keyring {
        super::Keyring {
            address: Some(self.address.clone()),
            prompt,
        }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        state(&self.shared)
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
