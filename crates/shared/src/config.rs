//! `~/.config/vinosearch/config.toml`: settings shared by the CLI, the runner
//! and (later) the settings app, which edits this file rather than keeping
//! state of its own, through [`Config::save`].
//!
//! ```toml
//! exclude = ["~/Downloads", "~/Documents/tax"]
//! device = "NPU"
//! idle_unload_minutes = 5
//! index_busy_percent = 50
//! index_on_battery = true
//! editor = "code"
//! player = "mpv --start={seconds} {file}"
//!
//! [formats.text]
//! min_score = 0.65
//!
//! [formats.images]
//! enabled = true
//! extensions = ["jpg", "jpeg", "png", "heic"]
//! min_score = 0.68
//!
//! [formats.documents]
//! enabled = true
//!
//! [formats.code]
//! enabled = true
//! min_score = 0.72
//!
//! [formats.sessions]
//! enabled = true
//!
//! [formats.media]
//! enabled = true
//! max_minutes = 60
//!
//! [encryption]
//! enabled = true
//! ```
//!
//! A missing file is the defaults, and a missing `exclude` is
//! [`DEFAULT_EXCLUDE`]. Flags and environment variables override it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_DEVICE: &str = "NPU";
pub const DEFAULT_IDLE_UNLOAD_MINUTES: f64 = 5.0;
pub const DEFAULT_INDEX_BUSY_PERCENT: u8 = 50;
/// Big trees of downloaded or installed files in the home folder under names
/// too common to skip everywhere (`pkg`, `sdk`, `Games`): Snap, Go, Android SDK,
/// R and Perl libraries, game launchers.
pub const DEFAULT_EXCLUDE: &[&str] = &[
    "~/snap",
    "~/go/pkg",
    "~/sdk",
    "~/Android/Sdk",
    "~/R",
    "~/perl5",
    "~/Games",
];

/// Text results below this are left out. An off-topic query against a code
/// index topped out at 0.60, real matches 0.72 to 0.83. Not higher: at 0.66 the
/// evals' paraphrases fall from 0.909 recall@1 to 0.873.
pub const DEFAULT_TEXT_MIN_SCORE: f32 = 0.65;
/// Code results (`code:`) below this are left out. On ovfetch's source (251
/// chunks), the right function for 12 queries scored 0.76 to 0.92, the best match
/// for 4 unrelated ones 0.64 to 0.70.
pub const DEFAULT_CODE_MIN_SCORE: f32 = 0.72;
/// Images score lower against a text query. On Flickr30k's 1k test images, 0.60
/// showed images for 85% of off-topic queries and 0.63 for 38%, while 99.8% of
/// captions still found their image. On 153 home-folder images (2026-11-13),
/// off-topic words put photos at up to 0.677, right matches at 0.73 to 0.89.
pub const DEFAULT_IMAGE_MIN_SCORE: f32 = 0.68;
/// On the A/V spike, right windows scored 0.67 to 0.75, wrong ones up to 0.05
/// less. On 36 home-folder videos (2026-11-13), off-topic words put most at up to
/// 0.66 (two at 0.69, 0.70), videos named for the query at 0.68 to 0.82.
pub const DEFAULT_MEDIA_MIN_SCORE: f32 = 0.68;
/// Initial limit offered when skipping long audio and video is turned on.
pub const DEFAULT_MEDIA_MAX_MINUTES: f64 = 60.0;
/// Audio and video files, by extension. Not `ts`: TypeScript has it.
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "mpg", "mpeg", "wmv", "mp3", "m4a", "aac", "flac",
    "ogg", "oga", "opus", "wav", "wma",
];
/// HEIC, HEIF and AVIF need libheif's heif-convert installed; without
/// it they are counted and skipped.
pub const DEFAULT_IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff", "heic", "heif", "avif",
];
pub const HEIF_CONVERT_PACKAGES: &str =
    "libheif-tools on Fedora, libheif-examples on Debian and Ubuntu, libheif on Arch";

pub fn index_share(percent: u8) -> Result<f64> {
    if !(1..=100).contains(&percent) {
        bail!("index_busy_percent must be 1 to 100");
    }
    Ok(f64::from(percent) / 100.0)
}

pub fn idle_unload(minutes: f64) -> Result<Option<Duration>> {
    if !(minutes >= 0.0 && minutes.is_finite()) {
        bail!("idle_unload_minutes must be 0 or more");
    }
    Ok((minutes > 0.0).then(|| Duration::from_secs_f64(minutes * 60.0)))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Directories never indexed or watched, even under an indexed root. A
    /// leading `~/` is the home directory.
    pub exclude: Vec<String>,
    /// Device the model runs on [default: NPU].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Free the model after this many idle minutes; 0 keeps it loaded [default: 5].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_unload_minutes: Option<f64>,
    /// Most of the device's time background indexing may take, in percent;
    /// half that on battery [default: 50].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_busy_percent: Option<u8>,
    /// Index while on battery; false holds indexing until a charger is
    /// plugged in [default: true].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_on_battery: Option<bool>,
    /// What opens a text result at its line: an app's desktop entry
    /// id (`org.kde.kate.desktop`, as the settings app's picker saves it), a
    /// known editor (`code`, `kate`, `zed`, a JetBrains IDE...), or any
    /// command with `{file}` and `{line}` in it, split on spaces and run
    /// without a shell [default: the default app for the file's type].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// What plays a media result at its time: an app's desktop entry
    /// id, a known player (`mpv`, `vlc`, `celluloid`), or any command with
    /// `{file}` and `{seconds}` (whole seconds) in it, split on spaces and
    /// run without a shell [default: the default app for the file's type].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player: Option<String>,
    pub formats: Formats,
    pub encryption: Encryption,
}

/// SQLCipher encryption, the key sealed by `systemd-creds` to this TPM and user,
/// else kept in the desktop keyring.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Encryption {
    /// Encrypt the index. Turning it on or off converts the index as it is
    /// [default: false].
    pub enabled: Option<bool>,
    /// Without a TPM, seal the key with systemd's host key instead of
    /// refusing. Weaker: root, or anyone with a copy of the disk, can unseal
    /// it; it still keeps the index unreadable in a copy of the home folder
    /// alone [default: false].
    pub allow_host_key: Option<bool>,
}

impl Encryption {
    pub fn on(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    pub fn host_key_allowed(&self) -> bool {
        self.allow_host_key.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Formats {
    pub text: TextFormat,
    pub images: ImageFormat,
    pub documents: DocumentFormat,
    pub code: CodeFormat,
    pub sessions: SessionFormat,
    pub media: MediaFormat,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TextFormat {
    /// Results scoring below this are left out [default: 0.65].
    pub min_score: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImageFormat {
    /// Index images, by what's in them and the text OCR reads in them.
    /// Needs the vision encoder, a 671 MB download [default: false].
    pub enabled: Option<bool>,
    /// File extensions taken for images [default: DEFAULT_IMAGE_EXTENSIONS].
    pub extensions: Option<Vec<String>>,
    /// Results scoring below this are left out [default: 0.68].
    pub min_score: Option<f32>,
}

/// PDF, Office, OpenDocument, spreadsheets, CSV, EPUB, subtitles, `.doc`,
/// `.ppt` and RTF, searched with text so sharing its `min_score`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DocumentFormat {
    /// Index documents' text. Off by default: statements, tax forms and
    /// exports end up in the index, which is plaintext unless `encryption`
    /// is on [default: false].
    pub enabled: Option<bool>,
}

/// Source code, searched apart from everything else: only queries
/// starting `code:` find it, and they find nothing else. Files git ignores,
/// vendored and generated code, and code holding a key or token are left out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodeFormat {
    /// Index source code. Off by default: on a developer's home folder code
    /// is most of the index, and it's searched only with `code:` [default: false].
    pub enabled: Option<bool>,
    /// Results scoring below this are left out [default: 0.72].
    pub min_score: Option<f32>,
}

/// vinoWhisper sessions: `.vwsession` bundles (transcript and summary),
/// searched with text; also read from vinoWhisper's own folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionFormat {
    /// Index vinoWhisper sessions. Off by default: a transcript holds
    /// whatever was said or played, and the index keeps its text
    /// [default: false].
    pub enabled: Option<bool>,
}

/// Audio and video files, by their sound and picture: 10 s windows
/// of sound and a frame per shot, so "a dog barking" or "a whiteboard"
/// finds the moment. Needs ffmpeg, and the vision and audio encoders.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaFormat {
    /// Index audio and video. Off by default: an hour of film is minutes of
    /// the NPU's time, and the audio encoder is a 295 MB download, the
    /// vision encoder's 671 MB besides [default: false].
    pub enabled: Option<bool>,
    /// Results scoring below this are left out [default: 0.68].
    pub min_score: Option<f32>,
    /// Longer audio and video files are left out; 0 or unset means no limit
    /// [default: off].
    pub max_minutes: Option<f64>,
    /// Legacy alias for `max_minutes`, in hours; used only when `max_minutes`
    /// is unset.
    pub max_hours: Option<f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            exclude: DEFAULT_EXCLUDE.iter().map(|&e| e.to_owned()).collect(),
            device: None,
            idle_unload_minutes: None,
            index_busy_percent: None,
            index_on_battery: None,
            editor: None,
            player: None,
            formats: Formats::default(),
            encryption: Encryption::default(),
        }
    }
}

pub fn min_score(score: Option<f32>, default: f32) -> Result<f32> {
    let s = score.unwrap_or(default);
    if !(-1.0..=1.0).contains(&s) {
        bail!("min_score must be -1 to 1");
    }
    Ok(s)
}

impl Formats {
    pub fn images_enabled(&self) -> bool {
        self.images.enabled.unwrap_or(false)
    }

    pub fn documents_enabled(&self) -> bool {
        self.documents.enabled.unwrap_or(false)
    }

    pub fn code_enabled(&self) -> bool {
        self.code.enabled.unwrap_or(false)
    }

    pub fn sessions_enabled(&self) -> bool {
        self.sessions.enabled.unwrap_or(false)
    }

    pub fn media_enabled(&self) -> bool {
        self.media.enabled.unwrap_or(false)
    }

    pub fn media_min_score(&self) -> Result<f32> {
        min_score(self.media.min_score, DEFAULT_MEDIA_MIN_SCORE)
    }

    /// Configured duration limit for audio and video in minutes, or `None` when
    /// off. Legacy `max_hours` is converted to minutes when `max_minutes` is unset.
    pub fn media_max_minutes(&self) -> Option<f64> {
        if let Some(m) = self.media.max_minutes {
            return (m > 0.0 && m.is_finite()).then_some(m);
        }
        if let Some(h) = self.media.max_hours {
            return (h > 0.0 && h.is_finite()).then_some(f64::from(h) * 60.0);
        }
        None
    }

    pub fn media_max_seconds(&self) -> Result<Option<f64>> {
        if let Some(m) = self.media.max_minutes {
            if !(m >= 0.0 && m.is_finite()) {
                bail!("max_minutes must be 0 or more");
            }
            return Ok((m > 0.0).then_some(m * 60.0));
        }
        if let Some(h) = self.media.max_hours {
            if !(h >= 0.0 && h.is_finite()) {
                bail!("max_hours must be 0 or more");
            }
            return Ok((h > 0.0).then_some(f64::from(h) * 3600.0));
        }
        Ok(None)
    }

    pub fn code_min_score(&self) -> Result<f32> {
        min_score(self.code.min_score, DEFAULT_CODE_MIN_SCORE)
    }

    pub fn text_min_score(&self) -> Result<f32> {
        min_score(self.text.min_score, DEFAULT_TEXT_MIN_SCORE)
    }

    pub fn image_min_score(&self) -> Result<f32> {
        min_score(self.images.min_score, DEFAULT_IMAGE_MIN_SCORE)
    }

    /// Image extensions, lower case without a dot; empty while images are off.
    pub fn image_extensions(&self) -> Result<Vec<String>> {
        if !self.images_enabled() {
            return Ok(Vec::new());
        }
        let list = match &self.images.extensions {
            Some(l) => l.clone(),
            None => DEFAULT_IMAGE_EXTENSIONS
                .iter()
                .map(|&e| e.to_owned())
                .collect(),
        };
        list.iter()
            .map(|e| {
                let e = e.trim().trim_start_matches('.').to_ascii_lowercase();
                if e.is_empty() || !e.chars().all(|c| c.is_ascii_alphanumeric()) {
                    bail!("image extension {e:?}: use letters and digits, like \"jpg\"");
                }
                Ok(e)
            })
            .collect()
    }
}

pub fn default_path() -> Result<PathBuf> {
    Ok(crate::xdg::config_home()?.join("vinosearch/config.toml"))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        toml::from_str(&text).with_context(|| path.display().to_string())
    }

    /// Write these settings to `path`, keeping comments and layout. Written whole
    /// and renamed into place, so a runner re-reading never sees half.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        let mut doc: toml_edit::DocumentMut =
            text.parse().with_context(|| path.display().to_string())?;
        let mut exclude: toml_edit::Array = self.exclude.iter().map(String::as_str).collect();
        if !exclude.is_empty() {
            for e in exclude.iter_mut() {
                e.decor_mut().set_prefix("\n    ");
            }
            exclude.set_trailing_comma(true);
            exclude.set_trailing("\n");
        }
        match doc.get_mut("exclude").filter(|i| i.is_value()) {
            Some(old) => replace_keeping_comments(old, toml_edit::value(exclude)),
            None if self.exclude_is_the_default() => {}
            None => doc["exclude"] = toml_edit::value(exclude),
        }
        match &self.device {
            Some(d) => doc["device"] = toml_edit::value(d.as_str()),
            None => drop(doc.remove("device")),
        }
        match self.idle_unload_minutes {
            Some(m) => doc["idle_unload_minutes"] = minutes_as_written(m),
            None => drop(doc.remove("idle_unload_minutes")),
        }
        match self.index_busy_percent {
            Some(p) => doc["index_busy_percent"] = toml_edit::value(i64::from(p)),
            None => drop(doc.remove("index_busy_percent")),
        }
        match self.index_on_battery {
            Some(on) => doc["index_on_battery"] = toml_edit::value(on),
            None => drop(doc.remove("index_on_battery")),
        }
        match self.editor.as_deref().map(str::trim) {
            Some(e) if !e.is_empty() => doc["editor"] = toml_edit::value(e),
            _ => drop(doc.remove("editor")),
        }
        match self.player.as_deref().map(str::trim) {
            Some(p) if !p.is_empty() => doc["player"] = toml_edit::value(p),
            _ => drop(doc.remove("player")),
        }
        let f = &self.formats;
        set(
            &mut doc,
            "text",
            "min_score",
            f.text.min_score.map(shortest_decimal),
        );
        set(
            &mut doc,
            "images",
            "enabled",
            f.images.enabled.map(toml_edit::value),
        );
        set(
            &mut doc,
            "images",
            "extensions",
            f.images.extensions.as_ref().map(|l| {
                toml_edit::value(l.iter().map(String::as_str).collect::<toml_edit::Array>())
            }),
        );
        set(
            &mut doc,
            "images",
            "min_score",
            f.images.min_score.map(shortest_decimal),
        );
        set(
            &mut doc,
            "documents",
            "enabled",
            f.documents.enabled.map(toml_edit::value),
        );
        set(
            &mut doc,
            "code",
            "enabled",
            f.code.enabled.map(toml_edit::value),
        );
        set(
            &mut doc,
            "code",
            "min_score",
            f.code.min_score.map(shortest_decimal),
        );
        set(
            &mut doc,
            "sessions",
            "enabled",
            f.sessions.enabled.map(toml_edit::value),
        );
        set(
            &mut doc,
            "media",
            "enabled",
            f.media.enabled.map(toml_edit::value),
        );
        set(
            &mut doc,
            "media",
            "min_score",
            f.media.min_score.map(shortest_decimal),
        );
        set(
            &mut doc,
            "media",
            "max_minutes",
            f.media.max_minutes.map(minutes_as_written),
        );
        set(
            &mut doc,
            "media",
            "max_hours",
            f.media.max_hours.map(shortest_decimal),
        );
        let e = &self.encryption;
        set_table(
            &mut doc,
            "encryption",
            "enabled",
            e.enabled.map(toml_edit::value),
        );
        set_table(
            &mut doc,
            "encryption",
            "allow_host_key",
            e.allow_host_key.map(toml_edit::value),
        );
        let dir = path.parent().context("config path has no directory")?;
        fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
        let tmp = tmp_path_of_this_save(path);
        let saved = fs::write(&tmp, doc.to_string())
            .with_context(|| tmp.display().to_string())
            .and_then(|()| fs::rename(&tmp, path).with_context(|| path.display().to_string()));
        if saved.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        saved
    }

    /// An `exclude` left unwritten while it is the defaults takes a later
    /// release's defaults (pinned by
    /// `tests::a_missing_exclude_is_the_default_list_until_changed`).
    fn exclude_is_the_default(&self) -> bool {
        self.exclude == Config::default().exclude
    }

    pub fn excludes(&self) -> Result<Excludes> {
        let home = crate::xdg::home()?;
        self.exclude
            .iter()
            .map(|e| resolve(e, &home))
            .collect::<Result<_>>()
            .map(Excludes)
    }

    pub fn index_on_battery(&self) -> bool {
        self.index_on_battery.unwrap_or(true)
    }
}

/// A name of its own per save, so two saves at once don't rename each other's
/// file away (pinned by
fn tmp_path_of_this_save(path: &Path) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("toml.{}-{n}.tmp", std::process::id()))
}

/// Whole minutes stay integers, as people write them (pinned by
/// `tests::whole_idle_minutes_are_saved_as_an_integer` and
/// `tests::part_minutes_are_saved_as_a_float`).
fn minutes_as_written(m: f64) -> toml_edit::Item {
    if m.fract() == 0.0 && m.abs() < i64::MAX as f64 {
        toml_edit::value(m as i64)
    } else {
        toml_edit::value(m)
    }
}

/// `0.7` saved as `0.7`, not `0.699999988` (pinned by
/// `tests::saving_formats_keeps_comments_and_drops_empty_tables`).
fn shortest_decimal(s: f32) -> toml_edit::Item {
    toml_edit::value(s.to_string().parse::<f64>().unwrap_or(f64::from(s)))
}

/// Assigning the value alone keeps the comments around its key (pinned by
/// `tests::saving_keeps_comments_and_round_trips`).
fn replace_keeping_comments(old: &mut toml_edit::Item, new: toml_edit::Item) {
    let decor = old.as_value().unwrap().decor().clone();
    *old = new;
    *old.as_value_mut().unwrap().decor_mut() = decor;
}

fn set_table(
    doc: &mut toml_edit::DocumentMut,
    table: &str,
    key: &str,
    value: Option<toml_edit::Item>,
) {
    let Some(value) = value else {
        if let Some(t) = doc.get_mut(table).and_then(|i| i.as_table_like_mut()) {
            t.remove(key);
            if t.is_empty() {
                doc.remove(table);
            }
        }
        return;
    };
    let t = doc
        .entry(table)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .expect("a table");
    match t.get_mut(key) {
        Some(old) if old.is_value() => replace_keeping_comments(old, value),
        _ => {
            t.insert(key, value);
        }
    }
}

fn set(doc: &mut toml_edit::DocumentMut, format: &str, key: &str, value: Option<toml_edit::Item>) {
    let Some(value) = value else {
        if let Some(formats) = doc.get_mut("formats").and_then(|i| i.as_table_like_mut()) {
            if let Some(t) = formats.get_mut(format).and_then(|i| i.as_table_like_mut()) {
                t.remove(key);
                if t.is_empty() {
                    formats.remove(format);
                }
            }
            if formats.is_empty() {
                doc.remove("formats");
            }
        }
        return;
    };
    let formats = doc.entry("formats").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    });
    let t = formats
        .as_table_like_mut()
        .expect("formats is a table")
        .entry(format)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    match t
        .as_table_like_mut()
        .expect("a format is a table")
        .get_mut(key)
    {
        Some(old) if old.is_value() => replace_keeping_comments(old, value),
        _ => {
            t.as_table_like_mut().unwrap().insert(key, value);
        }
    }
}

/// `~/x` against `home`, absolute and free of `.`/`..`, its longest existing
/// ancestor canonicalized so it equals the canonical paths indexing records, even
/// once a missing folder is created (on Silverblue `/home` is a symlink to
/// `/var/home`).
fn resolve(entry: &str, home: &Path) -> Result<PathBuf> {
    let raw = match entry.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if entry == "~" => home.to_path_buf(),
        None => PathBuf::from(entry),
    };
    if !raw.is_absolute() {
        bail!("exclude {entry:?}: use an absolute path or one starting with ~/");
    }
    let mut clean = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            c => clean.push(c),
        }
    }
    Ok(canonical_prefix(&clean))
}

fn canonical_prefix(path: &Path) -> PathBuf {
    for base in path.ancestors() {
        if let Ok(canon) = fs::canonicalize(base) {
            let rest = path.strip_prefix(base).expect("an ancestor is a prefix");
            return if rest.as_os_str().is_empty() {
                canon
            } else {
                canon.join(rest)
            };
        }
    }
    path.to_path_buf()
}

/// Resolved exclusions. Matching is by whole path components, so excluding
/// `~/Down` does not exclude `~/Downloads`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Excludes(Vec<PathBuf>);

impl Excludes {
    pub fn contains(&self, path: &Path) -> bool {
        self.0.iter().any(|e| path.starts_with(e))
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_defaults() {
        let c: Config =
            toml::from_str("exclude = [\"~/Downloads\"]\nidle_unload_minutes = 0\n").unwrap();
        assert_eq!(c.exclude, ["~/Downloads"]);
        assert_eq!(c.idle_unload_minutes, Some(0.0));
        assert_eq!(c.device, None);
        assert_eq!(c.index_busy_percent, None);
        assert!(c.index_on_battery());
        let c: Config = toml::from_str("index_on_battery = false\n").unwrap();
        assert!(!c.index_on_battery());
        let c: Config = toml::from_str("index_busy_percent = 25\n").unwrap();
        assert_eq!(index_share(c.index_busy_percent.unwrap()).unwrap(), 0.25);
        assert!(index_share(0).is_err());
        assert!(index_share(101).is_err());
        assert_eq!(toml::from_str::<Config>("").unwrap(), Config::default());
        assert!(toml::from_str::<Config>("exclud = []").is_err());
    }

    #[test]
    fn excludes_match_whole_components() {
        let home = Path::new("/nonexistent-home");
        let ex = Excludes(vec![resolve("~/Downloads", home).unwrap()]);
        assert!(ex.contains(Path::new("/nonexistent-home/Downloads")));
        assert!(ex.contains(Path::new("/nonexistent-home/Downloads/tax/a.md")));
        assert!(!ex.contains(Path::new("/nonexistent-home/Downloads2/a.md")));
        assert!(!ex.contains(Path::new("/nonexistent-home/Development")));
    }

    #[test]
    fn resolves_tilde_and_dots() {
        let home = Path::new("/nonexistent-home");
        assert_eq!(
            resolve("~/a/../b/./c", home).unwrap(),
            Path::new("/nonexistent-home/b/c")
        );
        assert_eq!(resolve("~", home).unwrap(), home);
        assert!(resolve("relative/dir", home).is_err());
    }

    #[test]
    fn a_missing_exclusion_resolves_through_a_symlinked_home() {
        let dir = std::env::temp_dir().join(format!("vinosearch-cfg-{}", std::process::id()));
        let real = dir.join("var/home/u");
        fs::create_dir_all(&real).unwrap();
        let link = dir.join("home");
        std::os::unix::fs::symlink(dir.join("var/home"), &link).unwrap();
        let real = fs::canonicalize(&real).unwrap();
        let ex = resolve("~/Documents/tax", &link.join("u")).unwrap();
        assert_eq!(
            ex,
            real.join("Documents/tax"),
            "written through the symlink before Documents/tax exists"
        );
        fs::create_dir_all(real.join("Documents/tax")).unwrap();
        let indexed = fs::canonicalize(link.join("u/Documents/tax")).unwrap();
        assert!(Excludes(vec![ex]).contains(&indexed.join("2025.md")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn idle_unload_validates() {
        assert_eq!(idle_unload(0.0).unwrap(), None);
        assert_eq!(idle_unload(0.5).unwrap(), Some(Duration::from_secs(30)));
        assert!(idle_unload(-1.0).is_err());
        assert!(idle_unload(f64::NAN).is_err());
        assert!(idle_unload(f64::INFINITY).is_err());
    }

    #[test]
    fn saving_keeps_comments_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("vinosearch-config-{}", std::process::id()));
        let path = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &path,
            "# Never indexed or watched.\nexclude = [\"~/a\"]\ndevice = \"CPU\" # for now\n",
        )
        .unwrap();
        let mut c = Config::load(&path).unwrap();
        c.exclude.push("~/b".into());
        c.device = None;
        c.idle_unload_minutes = Some(0.0);
        c.index_on_battery = Some(false);
        c.editor = Some("foot -e nvim +{line} {file}".into());
        c.player = Some("mpv --start={seconds} {file}".into());
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("index_on_battery = false\n"), "{text}");
        assert!(
            text.starts_with("# Never indexed or watched.\nexclude = [\n"),
            "{text}"
        );
        assert!(!text.contains("device"), "{text}");
        assert!(text.contains("idle_unload_minutes = 0\n"), "{text}");
        assert!(
            text.contains("player = \"mpv --start={seconds} {file}\"\n"),
            "{text}"
        );
        assert_eq!(Config::load(&path).unwrap(), c);

        c.exclude.clear();
        c.idle_unload_minutes = Some(2.5);
        c.index_on_battery = None;
        c.save(&path).unwrap();
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("index_on_battery")
        );
        c.editor = None;
        c.player = Some("  ".into());
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("editor") && !text.contains("player"),
            "{text}"
        );
        c.player = None;
        assert_eq!(Config::load(&path).unwrap(), c);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn saved_text(c: &Config, name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("vinosearch-{name}-{}", std::process::id()));
        let path = dir.join("config.toml");
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        text
    }

    #[test]
    fn whole_idle_minutes_are_saved_as_an_integer() {
        let c = Config {
            idle_unload_minutes: Some(3.0),
            ..Config::default()
        };
        let text = saved_text(&c, "whole-minutes");
        assert!(text.contains("idle_unload_minutes = 3\n"), "{text}");
    }

    #[test]
    fn part_minutes_are_saved_as_a_float() {
        let c = Config {
            idle_unload_minutes: Some(2.5),
            ..Config::default()
        };
        let text = saved_text(&c, "part-minutes");
        assert!(text.contains("idle_unload_minutes = 2.5\n"), "{text}");
    }

    #[test]
    fn saving_creates_the_file() {
        let dir = std::env::temp_dir().join(format!("vinosearch-new-{}", std::process::id()));
        let path = dir.join("sub/config.toml");
        let c = Config {
            exclude: vec!["~/Downloads".into()],
            ..Config::default()
        };
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_exclude_is_the_default_list_until_changed() {
        let c: Config = toml::from_str("device = \"CPU\"\n").unwrap();
        assert_eq!(c.exclude, DEFAULT_EXCLUDE);
        let c: Config = toml::from_str("exclude = []\n").unwrap();
        assert!(c.exclude.is_empty());

        let dir = std::env::temp_dir().join(format!("vinosearch-dflt-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut c = Config {
            device: Some("CPU".into()),
            ..Config::default()
        };
        c.save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("exclude"));
        c.exclude.retain(|e| e != "~/Games");
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn formats_default_to_text_only() {
        let c = Config::default();
        assert!(c.formats.image_extensions().unwrap().is_empty());
        assert_eq!(c.formats.text_min_score().unwrap(), DEFAULT_TEXT_MIN_SCORE);
        let c: Config = toml::from_str("[formats.images]\nenabled = true\n").unwrap();
        assert_eq!(
            c.formats.image_extensions().unwrap(),
            DEFAULT_IMAGE_EXTENSIONS
        );
        let c: Config =
            toml::from_str("[formats.images]\nenabled = true\nextensions = [\".HEIC\", \"jpg\"]\n")
                .unwrap();
        assert_eq!(c.formats.image_extensions().unwrap(), ["heic", "jpg"]);
        let c: Config =
            toml::from_str("[formats.images]\nenabled = true\nextensions = [\"a/b\"]\n").unwrap();
        assert!(c.formats.image_extensions().is_err());
        let c: Config = toml::from_str("[formats.text]\nmin_score = 2\n").unwrap();
        assert!(c.formats.text_min_score().is_err());
    }

    #[test]
    fn text_cant_be_turned_off() {
        assert!(toml::from_str::<Config>("[formats.text]\nenabled = false\n").is_err());
    }

    #[test]
    fn code_is_opt_in() {
        assert!(!Config::default().formats.code_enabled());
        assert_eq!(
            Config::default().formats.code_min_score().unwrap(),
            DEFAULT_CODE_MIN_SCORE
        );
        let c: Config =
            toml::from_str("[formats.code]\nenabled = true\nmin_score = 0.7\n").unwrap();
        assert!(c.formats.code_enabled());
        assert_eq!(c.formats.code_min_score().unwrap(), 0.7);
        let c: Config = toml::from_str("[formats.code]\nmin_score = -2\n").unwrap();
        assert!(c.formats.code_min_score().is_err());
    }

    #[test]
    fn sessions_are_opt_in() {
        assert!(!Config::default().formats.sessions_enabled());
        let c: Config = toml::from_str("[formats.sessions]\nenabled = true\n").unwrap();
        assert!(c.formats.sessions_enabled());
    }

    #[test]
    fn media_is_opt_in_and_duration_limit_defaults_to_off() {
        let d = Config::default().formats;
        assert!(!d.media_enabled());
        assert_eq!(d.media_max_minutes(), None);
        assert_eq!(d.media_max_seconds().unwrap(), None);

        let c: Config =
            toml::from_str("[formats.media]\nenabled = true\nmax_minutes = 45\nmin_score = 0.7\n")
                .unwrap();
        assert!(c.formats.media_enabled());
        assert_eq!(c.formats.media_max_minutes(), Some(45.0));
        assert_eq!(c.formats.media_max_seconds().unwrap(), Some(2700.0));
        assert_eq!(c.formats.media_min_score().unwrap(), 0.7);

        let zero: Config = toml::from_str("[formats.media]\nmax_minutes = 0\n").unwrap();
        assert_eq!(zero.formats.media_max_minutes(), None);
        assert_eq!(zero.formats.media_max_seconds().unwrap(), None);

        let legacy: Config = toml::from_str("[formats.media]\nmax_hours = 0.5\n").unwrap();
        assert_eq!(legacy.formats.media_max_minutes(), Some(30.0));
        assert_eq!(legacy.formats.media_max_seconds().unwrap(), Some(1800.0));

        for bad in ["-1", "nan", "inf"] {
            let c: Config =
                toml::from_str(&format!("[formats.media]\nmax_minutes = {bad}\n")).unwrap();
            assert!(c.formats.media_max_seconds().is_err(), "{bad}");
            let c: Config =
                toml::from_str(&format!("[formats.media]\nmax_hours = {bad}\n")).unwrap();
            assert!(c.formats.media_max_seconds().is_err(), "{bad}");
        }
    }

    #[test]
    fn saves_at_once_dont_trip_over_each_other() {
        let dir = std::env::temp_dir().join(format!("vinosearch-race-{}", std::process::id()));
        let path = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        let c = Config::default();
        std::thread::scope(|s| {
            let saves: Vec<_> = (0..8).map(|_| s.spawn(|| c.save(&path))).collect();
            for save in saves {
                save.join().unwrap().unwrap();
            }
        });
        assert_eq!(Config::load(&path).unwrap(), c);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "a temp file left behind"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_formats_keeps_comments_and_drops_empty_tables() {
        let dir = std::env::temp_dir().join(format!("vinosearch-fmt-{}", std::process::id()));
        let path = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &path,
            "device = \"NPU\"\n\n[formats.images]\nenabled = false # big download\n",
        )
        .unwrap();
        let mut c = Config::load(&path).unwrap();
        c.formats.images.enabled = Some(true);
        c.formats.text.min_score = Some(0.7);
        c.formats.documents.enabled = Some(true);
        c.formats.code.enabled = Some(true);
        c.formats.code.min_score = Some(0.6);
        c.formats.sessions.enabled = Some(true);
        c.formats.media.enabled = Some(true);
        c.formats.media.max_minutes = Some(90.0);
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("enabled = true # big download"), "{text}");
        assert!(text.contains("min_score = 0.7\n"), "{text}");
        assert!(
            text.contains("[formats.documents]\nenabled = true"),
            "{text}"
        );
        assert!(
            text.contains("[formats.code]\nenabled = true\nmin_score = 0.6"),
            "{text}"
        );
        assert!(
            text.contains("[formats.sessions]\nenabled = true"),
            "{text}"
        );
        assert!(
            text.contains("[formats.media]\nenabled = true\nmax_minutes = 90"),
            "{text}"
        );
        assert_eq!(Config::load(&path).unwrap(), c);
        c.formats = Formats::default();
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("formats"), "{text}");
        assert_eq!(Config::load(&path).unwrap(), c);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encryption_is_off_by_default_and_saves_in_its_own_table() {
        let c = Config::default();
        assert!(!c.encryption.on());
        assert!(!c.encryption.host_key_allowed());
        let c: Config = toml::from_str("[encryption]\nenabled = true\n").unwrap();
        assert!(c.encryption.on());
        assert!(toml::from_str::<Config>("[encryption]\nkey = \"tpm\"\n").is_err());

        let dir = std::env::temp_dir().join(format!("vinosearch-enc-{}", std::process::id()));
        let path = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "[encryption]\nenabled = false # not yet\n").unwrap();
        let mut c = Config::load(&path).unwrap();
        c.encryption.enabled = Some(true);
        c.encryption.allow_host_key = Some(true);
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("enabled = true # not yet"), "{text}");
        assert!(text.contains("allow_host_key = true"), "{text}");
        assert_eq!(Config::load(&path).unwrap(), c);
        c.encryption = Encryption::default();
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("encryption"), "{text}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let c = Config::load(Path::new("/nonexistent-dir/config.toml")).unwrap();
        assert_eq!(c, Config::default());
    }
}
