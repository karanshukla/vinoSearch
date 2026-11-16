//! The runner on the session bus: KRunner's org.kde.krunner1 at /runner,
//! GNOME Shell's org.gnome.Shell.SearchProvider2 at /gnome, and
//! dev.vinosearch.Control1 at /control for the settings app. Queries go to the
//! worker thread, which owns the model; the watcher keeps the index current.
//!
//! Handlers never block the bus: Match awaits the worker's reply, and Control
//! reads shared status and its own database connection, so a status query
//! answers while the worker is busy indexing.

use crate::config::Config;
use crate::extract::session::{self, Part};
use crate::index::{self, Hit, Kind};
use crate::ml::model;
use crate::search::open::At;
use crate::service::start;
use crate::service::watch;
use crate::service::worker::{self, Job, Options, Request, Status};
use anyhow::{Context, Result, bail};
use futures_channel::oneshot;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use vinosearch_shared::sandbox;
use zbus::interface;
use zbus::{OwnedVariant, Variant};

use vinosearch_shared::dbus::{
    CONTROL_PATH as CONTROL, GNOME_PATH as GNOME, RUNNER_PATH as OBJECT, names, recent, results,
    status as key,
};
/// Files of each kind shown.
pub(crate) const RESULTS: usize = 5;
/// QueryMatch::CategoryRelevance::Moderate
const RELEVANCE_MODERATE: i32 = 50;

type Match = (
    String,
    String,
    String,
    i32,
    f64,
    HashMap<String, OwnedVariant>,
);

/// One connection for the handlers that read the index; queries are short.
/// Closed and opened again under it when encryption changes (#4).
type Db = Arc<crate::index::encryption::Live>;

struct Runner {
    jobs: Mutex<mpsc::Sender<Job>>,
    db: Db,
    config: PathBuf,
    places: Places,
    /// The last search's query, for Show all results.
    query: Mutex<String>,
}

/// How close a result must be, per kind (`formats.*.min_score`).
#[derive(Clone, Copy)]
pub(crate) struct Floors {
    text: f32,
    image: f32,
    code: f32,
    media: f32,
}

impl Floors {
    /// From the config file as it is now, so a change in the settings window
    /// shows from the next keystroke; the defaults if it doesn't parse.
    fn load(config: &Path) -> Self {
        Self::of_formats(&Config::load(config).map(|c| c.formats).unwrap_or_default())
    }

    /// `formats`' floors, the defaults where they are unset or out of range.
    pub(crate) fn of_formats(formats: &crate::config::Formats) -> Self {
        Self {
            text: formats
                .text_min_score()
                .unwrap_or(crate::config::DEFAULT_TEXT_MIN_SCORE),
            image: formats
                .image_min_score()
                .unwrap_or(crate::config::DEFAULT_IMAGE_MIN_SCORE),
            code: formats
                .code_min_score()
                .unwrap_or(crate::config::DEFAULT_CODE_MIN_SCORE),
            media: formats
                .media_min_score()
                .unwrap_or(crate::config::DEFAULT_MEDIA_MIN_SCORE),
        }
    }

    /// `h`'s floor: its kind's, or the image floor for text read in an
    /// image (results::picture).
    pub(crate) fn of(&self, h: &Hit) -> f32 {
        match h.kind {
            _ if crate::search::results::picture(h) => self.image,
            Kind::Text | Kind::Document | Kind::Session => self.text,
            Kind::Image => self.image,
            Kind::Code => self.code,
            Kind::Media => self.media,
        }
    }
}

/// The best chunk of each of the best files for `query`, text first, then
/// documents, then images, as KRunner and GNOME Shell both show them; code
/// alone for a query starting `code:` (#110). Empty if the worker has
/// stopped.
async fn search(jobs: &Mutex<mpsc::Sender<Job>>, floors: Floors, query: String) -> Vec<Hit> {
    per_file(chunks(jobs, query, RESULTS * 8).await, floors)
}

/// The best `k` chunks of each kind for `query`; empty if the worker has stopped.
async fn chunks(jobs: &Mutex<mpsc::Sender<Job>>, query: String, k: usize) -> Vec<Hit> {
    // KRunner counts `code:` towards its three letters.
    if index::scope(&query).0.chars().count() < MIN_QUERY_CHARS {
        return Vec::new();
    }
    let (tx, rx) = oneshot::channel();
    let job = Job::Match {
        query,
        k,
        reply: tx,
    };
    if jobs.lock().unwrap().send(job).is_err() {
        return Vec::new();
    }
    rx.await.unwrap_or_default()
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
    let others: Vec<String> = [
        match h.copies.len() {
            0 => None,
            1 => Some("+1 copy".to_owned()),
            n => Some(format!("+{n} copies")),
        },
        (!h.similar.is_empty()).then(|| format!("+{} similar", h.similar.len())),
    ]
    .into_iter()
    .flatten()
    .collect();
    let copies = match others.as_slice() {
        [] => String::new(),
        o => format!("  ({})", o.join(", ")),
    };
    let description = match h.kind {
        Kind::Text | Kind::Code => {
            format!("{dir}:{}{copies}  {}", h.line, crate::snippet(&h.text))
        }
        Kind::Image => format!("{dir}{copies}"),
        Kind::Document if read_in_an_image(h) => {
            format!("{dir}{copies}  {}", crate::snippet(&h.text))
        }
        Kind::Document => format!(
            "{dir}, {} {}{copies}  {}",
            crate::extract::unit(path),
            h.line,
            crate::snippet(&h.text)
        ),
        Kind::Session => format!(
            "{}{copies}  {}",
            session_place(h).unwrap_or(dir),
            crate::snippet(&h.text)
        ),
        Kind::Media => {
            let (track, second) = crate::extract::media::Track::of(h.line);
            let what = match track {
                crate::extract::media::Track::Sound => "heard",
                crate::extract::media::Track::Picture => "seen",
            };
            let at = crate::extract::media::stamp(second);
            format!("{dir}, {what} at {at}{copies}")
        }
    };
    (name, description)
}

/// Which part of a session matched, and when it was (#143): "Transcript at
/// 01:12 · 2026-10-27 14:05, 43 min", "Summary, AI-generated · ...". None
/// for a bundle that no longer reads.
fn session_place(h: &Hit) -> Option<String> {
    let bytes = session::load(Path::new(&h.path)).ok()?;
    let about = session::about(&bytes).ok()?;
    let part = match Part::of(h.line) {
        (Part::Transcript, line) => {
            let text = session::text(&bytes, Part::Transcript).ok()?;
            match session::stamp_at(&text, usize::try_from(line).unwrap_or(0)) {
                Some(stamp) => format!("Transcript at {stamp}"),
                None => "Transcript".to_owned(),
            }
        }
        (Part::Summary, _) if about.ai_summary => "Summary, AI-generated".to_owned(),
        (Part::Summary, _) => "Summary".to_owned(),
    };
    Some(format!("{part} · {}", about.when()))
}

/// The actions a match offers: the other part of a session (#143) as well.
fn match_actions(h: &Hit, plays: bool) -> Vec<&'static str> {
    let mut out = vec![OPEN_FOLDER, SHOW_ALL];
    if h.kind == Kind::Session {
        let other = match Part::of(h.line).0 {
            Part::Summary => Some(OPEN_TRANSCRIPT),
            Part::Transcript => session::load(Path::new(&h.path))
                .and_then(|b| session::about(&b))
                .is_ok_and(|a| a.has_summary)
                .then_some(OPEN_SUMMARY),
        };
        out.splice(0..0, other);
        // The default action plays the recording, so the transcript is offered.
        if plays {
            out.insert(0, OPEN_TRANSCRIPT);
        }
    }
    out
}

/// A recording, and the second in it a transcript hit is at.
type Play = (PathBuf, u32);

/// The indexed recording the session `id` was transcribed from (#188), by
/// the SHA-256 its attestation gives. None for a live session, or while the
/// recording isn't indexed (media is off, or the file is outside the roots).
fn session_recording(db: &Db, id: &str) -> Option<PathBuf> {
    let bytes = session::load(Path::new(id)).ok()?;
    let sha = session::about(&bytes).ok()?.source_sha256?;
    let file = db
        .lock()
        .and_then(|c| index::media_with_hash(&c, &sha))
        .ok()??;
    let file = PathBuf::from(file);
    file.is_file().then_some(file)
}

/// What a hit at `line` of the session `id`'s transcript plays: its
/// recording, from the stamp of the paragraph that matched.
fn recording_at(db: &Db, id: &str, line: u32) -> Option<Play> {
    let bytes = session::load(Path::new(id)).ok()?;
    let text = session::text(&bytes, Part::Transcript).ok()?;
    let stamp = session::stamp_at(&text, line as usize)?;
    Some((session_recording(db, id)?, session::seconds(&stamp)?))
}

/// What `h` plays, if it is a transcript hit with a recording to play.
fn recording(db: &Db, h: &Hit) -> Option<Play> {
    match at(h) {
        Some(At::Session(Part::Transcript, line)) => recording_at(db, &h.path, line),
        _ => None,
    }
}

fn owned(v: Variant<'_>) -> OwnedVariant {
    v.try_into().expect("string values have no fds")
}

/// Text OCR read in an image: scored as a document, but with no page or
/// slide to name, and listed with the images.
fn read_in_an_image(h: &Hit) -> bool {
    h.kind == Kind::Document && !crate::extract::is_document(Path::new(&h.path))
}

fn to_match(h: Hit, floors: Floors, play: Option<&Play>) -> Match {
    let (name, subtext) = describe(&h);
    let mut props = HashMap::new();
    props.insert("subtext".into(), owned(Variant::from(subtext)));
    props.insert("urls".into(), owned(Variant::from(vec![file_url(&h.path)])));
    // Named so each group reads as ours among KRunner's.
    let category = match h.kind {
        Kind::Document if read_in_an_image(&h) => "vinoSearch Images",
        Kind::Text => "vinoSearch Text",
        Kind::Document => "vinoSearch Documents",
        Kind::Image => "vinoSearch Images",
        Kind::Code => "vinoSearch Code",
        Kind::Session => "vinoWhisper sessions",
        Kind::Media => "vinoSearch Media",
    };
    props.insert("category".into(), owned(Variant::from(category)));
    // Without it KRunner offers every action on every match.
    props.insert(
        "actions".into(),
        owned(Variant::from(match_actions(&h, play.is_some()))),
    );
    // For vinosearch menu, which opens the pick itself; KRunner ignores keys
    // it doesn't know. A summary opens at its top.
    match at(&h) {
        Some(At::Line(l)) => drop(props.insert("line".into(), owned(Variant::from(l)))),
        Some(At::Page(p)) => drop(props.insert("page".into(), owned(Variant::from(p)))),
        Some(At::Session(Part::Transcript, l)) => {
            drop(props.insert("line".into(), owned(Variant::from(l))))
        }
        Some(At::Second(s)) => drop(props.insert("second".into(), owned(Variant::from(s)))),
        Some(At::Session(Part::Summary, _)) | None => {}
    }
    if let Some((file, second)) = play {
        let file = file.display().to_string();
        props.insert("play".into(), owned(Variant::from(file)));
        props.insert("second".into(), owned(Variant::from(*second)));
    }
    let (icon, _) = crate::search::icons::for_path(Path::new(&h.path), h.kind);
    let relevance = relevance(&h, floors);
    (
        h.path,
        name,
        icon.into(),
        RELEVANCE_MODERATE,
        relevance,
        props,
    )
}

/// How far above its kind's floor `h` scores, 0 to 1: what KRunner orders
/// matches by.
pub(crate) fn relevance(h: &Hit, floors: Floors) -> f64 {
    let floor = floors.of(h);
    f64::from(((h.score - floor) / (1.0 - floor)).clamp(0.0, 1.0))
}

/// `path` as a file:// URL, percent-encoded so `#`, `%`, `?` and spaces
/// survive (RFC 8089; `/` stays as the separator).
fn file_url(path: &str) -> String {
    format!("file://{}", utf8_percent_encode(path, FILE_URL_ESCAPED))
}

/// Everything but RFC 3986's unreserved characters and the `/` separator.
const FILE_URL_ESCAPED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Whether `id` is an indexed file. Ids come from callers on the session
/// bus, so anything this runner could not have returned is refused.
fn indexed(db: &Db, id: &str, doing: &str) -> bool {
    match db.lock().and_then(|c| index::is_indexed(&c, id)) {
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

/// Open the indexed file `id` at `at`, or its folder, on a thread of its
/// own: finding the default app runs `xdg-mime`.
fn open(db: &Db, config: &Path, id: &str, at: Option<At>, folder: bool) {
    if !indexed(db, id, "opening") {
        return;
    }
    let path = PathBuf::from(id);
    if folder {
        std::thread::spawn(move || crate::search::open::xdg_open(path.parent().unwrap_or(&path)));
        return;
    }
    let (editor, player) = Config::load(config)
        .map(|c| (c.editor, c.player))
        .unwrap_or_default();
    std::thread::spawn(move || {
        crate::search::open::open(&path, at, editor.as_deref(), player.as_deref());
    });
}

/// The default action on the indexed file `id` at `at`: a transcript hit
/// whose recording is indexed plays the recording from its stamp (#188);
/// anything else opens as `open` does.
fn open_or_play(db: &Db, config: &Path, id: &str, at: Option<At>) {
    if let Some(At::Session(Part::Transcript, line)) = at
        && indexed(db, id, "opening")
        && let Some((file, second)) = recording_at(db, id, line)
    {
        let file = file.display().to_string();
        return open(db, config, &file, Some(At::Second(second)), false);
    }
    open(db, config, id, at, false);
}

/// The last search's places, by result id, for opening a result there.
type Places = Mutex<HashMap<String, Option<At>>>;

fn remember(places: &Places, hits: &[Hit]) {
    *places.lock().unwrap() = hits.iter().map(|h| (h.path.clone(), at(h))).collect();
}

fn at(h: &Hit) -> Option<At> {
    At::of(h.kind, Path::new(&h.path), h.line)
}

/// The text of the indexed file `id`, for Control1.FileText: a text file's
/// contents (an HTML page's without its tags), a document's text from the
/// extractor child (seconds for a big PDF), or a session's transcript. Err for anything else, an image
/// included.
fn file_text(db: &Db, id: &str) -> Result<String> {
    if !index::is_indexed(&*db.lock()?, id)? {
        bail!("{id} is not an indexed file");
    }
    let path = Path::new(id);
    match index::readable_kind(path) {
        Some(Kind::Text | Kind::Code) => {
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
        // The record; the summary is a model's account of it.
        Some(Kind::Session) => session::load(path)
            .and_then(|b| session::text(&b, Part::Transcript))
            .with_context(|| format!("reading {id}")),
        _ => bail!("{id} is not a text file or document"),
    }
}

#[interface(name = "org.kde.krunner1")]
impl Runner {
    #[zbus(name = "Actions")]
    fn actions(&self) -> Vec<(String, String, String)> {
        vec![
            (
                OPEN_SUMMARY.into(),
                "Open summary".into(),
                "view-list-text".into(),
            ),
            (
                OPEN_TRANSCRIPT.into(),
                "Open transcript".into(),
                "text-x-generic".into(),
            ),
            (
                OPEN_FOLDER.into(),
                "Open containing folder".into(),
                "document-open-folder".into(),
            ),
            (
                SHOW_ALL.into(),
                "Show all results".into(),
                "view-list-icons".into(),
            ),
        ]
    }

    #[zbus(name = "Match")]
    async fn find(&self, query: String) -> Vec<Match> {
        let floors = Floors::load(&self.config);
        query.clone_into(&mut self.query.lock().unwrap());
        let hits = search(&self.jobs, floors, query).await;
        remember(&self.places, &hits);
        hits.into_iter()
            .map(|h| {
                let play = recording(&self.db, &h);
                to_match(h, floors, play.as_ref())
            })
            .collect()
    }

    /// At the match's line or page when it is from the last search; at the
    /// top otherwise.
    #[zbus(name = "Run")]
    fn run(&self, match_id: String, action_id: String) {
        if action_id == SHOW_ALL {
            let query = self.query.lock().unwrap().clone();
            launch_settings(&["--search", &query]);
            return;
        }
        let place = self
            .places
            .lock()
            .unwrap()
            .get(&match_id)
            .copied()
            .flatten();
        match action_id.as_str() {
            OPEN_SUMMARY => {
                let at = Some(At::Session(Part::Summary, 0));
                open(&self.db, &self.config, &match_id, at, false);
            }
            OPEN_TRANSCRIPT => {
                let at = match place {
                    Some(at @ At::Session(Part::Transcript, _)) => at,
                    _ => At::Session(Part::Transcript, 0),
                };
                open(&self.db, &self.config, &match_id, Some(at), false);
            }
            OPEN_FOLDER => open(&self.db, &self.config, &match_id, place, true),
            _ => open_or_play(&self.db, &self.config, &match_id, place),
        }
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
        if index::scope(&query).0.trim().chars().count() < MIN_QUERY_CHARS {
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
    fn metas(&self, ids: Vec<String>) -> Vec<HashMap<String, OwnedVariant>> {
        let last = self.last.lock().unwrap();
        ids.into_iter()
            .map(|id| {
                let (name, description, kind) = match last.get(&id) {
                    Some(h) => {
                        let (name, description) = describe(h);
                        (name, description, h.kind)
                    }
                    None => (
                        Path::new(&id)
                            .file_name()
                            .map_or(id.clone(), |n| n.to_string_lossy().into_owned()),
                        id.clone(),
                        Kind::Text,
                    ),
                };
                let mut meta = HashMap::new();
                meta.insert("name".into(), owned(Variant::from(name)));
                meta.insert("description".into(), owned(Variant::from(description)));
                let (specific, generic) = crate::search::icons::for_path(Path::new(&id), kind);
                let gicon = format!(". GThemedIcon {specific} {generic}");
                meta.insert("gicon".into(), owned(Variant::from(gicon)));
                meta.insert("id".into(), owned(Variant::from(id)));
                meta
            })
            .collect()
    }

    #[zbus(name = "ActivateResult")]
    fn activate(&self, id: String, _terms: Vec<String>, _timestamp: u32) {
        let at = self.last.lock().unwrap().get(&id).and_then(at);
        open_or_play(&self.db, &self.config, &id, at);
    }

    /// Clicking the provider's icon: the search window with the query
    /// (#111), when vinosearch-settings is installed beside us.
    #[zbus(name = "LaunchSearch")]
    fn launch(&self, terms: Vec<String>, _timestamp: u32) {
        launch_settings(&["--search", &terms.join(" ")]);
    }
}

/// KRunner's action for opening the query in the search window.
const SHOW_ALL: &str = "show-all";
const OPEN_FOLDER: &str = "open-folder";
/// A session's other part (#143).
const OPEN_SUMMARY: &str = "open-summary";
const OPEN_TRANSCRIPT: &str = "open-transcript";

/// Start vinosearch-settings with `args` on a thread of its own, when it is
/// installed beside us.
fn launch_settings(args: &[&str]) {
    let settings = std::env::current_exe().map(|e| e.with_file_name("vinosearch-settings"));
    match settings {
        Ok(s) if s.is_file() => {
            let program = s.display().to_string();
            let argv = settings_argv(sandbox::flatpak_id(), &program, args);
            std::thread::spawn(move || {
                if let Err(e) = crate::search::open::spawn(argv) {
                    eprintln!("{program}: {e}");
                }
            });
        }
        _ => eprintln!("vinosearch-settings is not installed beside vinosearch"),
    }
}

/// The command that opens vinosearch-settings with `args`. In the Flatpak
/// `open::spawn` runs commands on the host, so there it is the host's
/// `flatpak run`, as the app menu's entry is: a sandbox of its own, which
/// one started in the runner's wouldn't outlive. Natively, `program`.
fn settings_argv(flatpak: Option<&str>, program: &str, args: &[&str]) -> Vec<String> {
    let command: Vec<&str> = match flatpak {
        Some(app) => vec!["flatpak", "run", "--command=vinosearch-settings", app],
        None => vec![program],
    };
    command
        .into_iter()
        .chain(args.iter().copied())
        .map(String::from)
        .collect()
}

struct Control {
    jobs: Mutex<mpsc::Sender<Job>>,
    status: Arc<Status>,
    db: Db,
    config: PathBuf,
    /// Taken by Quit, which `serve` waits on beside the worker.
    quit: Mutex<Option<oneshot::Sender<()>>>,
    /// What Status last counted from the index, and the index files' stamp
    /// it was counted at.
    counted: Mutex<Option<(Stamp, Counted)>>,
}

/// What Status counts from the index. Each is a scan of it, and with an
/// encrypted index that is all of it read and decrypted: 53 MB and a
/// quarter of a second for 11,600 chunks, once a second while the settings
/// window is open, on the thread that answers searches.
#[derive(Clone, Default)]
struct Counted {
    files: i64,
    chunks: i64,
    by_kind: [(i64, i64); 6],
    unread_sessions: i64,
    undecodable: i64,
    roots: Vec<PathBuf>,
    partial: Vec<(String, i64, i64)>,
    media_partial: i64,
}

/// Length and modification time of the index and its write-ahead log: any
/// commit, by this process or `vinosearch index`, changes one of them.
type Stamp = [Option<(u64, std::time::SystemTime)>; 2];

fn stamp(db: &Path) -> Stamp {
    ["", "-wal"].map(|ext| {
        let meta = fs::metadata(format!("{}{ext}", db.display())).ok()?;
        Some((meta.len(), meta.modified().ok()?))
    })
}

impl Counted {
    fn of(conn: &rusqlite::Connection) -> anyhow::Result<Self> {
        let (files, chunks) = index::counts(conn)?;
        Ok(Counted {
            files,
            chunks,
            by_kind: index::counts_by_kind(conn)?,
            unread_sessions: index::unread_sessions(conn)?,
            // With a decoder installed they are retried at the next start.
            undecodable: match crate::ml::vision::heif_convert() {
                None => index::undecodable(conn)?,
                Some(_) => 0,
            },
            roots: index::roots(conn)?,
            partial: index::partial(conn, key::PARTIAL_MAX)?,
            media_partial: index::media_partial(conn)?,
        })
    }
}

fn failed(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

impl Control {
    /// The index's counts: counted again only when the index has changed
    /// since the last time. Nothing while its key can't be had (`db` None).
    fn counted(&self, db: Option<&rusqlite::Connection>) -> zbus::fdo::Result<Counted> {
        let Some(conn) = db else {
            return Ok(Counted::default());
        };
        let now = stamp(&index::default_db().map_err(failed)?);
        let mut kept = self.counted.lock().unwrap();
        match &*kept {
            Some((at, counted)) if *at == now => Ok(counted.clone()),
            _ => {
                let counted = Counted::of(conn).map_err(failed)?;
                *kept = Some((now, counted.clone()));
                Ok(counted)
            }
        }
    }

    /// What the formats on in the config file as it is now need; nothing
    /// extra if it doesn't parse.
    fn model_wants(&self) -> model::Wants {
        Config::load(&self.config)
            .map(|c| model::Wants::of(&c.formats))
            .unwrap_or_default()
    }

    fn send(&self, job: Job) -> zbus::fdo::Result<()> {
        self.jobs
            .lock()
            .unwrap()
            .send(job)
            .map_err(|_| failed("the worker has stopped"))
    }

    /// For the worker's next step.
    fn request(&self, r: Request) -> zbus::fdo::Result<()> {
        self.status.pending.request(r);
        self.send(Job::Wake)
    }

    fn is_indexed(&self, path: &str) -> zbus::fdo::Result<()> {
        match self.db.lock().and_then(|c| index::is_indexed(&c, path)) {
            Ok(true) => Ok(()),
            Ok(false) => Err(failed(format!("{path} is not an indexed file"))),
            Err(e) => Err(failed(format!("{e:#}"))),
        }
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
    fn status(&self) -> zbus::fdo::Result<HashMap<String, OwnedVariant>> {
        // Answered while the key can't be had yet, counts empty.
        let db = match self.db.lock() {
            Ok(db) => Some(db),
            Err(e) if crate::index::seal::is_unavailable(&e) => None,
            Err(e) => return Err(failed(e)),
        };
        let db = db.as_deref();
        let counted = self.counted(db)?;
        let (files, chunks) = (counted.files, counted.chunks);
        let roots: Vec<String> = counted
            .roots
            .iter()
            .map(|r| r.display().to_string())
            .collect();
        let mut out = HashMap::new();
        out.insert(
            key::MODEL_LOADED.into(),
            owned(Variant::from(self.status.loaded.load(Ordering::Relaxed))),
        );
        out.insert(
            key::INDEXING.into(),
            owned(Variant::from(self.status.indexing())),
        );
        out.insert(
            key::PAUSED.into(),
            owned(Variant::from(self.status.paused.load(Ordering::Relaxed))),
        );
        out.insert(
            key::POWER_HOLD.into(),
            owned(Variant::from(
                self.status
                    .power
                    .hold()
                    .map_or("", crate::service::power::Hold::key),
            )),
        );
        out.insert(
            key::DEVICE.into(),
            owned(Variant::from(self.status.device.lock().unwrap().clone())),
        );
        out.insert(key::FILES.into(), owned(Variant::from(files)));
        out.insert(key::CHUNKS.into(), owned(Variant::from(chunks)));
        let [text, documents, images, code, sessions, media] = counted.by_kind;
        for (k, n) in [
            (key::TEXT_FILES, text.0),
            (key::TEXT_CHUNKS, text.1),
            (key::DOCUMENT_FILES, documents.0),
            (key::DOCUMENT_CHUNKS, documents.1),
            (key::IMAGE_FILES, images.0),
            (key::CODE_FILES, code.0),
            (key::CODE_CHUNKS, code.1),
            (key::SESSION_FILES, sessions.0),
            (key::SESSION_CHUNKS, sessions.1),
            (key::MEDIA_FILES, media.0),
            (key::MEDIA_CHUNKS, media.1),
            (key::SESSIONS_UNREAD, counted.unread_sessions),
        ] {
            out.insert(k.into(), owned(Variant::from(n)));
        }
        out.insert(key::ROOTS.into(), owned(Variant::from(roots)));
        let st = &self.status;
        let u64_of = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        out.insert(
            key::INDEXED_FILES.into(),
            owned(Variant::from(u64_of(&st.indexed_files))),
        );
        out.insert(
            key::FETCHING.into(),
            owned(Variant::from(st.fetching.load(Ordering::Relaxed))),
        );
        out.insert(
            key::FETCH_DONE.into(),
            owned(Variant::from(u64_of(&st.fetch_done))),
        );
        out.insert(
            key::FETCH_TOTAL.into(),
            owned(Variant::from(u64_of(&st.fetch_total))),
        );
        out.insert(
            key::FETCH_ERROR.into(),
            owned(Variant::from(st.fetch_error.lock().unwrap().clone())),
        );
        let model_dir = model::default_dir().map_err(failed)?;
        let vision_dir = model::vision_dir().map_err(failed)?;
        let ocr_dir = model::ocr_dir().map_err(failed)?;
        let wants = self.model_wants();
        let images = model::images_present(&model_dir, &vision_dir, &ocr_dir);
        let media = model::vision_present(&model_dir, &vision_dir)
            && model::audio_present(&model_dir, &model::audio_dir().map_err(failed)?);
        let present =
            model::present(&model_dir) && (!wants.images || images) && (!wants.media || media);
        out.insert(key::MODEL_PRESENT.into(), owned(Variant::from(present)));
        out.insert(
            key::IMAGES_MODEL_PRESENT.into(),
            owned(Variant::from(images)),
        );
        out.insert(key::MEDIA_MODEL_PRESENT.into(), owned(Variant::from(media)));
        out.insert(
            key::FFMPEG.into(),
            owned(Variant::from(crate::extract::media::ffmpeg())),
        );
        let db_path = index::default_db().map_err(failed)?;
        let index_bytes: u64 = ["", "-wal", "-shm"]
            .iter()
            .map(|ext| disk_bytes(Path::new(&format!("{}{ext}", db_path.display()))))
            .sum();
        out.insert(key::INDEX_BYTES.into(), owned(Variant::from(index_bytes)));
        out.insert(
            key::MODEL_BYTES.into(),
            owned(Variant::from(model::bytes().map_err(failed)?)),
        );
        for (k, at) in [
            (key::LAST_CHECKED, &st.last_checked),
            (key::LAST_INDEXED, &st.last_indexed),
        ] {
            out.insert(k.into(), owned(Variant::from(u64_of(at))));
        }
        out.insert(
            key::INDEXING_REST.into(),
            owned(Variant::from(st.indexing_rest.load(Ordering::Relaxed))),
        );
        // Media runs on AC only, whatever `index_on_battery` says.
        let media_waiting = if st.power.on_battery() {
            counted.media_partial.max(0) as u64
        } else {
            0
        };
        out.insert(
            key::MEDIA_WAITING_FOR_AC.into(),
            owned(Variant::from(media_waiting)),
        );
        let partial: Vec<(String, String, u64, u64)> = counted
            .partial
            .into_iter()
            .map(|(path, done, total)| {
                let unit = crate::extract::part_unit(Path::new(&path)).to_owned();
                (path, unit, done as u64, total as u64)
            })
            .collect();
        out.insert(key::PARTIAL.into(), owned(Variant::from(partial)));
        out.insert(
            key::MODEL_CHANGED.into(),
            owned(Variant::from(st.model_changed.lock().unwrap().clone())),
        );
        let no_decoder = counted.undecodable;
        out.insert(key::NO_DECODER.into(), owned(Variant::from(no_decoder)));
        let cache = crate::cache_dir().map_err(failed)?;
        out.insert(
            key::CACHE_BYTES.into(),
            owned(Variant::from(disk_bytes(&cache))),
        );
        let enc = st.encryption.lock().unwrap().clone();
        out.insert(key::ENCRYPTED.into(), owned(Variant::from(enc.encrypted)));
        out.insert(key::SEAL.into(), owned(Variant::from(enc.seal)));
        out.insert(
            key::TPM.into(),
            owned(Variant::from(st.tpm.load(Ordering::Relaxed))),
        );
        out.insert(
            key::ENCRYPTION_ERROR.into(),
            owned(Variant::from(enc.error)),
        );
        out.insert(key::SEAL_BROKEN.into(), owned(Variant::from(enc.broken)));
        out.insert(key::SEAL_WAITING.into(), owned(Variant::from(enc.waiting)));
        out.insert(
            key::NEW_SEAL.into(),
            // Here rather than at start, which it would slow.
            owned(Variant::from(
                crate::index::seal::Sealer::default()
                    .new_seal(st.tpm.load(Ordering::Relaxed))
                    .name(),
            )),
        );
        Ok(out)
    }

    /// Index a directory and watch it from now on. Returns once queued.
    fn reindex(&self, path: String) -> zbus::fdo::Result<()> {
        let root = self.root(&path)?;
        self.status.work.root(root);
        self.send(Job::Wake)
    }

    /// Stop indexing a root and drop what was indexed under it.
    fn forget(&self, path: String) -> zbus::fdo::Result<()> {
        let root = fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
        self.request(Request::Forget(root))
    }

    /// The folders holding most of the index, biggest first, with their
    /// chunk counts: at most `limit`, none inside another (index::largest_folders).
    fn largest_folders(&self, limit: u32) -> zbus::fdo::Result<Vec<(String, u64)>> {
        let db = self.db.lock().map_err(failed)?;
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
        let db = self.db.lock().map_err(failed)?;
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

    /// Every file `query` finds, for the search window (#111): the best
    /// chunk of each, at or above its kind's min_score, at most `limit`
    /// (capped at results::MAX) of each kind. See crate::search::results::Found.
    async fn search(
        &self,
        query: String,
        limit: u32,
    ) -> zbus::fdo::Result<Vec<crate::search::results::Found>> {
        let limit = limit.min(results::MAX) as usize;
        let floors = Floors::load(&self.config);
        let hits = chunks(&self.jobs, query, limit * 4).await;
        Ok(
            crate::search::results::every_file(hits, |h| floors.of(h), limit)
                .into_iter()
                .filter_map(|h| {
                    let play = recording(&self.db, &h);
                    let mut found = crate::search::results::found(h)?;
                    if let Some((_, second)) = play {
                        found.4 = "second".to_owned();
                        found.5 = second;
                    }
                    Some(found)
                })
                .collect(),
        )
    }

    /// Open an indexed file at `line`, a number from Search's results, as
    /// KRunner's Run opens a match.
    #[zbus(name = "Open")]
    fn open_file(&self, path: String, line: u32) -> zbus::fdo::Result<()> {
        self.is_indexed(&path)?;
        let at = index::openable_kind(Path::new(&path))
            .and_then(|kind| At::of(kind, Path::new(&path), i64::from(line)));
        open(&self.db, &self.config, &path, at, false);
        Ok(())
    }

    /// Play an indexed audio or video file from `second`, or the recording
    /// an indexed session was transcribed from (#188).
    fn play(&self, path: String, second: u32) -> zbus::fdo::Result<()> {
        self.is_indexed(&path)?;
        let file = if crate::extract::is_session(Path::new(&path)) {
            session_recording(&self.db, &path)
                .ok_or_else(|| failed(format!("{path}: its recording is not indexed")))?
                .display()
                .to_string()
        } else if index::openable_kind(Path::new(&path)) == Some(Kind::Media) {
            path
        } else {
            return Err(failed(format!("{path} is not audio or video")));
        };
        open(
            &self.db,
            &self.config,
            &file,
            Some(At::Second(second)),
            false,
        );
        Ok(())
    }

    /// Open the folder an indexed file is in.
    fn open_folder(&self, path: String) -> zbus::fdo::Result<()> {
        self.is_indexed(&path)?;
        open(&self.db, &self.config, &path, None, true);
        Ok(())
    }

    /// Free the model now.
    fn unload(&self) -> zbus::fdo::Result<()> {
        self.request(Request::Unload)
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
        let wants = self.model_wants();
        status
            .fetch_total
            .store(model::total_bytes(wants), Ordering::Relaxed);
        std::thread::spawn(move || {
            let result = model::fetch(wants, &mut |_, done, _| {
                status.fetch_done.store(done, Ordering::Relaxed);
            });
            match result {
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
        self.request(Request::DeleteModel)
    }

    /// Delete the compiled-model cache; the next model load recompiles (seconds).
    fn clear_cache(&self) -> zbus::fdo::Result<()> {
        self.request(Request::ClearCache)
    }

    /// Exit, releasing the bus name; `krunner` doesn't start it again. For
    /// installs and uninstalls, in place of a service manager's stop.
    fn quit(&self) {
        if let Some(quit) = self.quit.lock().unwrap().take() {
            let _ = quit.send(());
        }
    }

    /// Encrypt or decrypt the index as the config file's `encryption` says,
    /// now rather than at the next read of it; again after a refusal. Queued:
    /// `encrypted`, `seal` and `encryption_error` in Status say how it went.
    fn apply_encryption(&self) -> zbus::fdo::Result<()> {
        let file = Config::load(&self.config).map_err(|e| failed(format!("{e:#}")))?;
        // The watcher too, or it would take its next read for unchanged.
        self.status.reload_config.store(true, Ordering::Relaxed);
        self.request(Request::Encryption(crate::index::encryption::Want::of(
            &file,
        )))
    }

    /// Encrypt the index again under a new key, sealed afresh. Fails while
    /// it isn't encrypted.
    fn rekey(&self) -> zbus::fdo::Result<()> {
        if !self.status.encryption.lock().unwrap().encrypted {
            return Err(failed("the index isn't encrypted"));
        }
        self.request(Request::Rekey)
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

/// Best chunk per file, files at or above their kind's floor, best RESULTS
/// of each kind: text, then documents, then vinoWhisper sessions, then
/// audio and video, then images, less audio, video and images too far below
/// the best of the others (results::far_below), then code (only ever on its
/// own, for `code:`).
/// Near-duplicate images are one result (#78).
pub(crate) fn per_file(hits: Vec<Hit>, floors: Floors) -> Vec<Hit> {
    const IMAGES: usize = 4;
    let mut seen = HashSet::new();
    let mut groups: [Vec<Hit>; 6] = Default::default();
    for h in hits {
        if h.score < floors.of(&h) || !seen.insert(h.path.clone()) {
            continue;
        }
        let group = &mut groups[match h.kind {
            Kind::Text => 0,
            Kind::Document => 1,
            Kind::Session => 2,
            Kind::Media => 3,
            Kind::Image => IMAGES,
            Kind::Code => 5,
        }];
        group.push(h);
    }
    // Before the cut, so a burst folded into one leaves room for others.
    groups[IMAGES] = crate::search::dupes::collapse(std::mem::take(&mut groups[IMAGES]));
    for group in &mut groups {
        group.truncate(RESULTS);
    }
    let mut kept = groups.into_iter().flatten().collect();
    crate::search::results::drop_far_below(&mut kept);
    kept
}

/// A runner that stopped before it owned the bus name, which exits with
/// start::NOT_STARTED so `krunner` doesn't start it again.
#[derive(Debug)]
pub struct NotStarted(pub(crate) anyhow::Error);

impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for NotStarted {}

/// `vinosearch krunner`: this same command line with `--worker`, the
/// runner, kept running by service::start. False when it stopped on an
/// error.
pub fn supervise() -> Result<bool> {
    let mut worker = std::process::Command::new(std::env::current_exe()?);
    worker.args(std::env::args_os().skip(1)).arg("--worker");
    Ok(start::supervise(worker)?.is_none_or(|s| s.success()))
}

pub fn serve(opts: Options) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    worker::prepare(&opts.status, &opts.file).map_err(NotStarted)?;
    crate::service::power::spawn(tx.clone(), opts.status.clone());
    let _watcher = watch::spawn(
        tx.clone(),
        opts.config.clone(),
        opts.file.clone(),
        opts.status.clone(),
    )
    .map_err(NotStarted)?;
    let db: Db = index::default_db()
        .and_then(|p| crate::index::encryption::Live::open(&p))
        .map_err(NotStarted)?;
    let (quit, quitting) = oneshot::channel();
    let control = Control {
        jobs: Mutex::new(tx.clone()),
        status: opts.status.clone(),
        db: db.clone(),
        config: opts.config.clone(),
        quit: Mutex::new(Some(quit)),
        counted: Mutex::new(None),
    };
    let config = opts.config.clone();
    // The connection is driven by the thread that blocks on it, so that is
    // this one; the worker reports back through a channel the future awaits.
    let (done, finished) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = done.send(worker::run(rx, opts));
    });
    zbus::block_on(async {
        let conn = zbus::connection::Builder::session()
            .name(names().service.as_str())
            .serve_at(
                OBJECT,
                Runner {
                    jobs: Mutex::new(tx.clone()),
                    db: db.clone(),
                    config: config.clone(),
                    places: Mutex::default(),
                    query: Mutex::default(),
                },
            )
            .serve_at(
                GNOME,
                Gnome {
                    jobs: Mutex::new(tx),
                    db,
                    config,
                    last: Mutex::default(),
                },
            )
            .serve_at(CONTROL, control)
            .build()
            .await
            .context("registering on the session bus")
            .map_err(NotStarted)?;
        eprintln!("{} on {OBJECT}", names().service);
        let worker = async {
            finished
                .await
                .map_err(|_| anyhow::anyhow!("worker panicked"))?
        };
        let quit = async {
            if quitting.await.is_err() {
                std::future::pending::<()>().await;
            }
            // Before exiting, so Quit's reply goes out first.
            let _ = conn.release_name(names().service.as_str()).await;
            eprintln!("asked to quit");
            Ok(())
        };
        // A runner that outlived its session would hold the instance lock
        // against the next one. NameLost is only what the stream is on; it
        // ends when the connection does.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")
            .member("NameLost")
            .build()?;
        let mut bus = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
        let gone = async {
            use futures_lite::StreamExt;
            while let Some(Ok(_)) = bus.next().await {}
            bail!("the session bus went away")
        };
        futures_lite::future::or(futures_lite::future::or(worker, quit), gone).await
    })
}

/// Delete the models: through the runner while it runs, so it unloads them
/// first and refuses during a download; from disk otherwise. True when the
/// runner was asked, which deletes them once its worker takes the request.
pub fn delete_models() -> Result<bool> {
    // Without a session bus no runner can be running. A bus that doesn't
    // answer is an error: deleting under a runner mid-download would leave
    // half a model.
    if !matches!(
        runner_owned().context("asking whether the service is running")?,
        Some(true)
    ) {
        model::delete()?;
        return Ok(false);
    }
    zbus::block_on(async {
        let conn = zbus::Connection::session().await?;
        conn.call_method(
            Some(names().service.as_str()),
            CONTROL,
            Some(vinosearch_shared::dbus::CONTROL_INTERFACE),
            "DeleteModel",
            &(),
        )
        .await
    })
    .map_err(|e| match e {
        zbus::Error::MethodError(_, Some(why), _) => anyhow::anyhow!("the service refused: {why}"),
        other => other.into(),
    })?;
    Ok(true)
}

/// Whether something owns the runner's bus name; None without a session bus.
fn runner_owned() -> Result<Option<bool>> {
    zbus::block_on(async {
        let Ok(conn) = zbus::Connection::session().await else {
            return Ok(None);
        };
        let bus = zbus::fdo::DBusProxy::new(&conn).await?;
        Ok(Some(
            bus.name_has_owner(names().service.as_str().try_into()?)
                .await?,
        ))
    })
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
            vector: None,
            similar: Vec::new(),
        }
    }

    #[test]
    fn text_read_in_an_image_is_listed_with_the_images() {
        let category = |kind, path| {
            let m = to_match(hit(kind, 0.9, path), FLOORS, None);
            match m.5.get("category").map(|v| String::try_from(v.clone())) {
                Some(Ok(c)) => c,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(category(Kind::Document, "/p/shot.png"), "vinoSearch Images");
        assert_eq!(
            category(Kind::Document, "/d/scan.pdf"),
            "vinoSearch Documents"
        );
        assert_eq!(category(Kind::Image, "/p/cat.jpg"), "vinoSearch Images");
    }

    #[test]
    fn code_has_a_group_and_opens_at_its_line() {
        let mut h = hit(Kind::Code, 0.7, "/src/net.rs");
        h.line = 24;
        h.text = "pub fn check_url(url: &str) -> Result<()> {".into();
        let m = to_match(h, FLOORS, None);
        let get = |k: &str| m.5.get(k).cloned();
        assert_eq!(
            get("category").map(|v| String::try_from(v).unwrap()),
            Some("vinoSearch Code".into())
        );
        assert_eq!(get("line").map(|v| u32::try_from(v).unwrap()), Some(24));
        let subtext = String::try_from(get("subtext").unwrap()).unwrap();
        assert!(
            subtext.starts_with("/src:24  pub fn check_url"),
            "{subtext}"
        );
    }

    #[test]
    fn a_session_says_which_part_matched_and_offers_the_other() {
        use crate::extract::session::tests::{SUMMARY_TEXT, bundle};
        let dir =
            std::env::temp_dir().join(format!("vinosearch-krunner-session-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-10-27-140512.vwsession");
        fs::write(&path, bundle(Some(SUMMARY_TEXT), "vinowhisper-session/1")).unwrap();
        let session = |line: i64| {
            let mut h = hit(Kind::Session, 0.8, path.to_str().unwrap());
            h.line = line;
            let m = to_match(h, FLOORS, None);
            let get = |k: &str| m.5.get(k).cloned().unwrap();
            (
                String::try_from(get("category")).unwrap(),
                String::try_from(get("subtext")).unwrap(),
                Vec::<String>::try_from(get("actions")).unwrap(),
                m.5.get("line").map(|v| u32::try_from(v.clone()).unwrap()),
            )
        };
        let in_the_0112_paragraph = 6;
        let (category, subtext, actions, line) = session(in_the_0112_paragraph);
        assert_eq!(category, "vinoWhisper sessions");
        assert!(
            subtext.starts_with("Transcript at 01:12 · 2026-10-27 14:05, 43 min"),
            "{subtext}"
        );
        assert_eq!(actions, [OPEN_SUMMARY, OPEN_FOLDER, SHOW_ALL]);
        assert_eq!(line, Some(6));
        let summary = crate::extract::session::SUMMARY_LINES as i64 + 1;
        let (_, subtext, actions, line) = session(summary);
        assert!(subtext.starts_with("Summary, AI-generated · "), "{subtext}");
        assert_eq!(actions, [OPEN_TRANSCRIPT, OPEN_FOLDER, SHOW_ALL]);
        assert_eq!(line, None);
        let m = to_match(hit(Kind::Text, 0.8, "/n.md"), FLOORS, None);
        let actions = Vec::<String>::try_from(m.5.get("actions").cloned().unwrap()).unwrap();
        assert_eq!(actions, [OPEN_FOLDER, SHOW_ALL]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn code_is_held_to_its_own_floor() {
        let hits = vec![
            hit(Kind::Code, 0.62, "/a.rs"),
            hit(Kind::Code, 0.58, "/b.rs"),
        ];
        assert_eq!(paths(hits), ["/a.rs"]);
    }

    #[test]
    fn text_read_in_an_image_has_no_page_to_name() {
        let mut shot = hit(Kind::Document, 0.8, "/home/u/Pictures/Screenshots/a.png");
        shot.text = "Invoice INV-2026-004817".into();
        let (name, description) = describe(&shot);
        assert_eq!(name, "a.png");
        assert_eq!(
            description,
            "/home/u/Pictures/Screenshots  Invoice INV-2026-004817"
        );
        let mut scan = hit(Kind::Document, 0.8, "/d/scan.pdf");
        scan.line = 3;
        assert!(describe(&scan).1.starts_with("/d, page 3"));
    }

    const FLOORS: Floors = Floors {
        text: 0.65,
        image: 0.63,
        code: 0.6,
        media: 0.62,
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
    fn audio_and_video_far_below_a_session_are_dropped() {
        let hits = vec![
            hit(Kind::Session, 0.793, "/s/valorant.vwsession"),
            hit(Kind::Media, 0.661, "/v/promo.mp4"),
            hit(Kind::Image, 0.668, "/p/wallpaper.webp"),
        ];
        assert_eq!(paths(hits), ["/s/valorant.vwsession"]);
    }

    #[test]
    fn text_read_in_an_image_is_held_to_the_image_floor_and_gap() {
        let floors = Floors {
            image: 0.68,
            media: 0.68,
            ..FLOORS
        };
        let kept =
            |hits| -> Vec<String> { per_file(hits, floors).into_iter().map(|h| h.path).collect() };
        let text = |score, path| hit(Kind::Document, score, path);
        assert!(kept(vec![text(0.66, "/src/doc/main.png")]).is_empty());
        assert_eq!(kept(vec![text(0.66, "/d/scan.pdf")]), ["/d/scan.pdf"]);
        let hits = vec![
            hit(Kind::Session, 0.793, "/s/valorant.vwsession"),
            text(0.70, "/src/doc/main.png"),
        ];
        assert_eq!(kept(hits), ["/s/valorant.vwsession"]);
    }

    #[test]
    fn audio_and_video_alone_need_only_their_floor() {
        let hits = vec![
            hit(Kind::Text, 0.60, "/below-floor.md"),
            hit(Kind::Media, 0.66, "/v/dog-barking.mp4"),
        ];
        assert_eq!(paths(hits), ["/v/dog-barking.mp4"]);
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
    fn a_burst_takes_one_place_and_leaves_room_for_others() {
        let photo = |path: &str, score: f32, axis: usize| {
            let mut h = hit(Kind::Image, score, path);
            let mut v = vec![0.0; 8];
            v[axis] = 1.0;
            h.vector = Some(v);
            h
        };
        let mut hits: Vec<Hit> = (0..4)
            .map(|i| photo(&format!("/p/burst{i}.jpg"), 0.80 - 0.01 * i as f32, 0))
            .collect();
        hits.extend((1..6).map(|i| photo(&format!("/p/other{i}.jpg"), 0.70, i)));
        let got = per_file(hits, FLOORS);
        assert_eq!(got.len(), RESULTS);
        assert_eq!(got[0].similar.len(), 3);
        assert!(
            describe(&got[0]).1.ends_with("(+3 similar)"),
            "{:?}",
            describe(&got[0])
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
        drop(conn);
        let db: Db = crate::index::encryption::Live::open(&dir.join("i.sqlite")).unwrap();
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

    #[test]
    fn only_letters_digits_slash_and_four_marks_stay_unescaped_in_a_file_url() {
        for b in 0u8..=127 {
            let c = b as char;
            let kept = c.is_ascii_alphanumeric() || "/-._~".contains(c);
            let want = if kept {
                format!("file://{c}")
            } else {
                format!("file://%{b:02X}")
            };
            assert_eq!(file_url(&c.to_string()), want, "byte {b}");
        }
    }

    #[test]
    fn the_search_window_opens_natively_and_from_the_flatpak() {
        assert_eq!(
            settings_argv(None, "/usr/bin/vinosearch-settings", &["--search", "a b"]),
            ["/usr/bin/vinosearch-settings", "--search", "a b"]
        );
        // On the host, where open::spawn runs it: `flatpak-spawn` there has
        // no sandbox to spawn from, and /app/bin isn't the host's.
        assert_eq!(
            settings_argv(
                Some("dev.vinosearch.VinoSearch"),
                "/app/bin/vinosearch-settings",
                &["--search", "a b"]
            ),
            [
                "flatpak",
                "run",
                "--command=vinosearch-settings",
                "dev.vinosearch.VinoSearch",
                "--search",
                "a b"
            ]
        );
    }
}
