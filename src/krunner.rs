//! The runner on the session bus: KRunner's org.kde.krunner1 at /runner, and
//! dev.vinosearch.Control1 at /control for the settings app. Queries go to the
//! worker thread, which owns the model; the watcher keeps the index current.
//!
//! Handlers never block the bus: Match awaits the worker's reply, and Control
//! reads shared status and its own database connection, so a status query
//! answers while the worker is busy indexing.

use crate::config::Config;
use crate::index::{self, Hit};
use crate::watch;
use crate::worker::{self, Job, Options, Status};
use anyhow::{Context, Result};
use futures_channel::oneshot;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

const SERVICE: &str = "dev.vinosearch.Runner";
const OBJECT: &str = "/runner";
const CONTROL: &str = "/control";
const RESULTS: usize = 5;
/// Cosine below this is noise for this model: an off-topic query against a
/// code index topped out at 0.60, real matches scored 0.72 to 0.83.
const MIN_SCORE: f32 = 0.65;
/// QueryMatch::CategoryRelevance::Moderate
const RELEVANCE_MODERATE: i32 = 50;

type Match = (
    String,
    String,
    String,
    i32,
    f64,
    HashMap<String, OwnedValue>,
);

struct Runner {
    jobs: Mutex<mpsc::Sender<Job>>,
}

fn owned(v: Value<'_>) -> OwnedValue {
    v.try_into().expect("string values have no fds")
}

fn to_match(h: Hit) -> Match {
    let path = Path::new(&h.path);
    let name = path
        .file_name()
        .map_or(h.path.clone(), |n| n.to_string_lossy().into_owned());
    let dir = path
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let snippet = crate::snippet(&h.text);
    let mut props = HashMap::new();
    props.insert(
        "subtext".into(),
        owned(Value::from(format!("{dir}:{}  {snippet}", h.line))),
    );
    props.insert(
        "urls".into(),
        owned(Value::from(vec![format!("file://{}", h.path)])),
    );
    let relevance = f64::from(((h.score - MIN_SCORE) / (1.0 - MIN_SCORE)).clamp(0.0, 1.0));
    (
        h.path,
        name,
        "text-x-generic".into(),
        RELEVANCE_MODERATE,
        relevance,
        props,
    )
}

fn open(target: &Path) {
    if let Err(e) = std::process::Command::new("xdg-open").arg(target).spawn() {
        eprintln!("xdg-open {}: {e}", target.display());
    }
}

#[interface(name = "org.kde.krunner1")]
impl Runner {
    #[zbus(name = "Actions")]
    fn actions(&self) -> Vec<(String, String, String)> {
        vec![(
            "open-folder".into(),
            "Open containing folder".into(),
            "document-open-folder".into(),
        )]
    }

    #[zbus(name = "Match")]
    async fn find(&self, query: String) -> Vec<Match> {
        let (tx, rx) = oneshot::channel();
        let job = Job::Match {
            query,
            k: RESULTS * 8,
            reply: tx,
        };
        if self.jobs.lock().unwrap().send(job).is_err() {
            return Vec::new();
        }
        per_file(rx.await.unwrap_or_default())
            .into_iter()
            .map(to_match)
            .collect()
    }

    #[zbus(name = "Run")]
    fn run(&self, match_id: String, action_id: String) {
        let path = PathBuf::from(&match_id);
        match action_id.as_str() {
            "open-folder" => open(path.parent().unwrap_or(&path)),
            _ => open(&path),
        }
    }

    #[zbus(name = "Teardown")]
    fn teardown(&self) {}
}

struct Control {
    jobs: Mutex<mpsc::Sender<Job>>,
    status: Arc<Status>,
    db: Mutex<rusqlite::Connection>,
    config: PathBuf,
    device: String,
}

fn failed(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

impl Control {
    fn send(&self, job: Job) -> zbus::fdo::Result<()> {
        self.jobs
            .lock()
            .unwrap()
            .send(job)
            .map_err(|_| failed("the worker has stopped"))
    }

    /// An existing directory, canonical, not excluded.
    fn root(&self, path: &str) -> zbus::fdo::Result<PathBuf> {
        let p = fs::canonicalize(path).map_err(|e| failed(format!("{path}: {e}")))?;
        if !p.is_dir() {
            return Err(failed(format!("{path} is not a directory")));
        }
        let ex = Config::load(&self.config)
            .and_then(|c| c.excludes())
            .map_err(|e| failed(format!("{e:#}")))?;
        if ex.contains(&p) {
            return Err(failed(format!("{} is excluded", p.display())));
        }
        Ok(p)
    }
}

#[interface(name = "dev.vinosearch.Control1")]
impl Control {
    /// model_loaded (b), indexing (b), device (s), files (x), chunks (x), roots (as).
    fn status(&self) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        let db = self.db.lock().unwrap();
        let (files, chunks) = index::counts(&db).map_err(failed)?;
        let roots: Vec<String> = index::roots(&db)
            .map_err(failed)?
            .into_iter()
            .map(|r| r.display().to_string())
            .collect();
        let mut out = HashMap::new();
        out.insert(
            "model_loaded".into(),
            owned(Value::from(self.status.loaded.load(Ordering::Relaxed))),
        );
        out.insert(
            "indexing".into(),
            owned(Value::from(self.status.indexing.load(Ordering::Relaxed))),
        );
        out.insert("device".into(), owned(Value::from(self.device.clone())));
        out.insert("files".into(), owned(Value::from(files)));
        out.insert("chunks".into(), owned(Value::from(chunks)));
        out.insert("roots".into(), owned(Value::from(roots)));
        Ok(out)
    }

    /// Index a directory and watch it from now on. Returns once queued.
    fn reindex(&self, path: String) -> zbus::fdo::Result<()> {
        let root = self.root(&path)?;
        self.send(Job::Reindex(root))
    }

    /// Stop indexing a root and drop what was indexed under it.
    fn forget(&self, path: String) -> zbus::fdo::Result<()> {
        let root = fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
        self.send(Job::Forget(root))
    }

    /// Free the model now.
    fn unload(&self) -> zbus::fdo::Result<()> {
        self.send(Job::Unload)
    }
}

/// Best chunk per file, files above MIN_SCORE, best RESULTS of them.
fn per_file(hits: Vec<Hit>) -> Vec<Hit> {
    let mut seen = HashSet::new();
    hits.into_iter()
        .filter(|h| h.score >= MIN_SCORE && seen.insert(h.path.clone()))
        .take(RESULTS)
        .collect()
}

pub fn serve(opts: Options) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let _watcher = watch::spawn(tx.clone(), opts.config.clone(), opts.excludes.clone())?;
    let control = Control {
        jobs: Mutex::new(tx.clone()),
        status: opts.status.clone(),
        db: Mutex::new(index::open(&index::default_db()?)?),
        config: opts.config.clone(),
        device: opts.device.clone(),
    };
    let worker = std::thread::spawn(move || worker::run(rx, opts));
    let _conn = zbus::blocking::connection::Builder::session()?
        .name(SERVICE)?
        .serve_at(
            OBJECT,
            Runner {
                jobs: Mutex::new(tx),
            },
        )?
        .serve_at(CONTROL, control)?
        .build()
        .context("registering on the session bus")?;
    eprintln!("{SERVICE} on {OBJECT}");
    // The worker only returns on a startup error or when the channel closes.
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("worker panicked"))?
}

fn data_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".local/share"),
    })
}

/// Register the runner with KRunner and let D-Bus start it on demand. Only
/// flags given here are baked into the service; the rest come from the config
/// file each time the runner starts.
pub fn install(
    openvino_dir: Option<&Path>,
    device: Option<&str>,
    idle_unload_minutes: Option<f64>,
) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut exec = exe.display().to_string();
    if let Some(d) = device {
        exec.push_str(&format!(" --device {d}"));
    }
    if let Some(m) = idle_unload_minutes {
        exec.push_str(&format!(" --idle-unload-minutes {m}"));
    }
    if let Some(d) = openvino_dir {
        exec.push_str(&format!(" --openvino-dir {}", d.display()));
    }
    exec.push_str(" krunner");
    let data = data_home()?;
    let plugin = data.join("krunner/dbusplugins/vinosearch.desktop");
    let service = data.join(format!("dbus-1/services/{SERVICE}.service"));
    fs::create_dir_all(plugin.parent().unwrap())?;
    fs::create_dir_all(service.parent().unwrap())?;
    fs::write(
        &plugin,
        format!(
            "[Desktop Entry]\nName=vinoSearch\nComment=Semantic file search on the NPU\n\
             X-KDE-ServiceTypes=Plasma/Runner\nType=Service\nIcon=system-search\n\
             X-KDE-PluginInfo-Name=vinosearch\nX-KDE-PluginInfo-EnabledByDefault=true\n\
             X-Plasma-API=DBus\nX-Plasma-DBusRunner-Service={SERVICE}\nX-Plasma-DBusRunner-Path={OBJECT}\n\
             X-Plasma-Request-Actions-Once=true\nX-Plasma-Runner-Min-Letter-Count=3\n\
             X-Plasma-Runner-Unique-Results=true\n"
        ),
    )?;
    fs::write(
        &service,
        format!("[D-BUS Service]\nName={SERVICE}\nExec={exec}\n"),
    )?;
    println!("wrote {}\nwrote {}", plugin.display(), service.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let data = data_home()?;
    for p in [
        data.join("krunner/dbusplugins/vinosearch.desktop"),
        data.join(format!("dbus-1/services/{SERVICE}.service")),
    ] {
        if p.exists() {
            fs::remove_file(&p)?;
            println!("removed {}", p.display());
        }
    }
    Ok(())
}
