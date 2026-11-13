//! Names on the session bus. See `docs/dbus.md` for the methods.

/// The names vinoSearch goes by on the bus and in the desktop's files. A
/// native install's are fixed; the Flatpak's start with its app id, as
/// Flatpak exports only such files and lets an app own only such names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    /// Well-known name the runner owns; D-Bus activation starts it on first
    /// use.
    pub service: String,
    /// The desktop entry GNOME Shell shows the provider's results under. It
    /// also launches the settings window, whose Wayland app id is `app_id`,
    /// so the window gets the entry's icon.
    pub desktop_id: String,
    pub app_id: String,
    /// Icon name, installed under hicolor/scalable/apps.
    pub icon: String,
    /// KRunner's plugin file, under the XDG data directory. Its presence is
    /// the KDE search place being on (#45).
    pub krunner_plugin: String,
    /// The plugin's id in KRunner's settings.
    pub krunner_plugin_id: String,
    /// GNOME Shell's search provider file, under the XDG data directory. A
    /// native install's is in a directory of ours that environment.d adds to
    /// XDG_DATA_DIRS, and its presence is the GNOME search place being on.
    pub gnome_provider: String,
    /// The app menu entry for the search window, under the XDG data
    /// directory. Its presence is the menu place being on; off by default.
    /// None in the Flatpak, whose launcher has the search window as an
    /// action instead.
    pub search_entry: Option<String>,
    /// The D-Bus activation file, under the XDG data directory: the service
    /// is installed while it exists.
    pub dbus_service_file: String,
}

impl Names {
    pub fn native() -> Names {
        Names {
            service: "dev.vinosearch.Runner".into(),
            desktop_id: "dev.vinosearch.desktop".into(),
            app_id: "dev.vinosearch".into(),
            icon: "dev.vinosearch".into(),
            krunner_plugin: "krunner/dbusplugins/vinosearch.desktop".into(),
            krunner_plugin_id: "vinosearch".into(),
            gnome_provider:
                "vinosearch/xdg/gnome-shell/search-providers/dev.vinosearch.search-provider.ini"
                    .into(),
            search_entry: Some("applications/dev.vinosearch.search.desktop".into()),
            dbus_service_file: "dbus-1/services/dev.vinosearch.Runner.service".into(),
        }
    }

    /// The Flatpak with app id `app`.
    pub fn flatpak(app: &str) -> Names {
        Names {
            service: format!("{app}.Runner"),
            desktop_id: format!("{app}.desktop"),
            app_id: app.into(),
            icon: app.into(),
            krunner_plugin: format!("krunner/dbusplugins/{app}.desktop"),
            krunner_plugin_id: app.into(),
            gnome_provider: format!("gnome-shell/search-providers/{app}.search-provider.ini"),
            search_entry: None,
            dbus_service_file: format!("dbus-1/services/{app}.Runner.service"),
        }
    }
}

/// The names this process goes by: the Flatpak's inside its sandbox.
pub fn names() -> &'static Names {
    static NAMES: std::sync::OnceLock<Names> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| match crate::sandbox::flatpak_id() {
        Some(app) => Names::flatpak(app),
        None => Names::native(),
    })
}

/// KRunner's interface (`org.kde.krunner1`) lives here.
pub const RUNNER_PATH: &str = "/runner";
/// GNOME Shell's org.gnome.Shell.SearchProvider2 lives here.
pub const GNOME_PATH: &str = "/gnome";
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
    /// Source code (#110).
    pub const CODE_FILES: &str = "code_files";
    pub const CODE_CHUNKS: &str = "code_chunks";
    /// vinoWhisper sessions (#143), and those not read: in a format this
    /// build doesn't know, or broken.
    pub const SESSION_FILES: &str = "session_files";
    pub const SESSION_CHUNKS: &str = "session_chunks";
    pub const SESSIONS_UNREAD: &str = "sessions_unread";
    /// Audio and video files (#187), and their sound windows and frames.
    pub const MEDIA_FILES: &str = "media_files";
    pub const MEDIA_CHUNKS: &str = "media_chunks";
    /// What images (the vision encoder and OCR) and media (the vision and
    /// audio encoders) need is downloaded, whether or not they're on.
    pub const IMAGES_MODEL_PRESENT: &str = "images_model_present";
    pub const MEDIA_MODEL_PRESENT: &str = "media_model_present";
    /// ffmpeg and ffprobe are on the service's PATH; media needs both.
    pub const FFMPEG: &str = "ffmpeg";
    pub const ROOTS: &str = "roots";
    /// Files embedded so far by the current run of indexing.
    pub const INDEXED_FILES: &str = "indexed_files";
    /// The indexing job is the rest of a long document (#108).
    pub const INDEXING_REST: &str = "indexing_rest";
    /// Documents indexed in part so far (#108), next in line first, at most
    /// PARTIAL_MAX: `a(sstt)` of (path, unit, parts in, parts in all), the
    /// unit "page", "slide", "chapter" or "part".
    pub const PARTIAL: &str = "partial";
    pub const PARTIAL_MAX: usize = 100;
    /// PauseIndexing was called and ResumeIndexing not since.
    pub const PAUSED: &str = "paused";
    /// Why indexing is held for power (#109): a `hold` value, or empty.
    pub const POWER_HOLD: &str = "power_hold";
    pub const MODEL_PRESENT: &str = "model_present";
    pub const FETCHING: &str = "fetching";
    pub const FETCH_DONE: &str = "fetch_done";
    pub const FETCH_TOTAL: &str = "fetch_total";
    pub const FETCH_ERROR: &str = "fetch_error";
    /// Bytes on disk: the index database, every model (text, vision, OCR and
    /// audio), the compiled-model cache.
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
    /// The index on disk is encrypted (#4).
    pub const ENCRYPTED: &str = "encrypted";
    /// What its key is sealed with: a `seal` value, empty while it isn't
    /// encrypted.
    pub const SEAL: &str = "seal";
    /// systemd can seal a key to this machine's TPM.
    pub const TPM: &str = "tpm";
    /// Why the last change to encryption was refused or failed; empty
    /// otherwise.
    pub const ENCRYPTION_ERROR: &str = "encryption_error";
    /// Why this start found the index's key unusable and rebuilt the index
    /// from the files; empty otherwise.
    pub const SEAL_BROKEN: &str = "seal_broken";
    /// Why the index's key can't be had yet (the keyring is locked, its
    /// unlock prompt was dismissed or went unanswered, no Secret Service);
    /// empty otherwise. The index is kept, searches find nothing meanwhile,
    /// and the runner looks again every 30 s, or with a prompt on
    /// `ApplyEncryption`.
    pub const SEAL_WAITING: &str = "seal_waiting";
    /// What a new key is sealed with here, a `seal` value: `tpm` or `host`
    /// while `systemd-creds` works, else `keyring`.
    pub const NEW_SEAL: &str = "new_seal";
}

/// Values of `status::SEAL`.
pub mod seal {
    /// This machine's TPM, with systemd's host key.
    pub const TPM: &str = "tpm";
    /// systemd's host key alone (`/var/lib/systemd/credential.secret`).
    pub const HOST: &str = "host";
    /// The desktop keyring (Secret Service), where `systemd-creds` can't be
    /// used: unlocked at login, readable by the user's session, no TPM.
    pub const KEYRING: &str = "keyring";
    /// Nothing could seal it: a key held in memory only, so the index is
    /// rebuilt at the next start.
    pub const MEMORY: &str = "memory";
}

/// Values of `status::POWER_HOLD`.
pub mod hold {
    /// On battery with `index_on_battery = false`.
    pub const CHARGER: &str = "charger";
    /// power-profiles-daemon (or tuned-ppd) has the power-saver profile on.
    pub const POWER_SAVER: &str = "power_saver";
}

/// `Control1.RecentFiles`: the kinds it returns, and the most it returns.
pub mod recent {
    pub const TEXT: &str = "text";
    pub const DOCUMENT: &str = "document";
    pub const MAX: u32 = 50;
}

/// `Control1.Search`: the kinds a result is, and the most files of each kind
/// it returns. Text read in an image (#106) is an `IMAGE`, as KRunner lists it.
pub mod results {
    pub const TEXT: &str = "text";
    pub const DOCUMENT: &str = "document";
    pub const IMAGE: &str = "image";
    /// Source code, only ever alone, for a query starting `code:` (#110).
    pub const CODE: &str = "code";
    /// A vinoWhisper session's transcript or summary (#143).
    pub const SESSION: &str = "session";
    /// An audio or video file, by its sound or picture (#187).
    pub const MEDIA: &str = "media";
    pub const MAX: u32 = 100;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_flatpak_name_starts_with_the_app_id() {
        let app = "dev.vinosearch.VinoSearch";
        let n = Names::flatpak(app);
        let file = |p: &str| p.rsplit('/').next().unwrap().to_owned();
        for name in [
            n.service.clone(),
            n.desktop_id.clone(),
            n.app_id.clone(),
            n.icon.clone(),
            file(&n.krunner_plugin),
            file(&n.gnome_provider),
            file(&n.dbus_service_file),
        ] {
            assert!(name.starts_with(app), "{name}");
        }
        // The activation file is named after the bus name it starts.
        assert_eq!(file(&n.dbus_service_file), format!("{}.service", n.service));
        assert_ne!(n.service, Names::native().service);
    }
}
