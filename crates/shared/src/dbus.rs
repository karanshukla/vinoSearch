//! Names on the session bus. See `docs/dbus.md` for the methods.

/// Well-known name the runner owns; D-Bus activation starts it on first use.
pub const SERVICE: &str = "dev.vinosearch.Runner";
/// KRunner's interface (`org.kde.krunner1`) lives here.
pub const RUNNER_PATH: &str = "/runner";
/// GNOME Shell's org.gnome.Shell.SearchProvider2 lives here.
pub const GNOME_PATH: &str = "/gnome";
/// The desktop entry GNOME Shell shows the provider's results under. It also
/// launches the settings window, whose Wayland app id is the same name, so
/// the window gets the entry's icon.
pub const DESKTOP_ID: &str = "dev.vinosearch.desktop";
pub const SETTINGS_APP_ID: &str = "dev.vinosearch";
/// Icon name, installed under hicolor/scalable/apps.
pub const ICON: &str = "dev.vinosearch";
/// KRunner's plugin file, under the XDG data directory. Its presence is the
/// KDE search place being on (#45).
pub const KRUNNER_PLUGIN: &str = "krunner/dbusplugins/vinosearch.desktop";
/// GNOME Shell's search provider file, under the XDG data directory, in a
/// directory of ours that environment.d adds to XDG_DATA_DIRS. Its presence
/// is the GNOME search place being on.
pub const GNOME_PROVIDER: &str =
    "vinosearch/xdg/gnome-shell/search-providers/dev.vinosearch.search-provider.ini";
/// The D-Bus activation file, under the XDG data directory: the service is
/// installed while it exists.
pub const DBUS_SERVICE_FILE: &str = "dbus-1/services/dev.vinosearch.Runner.service";
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
    /// The same, per format (#73): a file counts under its chunks' kind.
    pub const TEXT_FILES: &str = "text_files";
    pub const TEXT_CHUNKS: &str = "text_chunks";
    pub const DOCUMENT_FILES: &str = "document_files";
    pub const DOCUMENT_CHUNKS: &str = "document_chunks";
    /// One chunk per image, so no chunk count.
    pub const IMAGE_FILES: &str = "image_files";
    pub const ROOTS: &str = "roots";
    /// Files embedded so far by the current indexing job.
    pub const INDEXED_FILES: &str = "indexed_files";
    /// PauseIndexing was called and ResumeIndexing not since.
    pub const PAUSED: &str = "paused";
    pub const MODEL_PRESENT: &str = "model_present";
    pub const FETCHING: &str = "fetching";
    pub const FETCH_DONE: &str = "fetch_done";
    pub const FETCH_TOTAL: &str = "fetch_total";
    pub const FETCH_ERROR: &str = "fetch_error";
    /// Bytes on disk: the index database, the model, the compiled-model cache.
    pub const INDEX_BYTES: &str = "index_bytes";
    pub const MODEL_BYTES: &str = "model_bytes";
    pub const CACHE_BYTES: &str = "cache_bytes";
    /// Unix seconds: the last check of every root against the index, and the
    /// last indexing job that changed it. 0 for not since the runner started.
    pub const LAST_CHECKED: &str = "last_checked";
    pub const LAST_INDEXED: &str = "last_indexed";
    /// Why files are being indexed again after a start: a model changed
    /// (#80). Empty otherwise, and once that indexing is done.
    pub const MODEL_CHANGED: &str = "model_changed";
    /// Photos (HEIC, AVIF) skipped because no HEIF decoder is installed (#79).
    pub const NO_DECODER: &str = "no_decoder";
}
