//! The runner on the session bus: KRunner's org.kde.krunner1 at /runner,
//! GNOME Shell's org.gnome.Shell.SearchProvider2 at /gnome, and
//! dev.vinosearch.Control1 at /control for the settings app. Queries go to the
//! worker thread, which owns the model; the watcher keeps the index current.
//!
//! Handlers never block the bus: Match awaits the worker's reply, and Control
//! reads shared status and its own database connection, so a status query
//! answers while the worker is busy indexing.

use crate::config::Config;
use crate::index::{self, Hit};
use crate::model;
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

use vinosearch_shared::dbus::{
    CONTROL_PATH as CONTROL, DESKTOP_ID, GNOME_PATH as GNOME, RUNNER_PATH as OBJECT, SERVICE,
    status as key,
};
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

/// The best chunk of each of the best files for `query`, as KRunner and GNOME
/// Shell both show them. Empty if the worker has stopped.
async fn search(jobs: &Mutex<mpsc::Sender<Job>>, query: String) -> Vec<Hit> {
    let (tx, rx) = oneshot::channel();
    let job = Job::Match {
        query,
        k: RESULTS * 8,
        reply: tx,
    };
    if jobs.lock().unwrap().send(job).is_err() {
        return Vec::new();
    }
    per_file(rx.await.unwrap_or_default())
}

/// `path`'s file name, and its directory with the hit's line and a snippet.
fn describe(h: &Hit) -> (String, String) {
    let path = Path::new(&h.path);
    let name = path
        .file_name()
        .map_or(h.path.clone(), |n| n.to_string_lossy().into_owned());
    let dir = path
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    (
        name,
        format!("{dir}:{}  {}", h.line, crate::snippet(&h.text)),
    )
}

fn owned(v: Value<'_>) -> OwnedValue {
    v.try_into().expect("string values have no fds")
}

fn to_match(h: Hit) -> Match {
    let (name, subtext) = describe(&h);
    let mut props = HashMap::new();
    props.insert("subtext".into(), owned(Value::from(subtext)));
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
        search(&self.jobs, query)
            .await
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

/// GNOME Shell asks for result ids first, then for how to show them, so the
/// last search's hits are kept to describe them without searching again.
struct Gnome {
    jobs: Mutex<mpsc::Sender<Job>>,
    last: Mutex<HashMap<String, Hit>>,
}

/// GNOME Shell searches on every keystroke; KRunner's plugin waits for three
/// letters (X-Plasma-Runner-Min-Letter-Count), so this does too.
const MIN_QUERY_CHARS: usize = 3;

impl Gnome {
    async fn results(&self, terms: Vec<String>) -> Vec<String> {
        let query = terms.join(" ");
        if query.trim().chars().count() < MIN_QUERY_CHARS {
            return Vec::new();
        }
        let hits = search(&self.jobs, query).await;
        let ids = hits.iter().map(|h| h.path.clone()).collect();
        *self.last.lock().unwrap() = hits.into_iter().map(|h| (h.path.clone(), h)).collect();
        ids
    }
}

#[interface(name = "org.gnome.Shell.SearchProvider2")]
impl Gnome {
    #[zbus(name = "GetInitialResultSet")]
    async fn initial(&self, terms: Vec<String>) -> Vec<String> {
        self.results(terms).await
    }

    /// A semantic match does not narrow as the query grows, so this searches
    /// afresh rather than filtering `previous`.
    #[zbus(name = "GetSubsearchResultSet")]
    async fn subsearch(&self, _previous: Vec<String>, terms: Vec<String>) -> Vec<String> {
        self.results(terms).await
    }

    #[zbus(name = "GetResultMetas")]
    fn metas(&self, ids: Vec<String>) -> Vec<HashMap<String, OwnedValue>> {
        let last = self.last.lock().unwrap();
        ids.into_iter()
            .map(|id| {
                let (name, description) = match last.get(&id) {
                    Some(h) => describe(h),
                    // From an earlier search: the path alone.
                    None => (
                        Path::new(&id)
                            .file_name()
                            .map_or(id.clone(), |n| n.to_string_lossy().into_owned()),
                        id.clone(),
                    ),
                };
                let mut meta = HashMap::new();
                meta.insert("name".into(), owned(Value::from(name)));
                meta.insert("description".into(), owned(Value::from(description)));
                meta.insert("gicon".into(), owned(Value::from("text-x-generic")));
                meta.insert("id".into(), owned(Value::from(id)));
                meta
            })
            .collect()
    }

    #[zbus(name = "ActivateResult")]
    fn activate(&self, id: String, _terms: Vec<String>, _timestamp: u32) {
        open(Path::new(&id));
    }

    /// Clicking the provider's icon. Nothing to show yet; the settings app
    /// (#5) is the natural target.
    #[zbus(name = "LaunchSearch")]
    fn launch(&self, _terms: Vec<String>, _timestamp: u32) {}
}

struct Control {
    jobs: Mutex<mpsc::Sender<Job>>,
    status: Arc<Status>,
    db: Mutex<rusqlite::Connection>,
    config: PathBuf,
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
    /// State, storage and progress; keys in vinosearch_shared::dbus::status,
    /// described in docs/dbus.md.
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
            key::MODEL_LOADED.into(),
            owned(Value::from(self.status.loaded.load(Ordering::Relaxed))),
        );
        out.insert(
            key::INDEXING.into(),
            owned(Value::from(
                self.status.indexing.load(Ordering::Relaxed)
                    || self.status.queued_reindex.load(Ordering::Relaxed) > 0,
            )),
        );
        out.insert(
            key::DEVICE.into(),
            owned(Value::from(self.status.device.lock().unwrap().clone())),
        );
        out.insert(key::FILES.into(), owned(Value::from(files)));
        out.insert(key::CHUNKS.into(), owned(Value::from(chunks)));
        out.insert(key::ROOTS.into(), owned(Value::from(roots)));
        let st = &self.status;
        let u64_of = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        out.insert(
            key::INDEXED_FILES.into(),
            owned(Value::from(u64_of(&st.indexed_files))),
        );
        out.insert(
            key::FETCHING.into(),
            owned(Value::from(st.fetching.load(Ordering::Relaxed))),
        );
        out.insert(
            key::FETCH_DONE.into(),
            owned(Value::from(u64_of(&st.fetch_done))),
        );
        out.insert(
            key::FETCH_TOTAL.into(),
            owned(Value::from(u64_of(&st.fetch_total))),
        );
        out.insert(
            key::FETCH_ERROR.into(),
            owned(Value::from(st.fetch_error.lock().unwrap().clone())),
        );
        let model_dir = model::default_dir().map_err(failed)?;
        out.insert(
            key::MODEL_PRESENT.into(),
            owned(Value::from(model::present(&model_dir))),
        );
        let db_path = index::default_db().map_err(failed)?;
        let index_bytes: u64 = ["", "-wal", "-shm"]
            .iter()
            .map(|ext| disk_bytes(Path::new(&format!("{}{ext}", db_path.display()))))
            .sum();
        out.insert(key::INDEX_BYTES.into(), owned(Value::from(index_bytes)));
        out.insert(
            key::MODEL_BYTES.into(),
            owned(Value::from(disk_bytes(&model_dir))),
        );
        let cache = crate::cache_dir().map_err(failed)?;
        out.insert(
            key::CACHE_BYTES.into(),
            owned(Value::from(disk_bytes(&cache))),
        );
        Ok(out)
    }

    /// Index a directory and watch it from now on. Returns once queued.
    fn reindex(&self, path: String) -> zbus::fdo::Result<()> {
        let root = self.root(&path)?;
        self.status.queued_reindex.fetch_add(1, Ordering::Relaxed);
        self.send(Job::Reindex(root)).inspect_err(|_| {
            self.status.queued_reindex.fetch_sub(1, Ordering::Relaxed);
        })
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

    /// Download the model (or whatever of it is missing or corrupt) in the
    /// background. Progress is in Status: fetching, fetch_done, fetch_total,
    /// fetch_error.
    fn fetch_model(&self) -> zbus::fdo::Result<()> {
        if self.status.fetching.swap(true, Ordering::SeqCst) {
            return Err(failed("a download is already running"));
        }
        let status = self.status.clone();
        let jobs = self.jobs.lock().unwrap().clone();
        *status.fetch_error.lock().unwrap() = String::new();
        status.fetch_done.store(0, Ordering::Relaxed);
        status
            .fetch_total
            .store(model::total_bytes(), Ordering::Relaxed);
        std::thread::spawn(move || {
            let result = model::default_dir().and_then(|dir| {
                model::fetch(&dir, &mut |_, done, _| {
                    status.fetch_done.store(done, Ordering::Relaxed);
                })
            });
            match result {
                // Indexing that found no model can run now.
                Ok(()) => {
                    let _ = jobs.send(Job::Retry);
                }
                Err(e) => {
                    eprintln!("fetching the model: {e:#}");
                    *status.fetch_error.lock().unwrap() = format!("{e:#}");
                }
            }
            status.fetching.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    /// Unload and delete the model's files.
    fn delete_model(&self) -> zbus::fdo::Result<()> {
        if self.status.fetching.load(Ordering::SeqCst) {
            return Err(failed("a download is running"));
        }
        self.send(Job::DeleteModel)
    }

    /// Delete the compiled-model cache; the next model load recompiles (seconds).
    fn clear_cache(&self) -> zbus::fdo::Result<()> {
        self.send(Job::ClearCache)
    }
}

/// Bytes under `path`, a file or a directory; 0 if it is missing.
fn disk_bytes(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
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
    let _watcher = watch::spawn(tx.clone(), opts.config.clone(), opts.file.clone())?;
    let control = Control {
        jobs: Mutex::new(tx.clone()),
        status: opts.status.clone(),
        db: Mutex::new(index::open(&index::default_db()?)?),
        config: opts.config.clone(),
    };
    let worker = std::thread::spawn(move || worker::run(rx, opts));
    let _conn = zbus::blocking::connection::Builder::session()?
        .name(SERVICE)?
        .serve_at(
            OBJECT,
            Runner {
                jobs: Mutex::new(tx.clone()),
            },
        )?
        .serve_at(
            GNOME,
            Gnome {
                jobs: Mutex::new(tx),
                last: Mutex::default(),
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

/// GNOME Shell reads search providers from the system data dirs only, never
/// the user's, so the provider file goes in a directory of ours that
/// environment.d appends to XDG_DATA_DIRS, as Flatpak does for its per-user
/// exports. It takes effect from the next login.
struct GnomeFiles {
    provider: PathBuf,
    desktop: PathBuf,
    environment: PathBuf,
    xdg: PathBuf,
}

fn gnome_files() -> Result<GnomeFiles> {
    let data = data_home()?;
    let xdg = data.join("vinosearch/xdg");
    Ok(GnomeFiles {
        provider: xdg.join("gnome-shell/search-providers/dev.vinosearch.search-provider.ini"),
        desktop: data.join("applications").join(DESKTOP_ID),
        environment: config_home()?.join("environment.d/60-vinosearch.conf"),
        xdg,
    })
}

fn install_gnome() -> Result<()> {
    let g = gnome_files()?;
    let xdg = g.xdg.to_str().context("data directory is not UTF-8")?;
    if xdg.contains([':', '$', '\\', '\n']) {
        anyhow::bail!("{xdg}: cannot go in XDG_DATA_DIRS");
    }
    for p in [&g.provider, &g.desktop, &g.environment] {
        fs::create_dir_all(p.parent().unwrap())?;
    }
    fs::write(
        &g.provider,
        format!(
            "[Shell Search Provider]\nDesktopId={DESKTOP_ID}\nBusName={SERVICE}\n\
             ObjectPath={GNOME}\nVersion=2\n"
        ),
    )?;
    // GNOME Shell skips a provider whose desktop entry is hidden
    // (NoDisplay), so this one shows in the app grid. Until the settings app
    // (#5) exists, launching it opens the documentation.
    fs::write(
        &g.desktop,
        "[Desktop Entry]\nType=Application\nName=vinoSearch\n\
         Comment=Semantic file search on the NPU\nIcon=system-search\n\
         Exec=xdg-open https://github.com/karanshukla/vinoSearch#readme\n\
         Categories=Utility;\nTerminal=false\n",
    )?;
    // Appended, so it never shadows anything the system provides. The
    // default is the XDG spec's, for sessions that leave the variable unset.
    fs::write(
        &g.environment,
        format!("XDG_DATA_DIRS=${{XDG_DATA_DIRS:-/usr/local/share/:/usr/share/}}:{xdg}\n"),
    )?;
    println!(
        "wrote {}\nwrote {}\nwrote {} (GNOME Shell search: from the next login)",
        g.provider.display(),
        g.desktop.display(),
        g.environment.display()
    );
    Ok(())
}

fn config_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".config"),
    })
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
const UNIT: &str = "vinosearch.service";

fn unit_path() -> Result<PathBuf> {
    Ok(config_home()?.join("systemd/user").join(UNIT))
}

/// `systemctl --user <args>`, reporting rather than failing: without systemd
/// the runner still starts through D-Bus activation's Exec line.
fn systemctl(args: &[&str]) -> bool {
    match std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
    {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("systemctl --user {}: {s}", args.join(" "));
            false
        }
        Err(e) => {
            eprintln!("systemctl: {e}");
            false
        }
    }
}

/// Whether something already owns the runner's bus name.
fn runner_running() -> bool {
    let owned = || -> Result<bool> {
        let conn = zbus::blocking::Connection::session()?;
        let bus = zbus::blocking::fdo::DBusProxy::new(&conn)?;
        Ok(bus.name_has_owner(SERVICE.try_into()?)?)
    };
    owned().unwrap_or(false)
}

/// Register the runner with KRunner, and with systemd as a user service that
/// starts at login, so the watcher keeps the index current from then on
/// (D-Bus activation alone would start it only at the first query). Only
/// flags given here are baked into the service; the rest come from the
/// config file each time the runner starts.
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
        format!("[D-BUS Service]\nName={SERVICE}\nExec={exec}\nSystemdService={UNIT}\n"),
    )?;
    let unit = unit_path()?;
    fs::create_dir_all(unit.parent().unwrap())?;
    fs::write(
        &unit,
        format!(
            "[Unit]\nDescription=vinoSearch semantic file search (KRunner runner and index watcher)\n\
             Documentation=https://github.com/karanshukla/vinoSearch\n\n\
             [Service]\nType=dbus\nBusName={SERVICE}\nExecStart={exec}\n\
             Restart=on-failure\nRestartSec=10\n\n\
             [Install]\nWantedBy=default.target\n"
        ),
    )?;
    println!(
        "wrote {}\nwrote {}\nwrote {}",
        plugin.display(),
        service.display(),
        unit.display()
    );
    install_gnome()?;
    if systemctl(&["daemon-reload"]) {
        if runner_running() {
            // Starting now would lose the bus name to it and fail; the unit
            // takes over from the next login.
            systemctl(&["enable", UNIT]);
            println!(
                "enabled {UNIT}; a runner is already running, so it starts from the next login"
            );
        } else if systemctl(&["enable", "--now", UNIT]) {
            println!("enabled and started {UNIT}");
        }
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let data = data_home()?;
    let unit = unit_path()?;
    let gnome = gnome_files()?;
    if unit.exists() {
        systemctl(&["disable", "--now", UNIT]);
    }
    for p in [
        data.join("krunner/dbusplugins/vinosearch.desktop"),
        data.join(format!("dbus-1/services/{SERVICE}.service")),
        unit,
        gnome.provider,
        gnome.desktop,
        gnome.environment,
    ] {
        if p.exists() {
            fs::remove_file(&p)?;
            println!("removed {}", p.display());
        }
    }
    systemctl(&["daemon-reload"]);
    Ok(())
}
