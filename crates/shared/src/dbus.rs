//! Names on the session bus. See `docs/dbus.md` for the methods.

/// Well-known name the runner owns; D-Bus activation starts it on first use.
pub const SERVICE: &str = "dev.vinosearch.Runner";
/// KRunner's interface (`org.kde.krunner1`) lives here.
pub const RUNNER_PATH: &str = "/runner";
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
}
