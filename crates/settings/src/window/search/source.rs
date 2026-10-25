//! Where the search window's results come from, and where its actions go: the
//! runner's `Control1` in the app, a fake in the tests. Called from a thread of
//! its own, as `control::Link` does, so a search that loads the model first
//! never stalls a frame.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui;
use vinosearch_shared::dbus::{self, results, status};
use zbus::zvariant::{OwnedValue, Value};

/// What a result is, as the window groups them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Document,
    /// A photo, or text read in one.
    Image,
}

/// One file a search found.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub path: String,
    pub kind: Kind,
    pub score: f64,
    /// The matching text, flattened; empty for a photo.
    pub snippet: String,
    /// What `at` counts: "line", "page", "slide", ...; empty for nothing.
    pub unit: String,
    /// Where the match is, from 1; 0 for nowhere in particular.
    pub at: u32,
    /// Unix seconds.
    pub mtime: i64,
}

impl Found {
    pub fn name(&self) -> &str {
        Path::new(&self.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.path)
    }

    pub fn folder(&self) -> &Path {
        Path::new(&self.path).parent().unwrap_or(Path::new("/"))
    }

    /// "page 4", "line 12"; empty when the match has no place to name.
    pub fn place(&self) -> String {
        if self.unit.is_empty() || self.at == 0 {
            String::new()
        } else {
            format!("{} {}", self.unit, self.at)
        }
    }

    /// The extension, lowercased; empty for none.
    pub fn extension(&self) -> String {
        Path::new(&self.path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
    }
}

pub trait Source: Send + 'static {
    fn search(&mut self, query: &str) -> Result<Vec<Found>, String>;
    /// The indexed folders, for the folder filter.
    fn roots(&mut self) -> Result<Vec<String>, String>;
    /// Open `path` at `at` (0 for the top), as KRunner opens a match.
    fn open(&mut self, path: &str, at: u32) -> Result<(), String>;
    fn open_folder(&mut self, path: &str) -> Result<(), String>;
    /// Put `path` on the clipboard as a file.
    fn copy(&mut self, path: &str) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ask {
    /// The query, and a number the answer comes back with.
    Search(u64, String),
    Open(String, u32),
    OpenFolder(String),
    Copy(String),
}

#[derive(Default)]
pub struct Answers {
    /// The latest search answered, by its number.
    pub results: Option<(u64, Result<Vec<Found>, String>)>,
    pub roots: Vec<String>,
    /// Why the last action failed, until the window shows it.
    pub refused: Option<String>,
    /// Said once an action is done, until the window shows it.
    pub done: Option<String>,
}

pub struct Link {
    asks: Sender<Ask>,
    pub shared: Arc<Mutex<Answers>>,
    /// Set by the window's first frame, to wake it when an answer lands.
    ctx: Arc<OnceLock<egui::Context>>,
}

impl Link {
    pub fn start(mut source: impl Source) -> Self {
        let (asks, rx): (Sender<Ask>, Receiver<Ask>) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Answers::default()));
        let ctx: Arc<OnceLock<egui::Context>> = Arc::default();
        let (out, wake) = (Arc::clone(&shared), Arc::clone(&ctx));
        std::thread::spawn(move || {
            let repaint = || {
                if let Some(ctx) = wake.get() {
                    ctx.request_repaint();
                }
            };
            if let Ok(roots) = source.roots() {
                out.lock().unwrap().roots = roots;
                repaint();
            }
            while let Ok(mut ask) = rx.recv() {
                // Only the newest of the searches waiting is worth running.
                while let Ask::Search(..) = ask {
                    match rx.try_recv() {
                        Ok(next @ Ask::Search(..)) => ask = next,
                        Ok(other) => {
                            run(&mut source, &out, ask);
                            ask = other;
                        }
                        Err(_) => break,
                    }
                }
                run(&mut source, &out, ask);
                repaint();
            }
        });
        Link { asks, shared, ctx }
    }

    pub fn attach(&self, ctx: &egui::Context) {
        let _ = self.ctx.set(ctx.clone());
    }

    pub fn send(&self, ask: Ask) {
        // The thread only stops with the window.
        let _ = self.asks.send(ask);
    }
}

fn run(source: &mut impl Source, out: &Mutex<Answers>, ask: Ask) {
    let name = |path: &str| {
        Path::new(path)
            .file_name()
            .map_or(path.to_owned(), |n| n.to_string_lossy().into_owned())
    };
    let (result, done) = match ask {
        Ask::Search(n, query) => {
            let found = source.search(&query);
            out.lock().unwrap().results = Some((n, found));
            return;
        }
        Ask::Open(path, at) => (source.open(&path, at), None),
        Ask::OpenFolder(path) => (source.open_folder(&path), None),
        Ask::Copy(path) => {
            let done = format!("Copied {}", name(&path));
            (source.copy(&path), Some(done))
        }
    };
    let mut out = out.lock().unwrap();
    match result {
        Ok(()) => out.done = done,
        Err(e) => out.refused = Some(e),
    }
}

/// The runner's `Control1`.
#[derive(Default)]
pub struct Runner {
    conn: Option<zbus::blocking::Connection>,
}

type Row = (String, String, f64, String, String, u32, i64);

impl Runner {
    fn proxy(&mut self) -> zbus::Result<zbus::blocking::Proxy<'static>> {
        let c = match &self.conn {
            Some(c) => c.clone(),
            None => self
                .conn
                .insert(zbus::blocking::Connection::session()?)
                .clone(),
        };
        zbus::blocking::Proxy::new_owned(
            c,
            dbus::SERVICE.to_owned(),
            dbus::CONTROL_PATH.to_owned(),
            dbus::CONTROL_INTERFACE.to_owned(),
        )
    }

    /// The runner's own message for a refusal; otherwise what went wrong on
    /// the bus, after which the next call reconnects.
    fn explain(&mut self, e: zbus::Error) -> String {
        match e {
            zbus::Error::MethodError(name, _, _) if name.as_str().ends_with("ServiceUnknown") => {
                "The search service isn't installed. Turn it on in vinoSearch settings, \
                 under Integration."
                    .to_owned()
            }
            zbus::Error::MethodError(name, _, _) if name.as_str().ends_with("UnknownMethod") => {
                "The search service is older than this window. Restart it to search here."
                    .to_owned()
            }
            zbus::Error::MethodError(_, Some(message), _) => message,
            other => {
                self.conn = None;
                other.to_string()
            }
        }
    }
}

impl Source for Runner {
    fn search(&mut self, query: &str) -> Result<Vec<Found>, String> {
        let rows: zbus::Result<Vec<Row>> = self
            .proxy()
            .and_then(|p| p.call("Search", &(query, results::MAX)));
        let rows = rows.map_err(|e| self.explain(e))?;
        Ok(rows
            .into_iter()
            .map(|(path, kind, score, snippet, unit, at, mtime)| Found {
                path,
                kind: match kind.as_str() {
                    results::TEXT => Kind::Text,
                    results::DOCUMENT => Kind::Document,
                    _ => Kind::Image,
                },
                score,
                snippet,
                unit,
                at,
                mtime,
            })
            .collect())
    }

    fn roots(&mut self) -> Result<Vec<String>, String> {
        let map: zbus::Result<std::collections::HashMap<String, OwnedValue>> =
            self.proxy().and_then(|p| p.call("Status", &()));
        let map = map.map_err(|e| self.explain(e))?;
        Ok(match map.get(status::ROOTS).map(|v| &**v) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Value::Str(s) => Some(s.to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
    }

    fn open(&mut self, path: &str, at: u32) -> Result<(), String> {
        let done: zbus::Result<()> = self.proxy().and_then(|p| p.call("Open", &(path, at)));
        done.map_err(|e| self.explain(e))
    }

    fn open_folder(&mut self, path: &str) -> Result<(), String> {
        let done: zbus::Result<()> = self.proxy().and_then(|p| p.call("OpenFolder", &(path,)));
        done.map_err(|e| self.explain(e))
    }

    /// As a `text/uri-list`, which file managers paste as the file. egui's
    /// clipboard only takes text, so through wl-copy: the window is Wayland
    /// only.
    fn copy(&mut self, path: &str) -> Result<(), String> {
        let uri = format!("{}\r\n", super::thumbs::file_uri(Path::new(path)));
        let status = std::process::Command::new("wl-copy")
            .args(["--type", "text/uri-list", &uri])
            .status()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    "Copying a file needs wl-copy, from the wl-clipboard package.".to_owned()
                }
                _ => format!("wl-copy: {e}"),
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("wl-copy failed ({status})"))
        }
    }
}
