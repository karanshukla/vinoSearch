//! Names on the session bus. See `docs/dbus.md` for the methods.

/// Well-known name the runner owns; D-Bus activation starts it on first use.
pub const SERVICE: &str = "dev.vinosearch.Runner";
/// KRunner's interface (`org.kde.krunner1`) lives here.
pub const RUNNER_PATH: &str = "/runner";
/// GNOME Shell's org.gnome.Shell.SearchProvider2 lives here.
pub const GNOME_PATH: &str = "/gnome";
/// The desktop entry GNOME Shell shows the provider's results under.
pub const DESKTOP_ID: &str = "dev.vinosearch.desktop";
/// The settings app's interface lives here.
pub const CONTROL_PATH: &str = "/control";
pub const CONTROL_INTERFACE: &str = "dev.vinosearch.Control1";

/// Keys in `Control1.Status`'s `a{sv}`.
pub mod status {
    pub const MODEL_LOADED: &str = "model_loaded";
    pub const INDEXING: &str = "indexing";
    pub const DEVICE: &str = "device";
    pub const FILES: &str = "files";
    pub const CHUNKS: &str = "chunks";
    pub const ROOTS: &str = "roots";
    /// Files embedded so far by the current indexing job.
    pub const INDEXED_FILES: &str = "indexed_files";
    pub const MODEL_PRESENT: &str = "model_present";
    pub const FETCHING: &str = "fetching";
    pub const FETCH_DONE: &str = "fetch_done";
    pub const FETCH_TOTAL: &str = "fetch_total";
    pub const FETCH_ERROR: &str = "fetch_error";
    /// Bytes on disk: the index database, the model, the compiled-model cache.
    pub const INDEX_BYTES: &str = "index_bytes";
    pub const MODEL_BYTES: &str = "model_bytes";
    pub const CACHE_BYTES: &str = "cache_bytes";
}
