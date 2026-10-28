//! The runner's `dev.vinosearch.Control1`, from a thread of its own so a slow
//! D-Bus activation never stalls a frame. It polls `Status` every second and
//! runs whatever the window asks for in between.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use vinosearch_shared::dbus::{self, status};
use zbus::zvariant::{OwnedValue, Value};

const POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub model_loaded: bool,
    pub indexing: bool,
    pub paused: bool,
    /// Why indexing is held for power: a `dbus::hold` value, or empty.
    pub power_hold: String,
    pub indexed_files: u64,
    pub device: String,
    pub files: i64,
    pub chunks: i64,
    /// Per format: a file counts under its chunks' kind.
    pub text_files: i64,
    pub text_chunks: i64,
    pub document_files: i64,
    pub document_chunks: i64,
    pub image_files: i64,
    pub code_files: i64,
    pub code_chunks: i64,
    pub session_files: i64,
    pub session_chunks: i64,
    /// Sessions not read: a newer format, or broken (#143).
    pub sessions_unread: i64,
    pub roots: Vec<String>,
    pub model_present: bool,
    pub fetching: bool,
    pub fetch_done: u64,
    pub fetch_total: u64,
    pub fetch_error: String,
    pub index_bytes: u64,
    pub model_bytes: u64,
    pub cache_bytes: u64,
    /// Unix seconds; 0 for not since the runner started.
    pub last_checked: u64,
    pub last_indexed: u64,
    /// Why files are being indexed again after a start; empty otherwise.
    pub model_changed: String,
    /// Photos skipped because no HEIF decoder is installed.
    pub no_decoder: i64,
    /// The indexing job is the rest of a long document.
    pub indexing_rest: bool,
    /// Documents indexed in part so far, the next in line first.
    pub partial: Vec<Partial>,
    /// The index on disk is encrypted (#4).
    pub encrypted: bool,
    /// What its key is sealed with: a `dbus::seal` value, or empty.
    pub seal: String,
    /// systemd can seal a key to a TPM here.
    pub tpm: bool,
    /// Why the last change to encryption was refused or failed.
    pub encryption_error: String,
    /// Why the index was rebuilt from the files at the runner's start.
    pub seal_broken: String,
}

/// A long document with only some of its parts indexed so far (#108).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Partial {
    pub path: String,
    /// "page", "slide", "chapter" or "part".
    pub unit: String,
    pub done: u64,
    pub total: u64,
}

impl Status {
    fn parse(map: &HashMap<String, OwnedValue>) -> Self {
        let get = |key: &str| map.get(key).map(|v| &**v);
        let flag = |key| matches!(get(key), Some(Value::Bool(true)));
        let unsigned = |key| match get(key) {
            Some(Value::U64(n)) => *n,
            _ => 0,
        };
        let signed = |key| match get(key) {
            Some(Value::I64(n)) => *n,
            _ => 0,
        };
        let text = |key| match get(key) {
            Some(Value::Str(s)) => s.to_string(),
            _ => String::new(),
        };
        let roots = match get(status::ROOTS) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Value::Str(s) => Some(s.to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let partial = match get(status::PARTIAL) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Value::Structure(s) => match s.fields() {
                        [
                            Value::Str(path),
                            Value::Str(unit),
                            Value::U64(done),
                            Value::U64(total),
                        ] => Some(Partial {
                            path: path.to_string(),
                            unit: unit.to_string(),
                            done: *done,
                            total: *total,
                        }),
                        _ => None,
                    },
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        Status {
            model_loaded: flag(status::MODEL_LOADED),
            indexing: flag(status::INDEXING),
            paused: flag(status::PAUSED),
            power_hold: text(status::POWER_HOLD),
            indexed_files: unsigned(status::INDEXED_FILES),
            device: text(status::DEVICE),
            files: signed(status::FILES),
            chunks: signed(status::CHUNKS),
            text_files: signed(status::TEXT_FILES),
            text_chunks: signed(status::TEXT_CHUNKS),
            document_files: signed(status::DOCUMENT_FILES),
            document_chunks: signed(status::DOCUMENT_CHUNKS),
            image_files: signed(status::IMAGE_FILES),
            code_files: signed(status::CODE_FILES),
            code_chunks: signed(status::CODE_CHUNKS),
            session_files: signed(status::SESSION_FILES),
            session_chunks: signed(status::SESSION_CHUNKS),
            sessions_unread: signed(status::SESSIONS_UNREAD),
            roots,
            model_present: flag(status::MODEL_PRESENT),
            fetching: flag(status::FETCHING),
            fetch_done: unsigned(status::FETCH_DONE),
            fetch_total: unsigned(status::FETCH_TOTAL),
            fetch_error: text(status::FETCH_ERROR),
            index_bytes: unsigned(status::INDEX_BYTES),
            model_bytes: unsigned(status::MODEL_BYTES),
            cache_bytes: unsigned(status::CACHE_BYTES),
            last_checked: unsigned(status::LAST_CHECKED),
            last_indexed: unsigned(status::LAST_INDEXED),
            model_changed: text(status::MODEL_CHANGED),
            no_decoder: signed(status::NO_DECODER),
            indexing_rest: flag(status::INDEXING_REST),
            partial,
            encrypted: flag(status::ENCRYPTED),
            seal: text(status::SEAL),
            tpm: flag(status::TPM),
            encryption_error: text(status::ENCRYPTION_ERROR),
            seal_broken: text(status::SEAL_BROKEN),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Request {
    Reindex(String),
    Forget(String),
    Unload,
    FetchModel,
    DeleteModel,
    ClearCache,
    CheckNow,
    PauseIndexing,
    ResumeIndexing,
    /// Answered in `Shared::largest`.
    LargestFolders(u32),
    /// Apply the config file's `encryption` now.
    ApplyEncryption,
    Rekey,
}

#[derive(Clone, Debug, Default)]
pub struct Shared {
    /// None until the first answer; Err while the runner can't be reached.
    pub status: Option<Result<Status, String>>,
    /// Why the last request failed, until the window clears it.
    pub refused: Option<String>,
    /// The last `LargestFolders` answer: (path, chunks), biggest first.
    pub largest: Option<Vec<(String, u64)>>,
}

pub struct Link {
    requests: Sender<Request>,
    pub shared: Arc<Mutex<Shared>>,
}

impl Link {
    pub fn start(ctx: &egui::Context) -> Self {
        let (requests, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let out = Arc::clone(&shared);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut conn: Option<zbus::blocking::Connection> = None;
            loop {
                let request = match rx.recv_timeout(POLL) {
                    Ok(r) => Some(r),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                match request {
                    Some(Request::LargestFolders(limit)) => {
                        let answer = proxy(&mut conn)
                            .and_then(|p| p.call("LargestFolders", &(limit,)))
                            .map_err(|e| explain(&mut conn, e));
                        let mut shared = out.lock().unwrap();
                        match answer {
                            Ok(folders) => shared.largest = Some(folders),
                            Err(e) => shared.refused = Some(e),
                        }
                        ctx.request_repaint();
                    }
                    Some(request) => {
                        if let Err(e) = call(&mut conn, &request) {
                            out.lock().unwrap().refused = Some(e);
                        }
                    }
                    None => {}
                }
                let answer = status_of(&mut conn);
                let mut shared = out.lock().unwrap();
                if shared.status.as_ref() != Some(&answer) {
                    shared.status = Some(answer);
                    ctx.request_repaint();
                } else if shared.refused.is_some() {
                    ctx.request_repaint();
                }
            }
        });
        Link { requests, shared }
    }

    /// No thread and no bus: what the window asks for lands on the receiver.
    #[cfg(test)]
    pub fn detached() -> (Self, mpsc::Receiver<Request>) {
        let (requests, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        (Link { requests, shared }, rx)
    }

    pub fn send(&self, request: Request) {
        // The thread only stops with the window.
        let _ = self.requests.send(request);
    }
}

fn proxy(
    conn: &mut Option<zbus::blocking::Connection>,
) -> zbus::Result<zbus::blocking::Proxy<'static>> {
    let c = match conn {
        Some(c) => c.clone(),
        None => conn.insert(zbus::blocking::Connection::session()?).clone(),
    };
    zbus::blocking::Proxy::new_owned(
        c,
        dbus::SERVICE.to_owned(),
        dbus::CONTROL_PATH.to_owned(),
        dbus::CONTROL_INTERFACE.to_owned(),
    )
}

fn status_of(conn: &mut Option<zbus::blocking::Connection>) -> Result<Status, String> {
    let map: HashMap<String, OwnedValue> = proxy(conn)
        .and_then(|p| p.call("Status", &()))
        .map_err(|e| explain(conn, e))?;
    Ok(Status::parse(&map))
}

fn call(conn: &mut Option<zbus::blocking::Connection>, request: &Request) -> Result<(), String> {
    let result = proxy(conn).and_then(|p| match request {
        Request::Reindex(path) => p.call::<_, _, ()>("Reindex", &(path,)),
        Request::Forget(path) => p.call("Forget", &(path,)),
        Request::Unload => p.call("Unload", &()),
        Request::FetchModel => p.call("FetchModel", &()),
        Request::DeleteModel => p.call("DeleteModel", &()),
        Request::ClearCache => p.call("ClearCache", &()),
        Request::CheckNow => p.call("CheckNow", &()),
        Request::PauseIndexing => p.call("PauseIndexing", &()),
        Request::ResumeIndexing => p.call("ResumeIndexing", &()),
        Request::ApplyEncryption => p.call("ApplyEncryption", &()),
        Request::Rekey => p.call("Rekey", &()),
        Request::LargestFolders(_) => unreachable!("answered in the loop"),
    });
    result.map_err(|e| explain(conn, e))
}

/// The runner's own message for a refusal; otherwise what went wrong on the
/// bus, after which the next call reconnects.
fn explain(conn: &mut Option<zbus::blocking::Connection>, e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, _, _) if name.as_str().ends_with("ServiceUnknown") => {
            "The search service isn't installed. Turn it on under Integration.".to_owned()
        }
        zbus::Error::MethodError(_, Some(message), _) => message,
        other => {
            *conn = None;
            other.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Array;

    #[test]
    fn status_reads_every_key_and_tolerates_missing_ones() {
        let mut map = HashMap::new();
        let mut put = |k: &str, v: Value<'static>| {
            map.insert(k.to_owned(), OwnedValue::try_from(v).unwrap());
        };
        put(status::INDEXING, Value::Bool(true));
        put(status::FILES, Value::I64(4395));
        put(status::IMAGE_FILES, Value::I64(12));
        put(status::INDEXED_FILES, Value::U64(1709));
        put(status::DEVICE, Value::from("NPU"));
        put(
            status::ROOTS,
            Value::Array(Array::from(vec!["/home/u", "/home/u/Development"])),
        );
        let s = Status::parse(&map);
        assert!(s.indexing);
        assert!(!s.model_loaded);
        assert_eq!(s.files, 4395);
        assert_eq!(s.indexed_files, 1709);
        assert_eq!(s.device, "NPU");
        assert_eq!(s.roots, ["/home/u", "/home/u/Development"]);
        assert_eq!(s.chunks, 0);
        assert_eq!(s.image_files, 12);
        assert_eq!(s.text_files, 0);
        assert!(s.partial.is_empty());
        assert!(!s.encrypted && !s.tpm);
        assert_eq!(s.seal, "");
    }

    #[test]
    fn status_reads_encryption() {
        let mut map = HashMap::new();
        map.insert(status::ENCRYPTED.to_owned(), OwnedValue::from(true));
        map.insert(status::TPM.to_owned(), OwnedValue::from(true));
        for (k, v) in [
            (status::SEAL, "tpm"),
            (status::ENCRYPTION_ERROR, ""),
            (status::SEAL_BROKEN, "the TPM was cleared"),
        ] {
            map.insert(k.to_owned(), OwnedValue::try_from(Value::from(v)).unwrap());
        }
        let s = Status::parse(&map);
        assert!(s.encrypted && s.tpm);
        assert_eq!(s.seal, "tpm");
        assert_eq!(s.seal_broken, "the TPM was cleared");
    }

    #[test]
    fn status_reads_documents_indexed_in_part() {
        let partial = vec![
            ("/b/book.pdf".to_owned(), "page".to_owned(), 120u64, 640u64),
            ("/b/notes.txt".to_owned(), "part".to_owned(), 30, 400),
        ];
        let mut map = HashMap::new();
        map.insert(
            status::PARTIAL.to_owned(),
            OwnedValue::try_from(Value::from(partial)).unwrap(),
        );
        map.insert(status::INDEXING_REST.to_owned(), OwnedValue::from(true));
        let s = Status::parse(&map);
        assert!(s.indexing_rest);
        assert_eq!(
            s.partial,
            [
                Partial {
                    path: "/b/book.pdf".into(),
                    unit: "page".into(),
                    done: 120,
                    total: 640,
                },
                Partial {
                    path: "/b/notes.txt".into(),
                    unit: "part".into(),
                    done: 30,
                    total: 400,
                },
            ]
        );
    }
}
