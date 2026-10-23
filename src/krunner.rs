//! The runner on the session bus: KRunner's org.kde.krunner1 at /runner,
//! GNOME Shell's org.gnome.Shell.SearchProvider2 at /gnome, and
//! dev.vinosearch.Control1 at /control for the settings app. Queries go to the
//! worker thread, which owns the model; the watcher keeps the index current.
//!
//! Handlers never block the bus: Match awaits the worker's reply, and Control
//! reads shared status and its own database connection, so a status query
//! answers while the worker is busy indexing.

use crate::config::Config;
use crate::index::{self, Hit, Kind};
use crate::model;
use crate::watch;
use crate::worker::{self, Job, Options, Status};
use anyhow::{Context, Result, bail};
use futures_channel::oneshot;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

use vinosearch_shared::dbus::{
    CONTROL_PATH as CONTROL, DBUS_SERVICE_FILE, DESKTOP_ID, GNOME_PATH as GNOME, GNOME_PROVIDER,
    ICON, KRUNNER_PLUGIN, RUNNER_PATH as OBJECT, SERVICE, SETTINGS_APP_ID, recent, status as key,
};
/// Files of each kind shown.
const RESULTS: usize = 5;
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

/// One connection for the handlers that read the index; queries are short.
type Db = Arc<Mutex<rusqlite::Connection>>;

struct Runner {
    jobs: Mutex<mpsc::Sender<Job>>,
    db: Db,
    config: PathBuf,
}

/// How close a result must be, per kind (`formats.*.min_score`).
#[derive(Clone, Copy)]
struct Floors {
    text: f32,
    image: f32,
}

impl Floors {
    /// From the config file as it is now, so a change in the settings window
    /// shows from the next keystroke; the defaults if it doesn't parse.
    fn load(config: &Path) -> Self {
        let formats = Config::load(config).map(|c| c.formats).unwrap_or_default();
        Self {
            text: formats
                .text_min_score()
                .unwrap_or(crate::config::DEFAULT_TEXT_MIN_SCORE),
            image: formats
                .image_min_score()
                .unwrap_or(crate::config::DEFAULT_IMAGE_MIN_SCORE),
        }
    }

    fn of(&self, kind: Kind) -> f32 {
        match kind {
            Kind::Text | Kind::Document => self.text,
            Kind::Image => self.image,
        }
    }
}

/// The best chunk of each of the best files for `query`, text first, then
/// documents, then images, as KRunner and GNOME Shell both show them. Empty if the worker
/// has stopped.
async fn search(jobs: &Mutex<mpsc::Sender<Job>>, floors: Floors, query: String) -> Vec<Hit> {
    let (tx, rx) = oneshot::channel();
    let job = Job::Match {
        query,
        k: RESULTS * 8,
        reply: tx,
    };
    if jobs.lock().unwrap().send(job).is_err() {
        return Vec::new();
    }
    per_file(rx.await.unwrap_or_default(), floors)
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
    let copies = match h.copies.len() {
        0 => String::new(),
        1 => "  (+1 copy)".to_owned(),
        n => format!("  (+{n} copies)"),
    };
    let description = match h.kind {
        Kind::Text => format!("{dir}:{}{copies}  {}", h.line, crate::snippet(&h.text)),
        Kind::Image => format!("{dir}{copies}"),
        Kind::Document => format!(
            "{dir}, {} {}{copies}  {}",
            crate::extract::unit(path),
            h.line,
            crate::snippet(&h.text)
        ),
    };
    (name, description)
}

fn owned(v: Value<'_>) -> OwnedValue {
    v.try_into().expect("string values have no fds")
}

fn to_match(h: Hit, floors: Floors) -> Match {
    let (name, subtext) = describe(&h);
    let mut props = HashMap::new();
    props.insert("subtext".into(), owned(Value::from(subtext)));
    props.insert("urls".into(), owned(Value::from(vec![file_url(&h.path)])));
    // A group per kind, named so each reads as ours (#73).
    let category = match h.kind {
        Kind::Text => "vinoSearch Text",
        Kind::Document => "vinoSearch Documents",
        Kind::Image => "vinoSearch Images",
    };
    props.insert("category".into(), owned(Value::from(category)));
    let floor = floors.of(h.kind);
    let (icon, _) = crate::icons::for_path(Path::new(&h.path), h.kind);
    let relevance = f64::from(((h.score - floor) / (1.0 - floor)).clamp(0.0, 1.0));
    (
        h.path,
        name,
        icon.into(),
        RELEVANCE_MODERATE,
        relevance,
        props,
    )
}

/// `path` as a file:// URL, percent-encoded so `#`, `%`, `?` and spaces
/// survive (RFC 8089; `/` stays as the separator).
fn file_url(path: &str) -> String {
    let mut url = String::from("file://");
    for &b in path.as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// Whether `id` is an indexed file. Ids come from callers on the session
/// bus, so anything this runner could not have returned is refused.
fn indexed(db: &Db, id: &str, doing: &str) -> bool {
    match index::is_indexed(&db.lock().unwrap(), id) {
        Ok(true) => true,
        Ok(false) => {
            eprintln!("not {doing} {id}: not an indexed file");
            false
        }
        Err(e) => {
            eprintln!("not {doing} {id}: {e:#}");
            false
        }
    }
}

/// Open the indexed file `id`, or its folder.
fn open(db: &Db, id: &str, folder: bool) {
    if !indexed(db, id, "opening") {
        return;
    }
    let path = Path::new(id);
    let target = if folder {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    if let Err(e) = std::process::Command::new("xdg-open").arg(target).spawn() {
        eprintln!("xdg-open {}: {e}", target.display());
    }
}

/// The text of the indexed file `id`, for Control1.FileText: a text file's
/// contents (an HTML page's without its tags), or a document's text from the
/// extractor child (seconds for a big PDF). Err for anything else, an image
/// included.
fn file_text(db: &Db, id: &str) -> Result<String> {
    if !index::is_indexed(&db.lock().unwrap(), id)? {
        bail!("{id} is not an indexed file");
    }
    let path = Path::new(id);
    match index::readable_kind(path) {
        Some(Kind::Text) => {
            let mut text = String::new();
            fs::File::open(path)?
                .take(index::MAX_FILE_BYTES + 1)
                .read_to_string(&mut text)
                .with_context(|| format!("{id}: not UTF-8"))?;
            if text.len() as u64 > index::MAX_FILE_BYTES {
                bail!("{id} is over {} bytes", index::MAX_FILE_BYTES);
            }
            if crate::extract::is_html(path) {
                return Ok(crate::extract::html(&text));
            }
            Ok(text)
        }
        Some(Kind::Document) => {
            crate::extract::text(path).with_context(|| format!("extracting {id}"))
        }
        _ => bail!("{id} is not a text file or document"),
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
        let floors = Floors::load(&self.config);
        search(&self.jobs, floors, query)
            .await
            .into_iter()
            .map(|h| to_match(h, floors))
            .collect()
    }

    #[zbus(name = "Run")]
    fn run(&self, match_id: String, action_id: String) {
        open(&self.db, &match_id, action_id == "open-folder");
    }

    #[zbus(name = "Teardown")]
    fn teardown(&self) {}
}

/// GNOME Shell asks for result ids first, then for how to show them, so the
/// last search's hits are kept to describe them without searching again.
struct Gnome {
    jobs: Mutex<mpsc::Sender<Job>>,
    db: Db,
    config: PathBuf,
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
        let hits = search(&self.jobs, Floors::load(&self.config), query).await;
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
                let (name, description, kind) = match last.get(&id) {
                    Some(h) => {
                        let (name, description) = describe(h);
                        (name, description, h.kind)
                    }
                    // From an earlier search: the path alone.
                    None => (
                        Path::new(&id)
                            .file_name()
                            .map_or(id.clone(), |n| n.to_string_lossy().into_owned()),
                        id.clone(),
                        Kind::Text,
                    ),
                };
                let mut meta = HashMap::new();
                meta.insert("name".into(), owned(Value::from(name)));
                meta.insert("description".into(), owned(Value::from(description)));
                // A serialized GThemedIcon, which falls back to the generic name.
                let (specific, generic) = crate::icons::for_path(Path::new(&id), kind);
                let gicon = format!(". GThemedIcon {specific} {generic}");
                meta.insert("gicon".into(), owned(Value::from(gicon)));
                meta.insert("id".into(), owned(Value::from(id)));
                meta
            })
            .collect()
    }

    #[zbus(name = "ActivateResult")]
    fn activate(&self, id: String, _terms: Vec<String>, _timestamp: u32) {
        open(&self.db, &id, false);
    }

    /// Clicking the provider's icon: the settings window, when it is
    /// installed beside us.
    #[zbus(name = "LaunchSearch")]
    fn launch(&self, _terms: Vec<String>, _timestamp: u32) {
        let settings = std::env::current_exe().map(|e| e.with_file_name("vinosearch-settings"));
        match settings {
            Ok(s) if s.is_file() => {
                if let Err(e) = std::process::Command::new(&s).spawn() {
                    eprintln!("{}: {e}", s.display());
                }
            }
            _ => eprintln!("LaunchSearch: vinosearch-settings is not installed beside vinosearch"),
        }
    }
}

struct Control {
    jobs: Mutex<mpsc::Sender<Job>>,
    status: Arc<Status>,
    db: Db,
    config: PathBuf,
}

fn failed(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

impl Control {
    /// Images are on in the config file as it is now; off if it doesn't parse.
    fn images_enabled(&self) -> bool {
        Config::load(&self.config).is_ok_and(|c| c.formats.images_enabled())
    }

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
            key::PAUSED.into(),
            owned(Value::from(self.status.paused.load(Ordering::Relaxed))),
        );
        out.insert(
            key::DEVICE.into(),
            owned(Value::from(self.status.device.lock().unwrap().clone())),
        );
        out.insert(key::FILES.into(), owned(Value::from(files)));
        out.insert(key::CHUNKS.into(), owned(Value::from(chunks)));
        let [text, documents, images] = index::counts_by_kind(&db).map_err(failed)?;
        for (k, n) in [
            (key::TEXT_FILES, text.0),
            (key::TEXT_CHUNKS, text.1),
            (key::DOCUMENT_FILES, documents.0),
            (key::DOCUMENT_CHUNKS, documents.1),
            (key::IMAGE_FILES, images.0),
        ] {
            out.insert(k.into(), owned(Value::from(n)));
        }
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
        let vision_dir = model::vision_dir().map_err(failed)?;
        // The vision encoder counts while images are on.
        let present = model::present(&model_dir)
            && (!self.images_enabled() || model::images_present(&model_dir, &vision_dir));
        out.insert(key::MODEL_PRESENT.into(), owned(Value::from(present)));
        let db_path = index::default_db().map_err(failed)?;
        let index_bytes: u64 = ["", "-wal", "-shm"]
            .iter()
            .map(|ext| disk_bytes(Path::new(&format!("{}{ext}", db_path.display()))))
            .sum();
        out.insert(key::INDEX_BYTES.into(), owned(Value::from(index_bytes)));
        out.insert(
            key::MODEL_BYTES.into(),
            owned(Value::from(
                disk_bytes(&model_dir) + disk_bytes(&vision_dir),
            )),
        );
        for (k, at) in [
            (key::LAST_CHECKED, &st.last_checked),
            (key::LAST_INDEXED, &st.last_indexed),
        ] {
            out.insert(k.into(), owned(Value::from(u64_of(at))));
        }
        out.insert(
            key::MODEL_CHANGED.into(),
            owned(Value::from(st.model_changed.lock().unwrap().clone())),
        );
        // Counted from the index: with a decoder installed, they are retried
        // at the next start and the count would mislead until then.
        let no_decoder = match crate::vision::heif_convert() {
            None => index::undecodable(&db).map_err(failed)?,
            Some(_) => 0,
        };
        out.insert(key::NO_DECODER.into(), owned(Value::from(no_decoder)));
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

    /// The folders holding most of the index, biggest first, with their
    /// chunk counts: at most `limit`, none inside another (index::largest_folders).
    fn largest_folders(&self, limit: u32) -> zbus::fdo::Result<Vec<(String, u64)>> {
        let db = self.db.lock().unwrap();
        Ok(index::largest_folders(&db, limit as usize)
            .map_err(failed)?
            .into_iter()
            .map(|(p, n)| (p.to_string_lossy().into_owned(), n))
            .collect())
    }

    /// Compare every root with the index now, as the watcher does hourly,
    /// and index what differs. Waits for any indexing job to finish first.
    fn check_now(&self) -> zbus::fdo::Result<()> {
        self.status.check_now.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Stop indexing within a chunk and hold new indexing until
    /// ResumeIndexing. Files already indexed stay; queries still work.
    fn pause_indexing(&self) -> zbus::fdo::Result<()> {
        self.status.paused.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Run the indexing held since PauseIndexing. Unchanged files are
    /// skipped, so it carries on where it stopped.
    fn resume_indexing(&self) -> zbus::fdo::Result<()> {
        self.status.paused.store(false, Ordering::Relaxed);
        self.send(Job::Retry)
    }

    /// Indexed text files and documents, newest first by modification
    /// time, as (path, kind, unix seconds): at most `limit`, capped at
    /// recent::MAX. Files gone since they were indexed are left out.
    fn recent_files(&self, limit: u32) -> zbus::fdo::Result<Vec<(String, String, i64)>> {
        let db = self.db.lock().unwrap();
        let files = index::recent_files(&db, limit.min(recent::MAX) as usize, Path::exists)
            .map_err(failed)?;
        Ok(files
            .into_iter()
            .map(|(path, kind, mtime)| {
                let kind = match kind {
                    Kind::Document => recent::DOCUMENT,
                    _ => recent::TEXT,
                };
                (path, kind.to_owned(), mtime)
            })
            .collect())
    }

    /// The whole text of an indexed text file or document, for reading
    /// aloud. A document is parsed on a thread of its own, so the bus is not
    /// held while a big PDF is.
    async fn file_text(&self, path: String) -> zbus::fdo::Result<String> {
        let db = self.db.clone();
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(file_text(&db, &path));
        });
        rx.await
            .map_err(|_| failed("reading the file stopped"))?
            .map_err(|e| failed(format!("{e:#}")))
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
        let images = self.images_enabled();
        status
            .fetch_total
            .store(model::total_bytes(images), Ordering::Relaxed);
        std::thread::spawn(move || {
            let result = model::fetch(images, &mut |_, done, _| {
                status.fetch_done.store(done, Ordering::Relaxed);
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

/// Images further than this below the best text or document result are left
/// out. Their floor alone lets photos through for queries text answers well:
/// "bills" here: a document at 0.704 and a photo at 0.632, a gap of 0.072.
/// Not yet checked against a query where text and a photo are both right.
const IMAGE_GAP: f32 = 0.06;

/// Best chunk per file, files at or above their kind's floor, best RESULTS
/// of each kind: text, then documents, then images, less images too far
/// below the best of the others (IMAGE_GAP).
fn per_file(hits: Vec<Hit>, floors: Floors) -> Vec<Hit> {
    let mut seen = HashSet::new();
    let mut groups: [Vec<Hit>; 3] = Default::default();
    for h in hits {
        if h.score < floors.of(h.kind) || !seen.insert(h.path.clone()) {
            continue;
        }
        let group = &mut groups[match h.kind {
            Kind::Text => 0,
            Kind::Document => 1,
            Kind::Image => 2,
        }];
        if group.len() < RESULTS {
            group.push(h);
        }
    }
    let best = groups[..2]
        .iter()
        .flatten()
        .map(|h| h.score)
        .fold(f32::NEG_INFINITY, f32::max);
    groups[2].retain(|h| h.score >= best - IMAGE_GAP);
    groups.into_iter().flatten().collect()
}

pub fn serve(opts: Options) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    worker::prepare(&opts.status)?;
    let _watcher = watch::spawn(
        tx.clone(),
        opts.config.clone(),
        opts.file.clone(),
        opts.status.clone(),
    )?;
    let db: Db = Arc::new(Mutex::new(index::open(&index::default_db()?)?));
    let control = Control {
        jobs: Mutex::new(tx.clone()),
        status: opts.status.clone(),
        db: db.clone(),
        config: opts.config.clone(),
    };
    let config = opts.config.clone();
    let worker = std::thread::spawn(move || worker::run(rx, opts));
    let _conn = zbus::blocking::connection::Builder::session()?
        .name(SERVICE)?
        .serve_at(
            OBJECT,
            Runner {
                jobs: Mutex::new(tx.clone()),
                db: db.clone(),
                config: config.clone(),
            },
        )?
        .serve_at(
            GNOME,
            Gnome {
                jobs: Mutex::new(tx),
                db,
                config,
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

/// A place vinoSearch's results show (#45). Each has files of its own, written
/// and removed apart from the service's, so a Plasma session gets no GNOME
/// provider and a GNOME session no KRunner plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Place {
    /// KRunner (Alt+Space) and the Plasma app launcher's search, which reuses
    /// KRunner's plugins; they can't be split.
    Kde,
    /// GNOME Shell's overview search, from the next login.
    Gnome,
}

impl Place {
    pub const ALL: [Place; 2] = [Place::Kde, Place::Gnome];

    fn name(self) -> &'static str {
        match self {
            Place::Kde => "KRunner and the Plasma launcher",
            Place::Gnome => "GNOME Shell",
        }
    }
}

/// The places for the running desktop, from XDG_CURRENT_DESKTOP (`KDE`,
/// `ubuntu:GNOME`, ...). Both when it names neither (another desktop, a
/// TTY, ssh), as every install did before #45.
fn places_for(desktop: Option<&str>) -> Vec<Place> {
    let names: Vec<String> = desktop
        .unwrap_or_default()
        .split(':')
        .map(str::to_ascii_uppercase)
        .collect();
    let has = |n: &str| names.iter().any(|d| d == n);
    let places: Vec<Place> = [(Place::Kde, has("KDE")), (Place::Gnome, has("GNOME"))]
        .into_iter()
        .filter_map(|(p, on)| on.then_some(p))
        .collect();
    if places.is_empty() {
        Place::ALL.to_vec()
    } else {
        places
    }
}

pub fn current_places() -> Vec<Place> {
    places_for(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref())
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
        provider: data.join(GNOME_PROVIDER),
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
    // (NoDisplay), so this one shows in the app grid. It opens the settings
    // window when that is installed beside us, else the documentation.
    let settings = std::env::current_exe()?.with_file_name("vinosearch-settings");
    let exec = if settings.is_file() {
        let s = utf8(&settings)?;
        if s.chars().any(char::is_control) {
            anyhow::bail!("{s:?}: cannot go in a desktop entry");
        }
        desktop_quote(s)
    } else {
        "xdg-open https://github.com/karanshukla/vinoSearch#readme".to_owned()
    };
    fs::write(
        &g.desktop,
        format!(
            "[Desktop Entry]\nType=Application\nName=vinoSearch\n\
             Comment=Semantic file search on the NPU\nIcon={ICON}\nExec={exec}\n\
             StartupWMClass={SETTINGS_APP_ID}\nCategories=Utility;Settings;\nTerminal=false\n"
        ),
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

fn install_kde() -> Result<()> {
    let plugin = data_home()?.join(KRUNNER_PLUGIN);
    fs::create_dir_all(plugin.parent().unwrap())?;
    fs::write(
        &plugin,
        format!(
            "[Desktop Entry]\nName=vinoSearch\nComment=Semantic file search on the NPU\n\
             X-KDE-ServiceTypes=Plasma/Runner\nType=Service\nIcon={ICON}\n\
             X-KDE-PluginInfo-Name=vinosearch\nX-KDE-PluginInfo-EnabledByDefault=true\n\
             X-Plasma-API=DBus\nX-Plasma-DBusRunner-Service={SERVICE}\nX-Plasma-DBusRunner-Path={OBJECT}\n\
             X-Plasma-Request-Actions-Once=true\nX-Plasma-Runner-Min-Letter-Count=3\n\
             X-Plasma-Runner-Unique-Results=true\n"
        ),
    )?;
    println!("wrote {}", plugin.display());
    restart_krunner();
    Ok(())
}

/// KRunner reads its plugins at start, so it is restarted when it is
/// running: it comes back on the next Alt+Space, and the launcher's search
/// stops (or starts) asking us at once.
fn restart_krunner() {
    let unit = "plasma-krunner.service";
    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if active && systemctl(&["restart", unit]) {
        println!("restarted {unit}");
    }
}

/// Write `place`'s files. The service is installed too if it isn't yet,
/// with no flags: a plugin pointing at a bus name nothing activates would
/// search nothing.
pub fn install_place(place: Place) -> Result<()> {
    if !data_home()?.join(DBUS_SERVICE_FILE).exists() {
        install_service(None, None, None)?;
    }
    match place {
        Place::Kde => install_kde(),
        Place::Gnome => install_gnome(),
    }
}

/// Remove `place`'s files; the service stays for the other place, the CLI
/// and the settings app.
pub fn uninstall_place(place: Place) -> Result<()> {
    let files = match place {
        Place::Kde => vec![data_home()?.join(KRUNNER_PLUGIN)],
        Place::Gnome => {
            let g = gnome_files()?;
            vec![g.provider, g.desktop, g.environment]
        }
    };
    let mut removed = 0;
    for p in files {
        if p.exists() {
            fs::remove_file(&p)?;
            println!("removed {}", p.display());
            removed += 1;
        }
    }
    if removed == 0 {
        println!("{} was already off", place.name());
    }
    if place == Place::Kde {
        restart_krunner();
    }
    Ok(())
}

/// Turn one place on or off (`vinosearch integrate kde|gnome [--off]`).
pub fn integrate(place: Place, on: bool) -> Result<()> {
    if on {
        install_place(place)?;
    } else {
        uninstall_place(place)?;
    }
    println!("{}: {}", place.name(), if on { "on" } else { "off" });
    Ok(())
}

fn config_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".config"),
    })
}

/// The vinoPilot landing page's vinoSearch mark, with the viewBox cropped to
/// a 2 px margin: the page's 4 px left the tile visibly smaller than other
/// apps' icons in a task manager.
const ICON_SVG: &str = include_str!("../data/dev.vinosearch.svg");

fn icon_path() -> Result<PathBuf> {
    Ok(data_home()?.join(format!("icons/hicolor/scalable/apps/{ICON}.svg")))
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

/// Install the service and the search places for the running desktop (both
/// when that is neither Plasma nor GNOME); `integrate` adds or removes one.
pub fn install(
    openvino_dir: Option<&Path>,
    device: Option<&str>,
    idle_unload_minutes: Option<f64>,
) -> Result<()> {
    install_service(openvino_dir, device, idle_unload_minutes)?;
    for place in current_places() {
        match place {
            Place::Kde => install_kde()?,
            Place::Gnome => install_gnome()?,
        }
    }
    Ok(())
}

/// The icon, the D-Bus activation file and the systemd user service that
/// starts at login, so the watcher keeps the index current from then on
/// (D-Bus activation alone would start it only at the first query). Only
/// flags given here are baked into the service; the rest come from the
/// config file each time the runner starts.
fn install_service(
    openvino_dir: Option<&Path>,
    device: Option<&str>,
    idle_unload_minutes: Option<f64>,
) -> Result<()> {
    let mut args = vec![utf8(&std::env::current_exe()?)?.to_owned()];
    if let Some(d) = device {
        args.extend(["--device".into(), d.to_owned()]);
    }
    if let Some(m) = idle_unload_minutes {
        args.extend(["--idle-unload-minutes".into(), m.to_string()]);
    }
    if let Some(d) = openvino_dir {
        args.extend(["--openvino-dir".into(), utf8(d)?.to_owned()]);
    }
    args.push("krunner".into());
    for a in &args {
        if a.chars().any(char::is_control) {
            anyhow::bail!("{a:?}: cannot go in a service's command line");
        }
    }
    let data = data_home()?;
    let service = data.join(DBUS_SERVICE_FILE);
    let icon = icon_path()?;
    for p in [&service, &icon] {
        fs::create_dir_all(p.parent().unwrap())?;
    }
    fs::write(&icon, ICON_SVG)?;
    fs::write(
        &service,
        format!(
            "[D-BUS Service]\nName={SERVICE}\nExec={}\nSystemdService={UNIT}\n",
            command_line(&args, dbus_quote)
        ),
    )?;
    let unit = unit_path()?;
    fs::create_dir_all(unit.parent().unwrap())?;
    fs::write(
        &unit,
        format!(
            "[Unit]\nDescription=vinoSearch semantic file search (KRunner runner and index watcher)\n\
             Documentation=https://github.com/karanshukla/vinoSearch\n\n\
             [Service]\nType=dbus\nBusName={SERVICE}\nExecStart={}\n\
             Restart=on-failure\nRestartSec=10\n\n\
             [Install]\nWantedBy=default.target\n",
            command_line(&args, systemd_quote)
        ),
    )?;
    println!(
        "wrote {}\nwrote {}\nwrote {}",
        service.display(),
        unit.display(),
        icon.display()
    );
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

fn utf8(p: &Path) -> Result<&str> {
    p.to_str()
        .with_context(|| format!("{}: not UTF-8", p.display()))
}

/// `args` as one command line, each quoted by `quote`, so a path with a
/// space stays one argument.
fn command_line(args: &[String], quote: fn(&str) -> String) -> String {
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// A double-quoted word in a D-Bus service file's Exec (dbus-daemon's and
/// dbus-broker's shell-like parsing: inside double quotes a backslash
/// escapes `\`, `"`, `$` and `` ` ``).
fn dbus_quote(arg: &str) -> String {
    let mut q = String::from('"');
    for c in arg.chars() {
        if matches!(c, '\\' | '"' | '$' | '`') {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q
}

/// A double-quoted word in a systemd ExecStart: C-style backslash escapes,
/// `%%` for a literal `%` (specifiers) and `$$` for `$` (variables).
fn systemd_quote(arg: &str) -> String {
    let mut q = String::from('"');
    for c in arg.chars() {
        match c {
            '\\' | '"' => {
                q.push('\\');
                q.push(c);
            }
            '%' => q.push_str("%%"),
            '$' => q.push_str("$$"),
            _ => q.push(c),
        }
    }
    q.push('"');
    q
}

/// A quoted argument in a desktop entry's Exec: the D-Bus quoting, which
/// matches the Exec key's, then `%%` for `%` (field codes) and every
/// backslash doubled again for the string value's own escaping.
fn desktop_quote(arg: &str) -> String {
    dbus_quote(arg).replace('\\', "\\\\").replace('%', "%%")
}

/// Everything off: both places, the service, the icon.
pub fn uninstall() -> Result<()> {
    let data = data_home()?;
    let unit = unit_path()?;
    if unit.exists() {
        systemctl(&["disable", "--now", UNIT]);
    }
    for place in Place::ALL {
        uninstall_place(place)?;
    }
    for p in [data.join(DBUS_SERVICE_FILE), unit, icon_path()?] {
        if p.exists() {
            fs::remove_file(&p)?;
            println!("removed {}", p.display());
        }
    }
    systemctl(&["daemon-reload"]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: Kind, score: f32, path: &str) -> Hit {
        Hit {
            kind,
            score,
            path: path.to_owned(),
            line: 1,
            text: String::new(),
            copies: Vec::new(),
        }
    }

    const FLOORS: Floors = Floors {
        text: 0.65,
        image: 0.63,
    };

    fn paths(hits: Vec<Hit>) -> Vec<String> {
        per_file(hits, FLOORS).into_iter().map(|h| h.path).collect()
    }

    #[test]
    fn images_far_below_a_strong_text_hit_are_dropped() {
        let hits = vec![
            hit(Kind::Document, 0.704, "/statement.csv"),
            hit(Kind::Image, 0.632, "/cat.png"),
        ];
        assert_eq!(paths(hits), ["/statement.csv"]);
    }

    #[test]
    fn images_close_to_the_best_text_hit_stay() {
        let hits = vec![
            hit(Kind::Text, 0.74, "/wedding-plan.md"),
            hit(Kind::Image, 0.70, "/wedding.jpg"),
            hit(Kind::Image, 0.64, "/blurry.jpg"),
        ];
        assert_eq!(paths(hits), ["/wedding-plan.md", "/wedding.jpg"]);
    }

    #[test]
    fn images_alone_need_only_their_floor() {
        let hits = vec![
            hit(Kind::Text, 0.60, "/below-floor.md"),
            hit(Kind::Image, 0.64, "/dog.jpg"),
        ];
        assert_eq!(paths(hits), ["/dog.jpg"]);
    }

    #[test]
    fn the_running_desktop_picks_the_places() {
        assert_eq!(places_for(Some("KDE")), [Place::Kde]);
        assert_eq!(places_for(Some("ubuntu:GNOME")), [Place::Gnome]);
        assert_eq!(places_for(Some("GNOME-Classic:GNOME")), [Place::Gnome]);
        assert_eq!(places_for(Some("XFCE")), Place::ALL);
        assert_eq!(places_for(None), Place::ALL);
        assert_eq!(places_for(Some("")), Place::ALL);
    }

    #[test]
    fn exec_lines_quote_every_argument() {
        let args = ["/opt/my apps/vinosearch".to_owned(), "krunner".to_owned()];
        assert_eq!(
            command_line(&args, dbus_quote),
            r#""/opt/my apps/vinosearch" "krunner""#
        );
        assert_eq!(
            command_line(&args, systemd_quote),
            r#""/opt/my apps/vinosearch" "krunner""#
        );
        let odd = r#"/a "b" \c $d `e` 100%"#;
        assert_eq!(dbus_quote(odd), r#""/a \"b\" \\c \$d \`e\` 100%""#);
        assert_eq!(systemd_quote(odd), r#""/a \"b\" \\c $$d `e` 100%%""#);
        assert_eq!(
            desktop_quote(odd),
            r#""/a \\"b\\" \\\\c \\$d \\`e\\` 100%%""#
        );
    }

    #[test]
    fn file_text_is_for_indexed_text_and_documents_only() {
        let dir = std::env::temp_dir().join(format!("vinosearch-text-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let conn = index::open(&dir.join("i.sqlite")).unwrap();
        let note = dir.join("note.md");
        let photo = dir.join("photo.jpg");
        fs::write(&note, "# Hello\n").unwrap();
        fs::write(&photo, b"\xff\xd8").unwrap();
        for p in [&note, &photo] {
            conn.execute(
                "INSERT INTO files (path, mtime, size) VALUES (?1, 0, 0)",
                [p.to_str().unwrap()],
            )
            .unwrap();
        }
        let db: Db = Arc::new(Mutex::new(conn));
        assert_eq!(file_text(&db, note.to_str().unwrap()).unwrap(), "# Hello\n");
        let e = file_text(&db, photo.to_str().unwrap()).unwrap_err();
        assert!(
            e.to_string().ends_with("is not a text file or document"),
            "{e}"
        );
        let e = file_text(&db, "/etc/passwd").unwrap_err();
        assert!(e.to_string().ends_with("is not an indexed file"), "{e}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_urls_are_percent_encoded() {
        assert_eq!(file_url("/home/u/a.md"), "file:///home/u/a.md");
        assert_eq!(
            file_url("/home/u/My Notes/#1 50%?.md"),
            "file:///home/u/My%20Notes/%231%2050%25%3F.md"
        );
        assert_eq!(file_url("/tmp/é.txt"), "file:///tmp/%C3%A9.txt");
    }
}
