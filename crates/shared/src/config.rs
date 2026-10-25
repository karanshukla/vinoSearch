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
//!
//! [formats.text]
//! min_score = 0.65
//!
//! [formats.images]
//! enabled = true
//! extensions = ["jpg", "jpeg", "png", "heic"]
//! min_score = 0.63
//!
//! [formats.documents]
//! enabled = true
//!
//! [formats.code]
//! enabled = true
//! min_score = 0.72
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
/// Big trees of downloaded or installed files that live in the home folder
/// under names too common to skip everywhere (`pkg`, `sdk`, `Games`).
pub const DEFAULT_EXCLUDE: &[&str] = &[
    // Snap packages' data and revisions
    "~/snap",
    // Go's module and build cache, and the Go SDKs `go install` fetches
    "~/go/pkg",
    "~/sdk",
    "~/Android/Sdk",
    // R and Perl libraries installed per user
    "~/R",
    "~/perl5",
    // Heroic, Lutris and others install games here
    "~/Games",
];

/// Text results scoring below this are left out. An off-topic query against a
/// code index topped out at 0.60; real matches scored 0.72 to 0.83.
pub const DEFAULT_TEXT_MIN_SCORE: f32 = 0.65;
/// Code results (queries starting `code:`, #110) scoring below this are
/// left out. The code retrieval prompt scores higher than the search one:
/// on ovfetch's source (251 chunks), the right function for 12 queries
/// scored 0.76 to 0.92 and the best match for 4 unrelated ones 0.64 to 0.70.
pub const DEFAULT_CODE_MIN_SCORE: f32 = 0.72;
/// Images score lower against a text query than text does. On Flickr30k's 1k
/// test images, 0.60 showed images for 85% of off-topic queries ("bills",
/// "tax return") and 0.63 for 38%, while 99.8% of captions still found their
/// own image. On ~/Pictures, right matches scored from 0.61, so not higher yet.
pub const DEFAULT_IMAGE_MIN_SCORE: f32 = 0.63;
/// HEIC, HEIF and AVIF need libheif's heif-convert installed (#79); without
/// it they are counted and skipped.
pub const DEFAULT_IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff", "heic", "heif", "avif",
];
/// Where heif-convert comes from, for the messages that say to install it.
pub const HEIF_CONVERT_PACKAGES: &str =
    "libheif-tools on Fedora, libheif-examples on Debian and Ubuntu, libheif on Arch";

/// `index_busy_percent` as a share of the device's time, in (0, 1].
pub fn index_share(percent: u8) -> Result<f64> {
    if !(1..=100).contains(&percent) {
        bail!("index_busy_percent must be 1 to 100");
    }
    Ok(f64::from(percent) / 100.0)
}

/// How long the model may sit idle before it is freed; None keeps it loaded.
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
    /// plugged in (#109) [default: true].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_on_battery: Option<bool>,
    /// What opens a text result at its line (#105): a known editor (`code`,
    /// `kate`, `zed`, a JetBrains IDE...), or any command with `{file}` and
    /// `{line}` in it, split on spaces and run without a shell
    /// [default: the default app for the file's type].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// What kinds of file are indexed, and how close a result must be (#73).
    pub formats: Formats,
    /// Encrypting the index at rest (#4).
    pub encryption: Encryption,
}

/// The index encrypted with SQLCipher, its key sealed by `systemd-creds` to
/// this machine's TPM and this user.
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
}

/// Text is always indexed for now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TextFormat {
    /// Results scoring below this are left out [default: 0.65].
    pub min_score: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImageFormat {
    /// Index images, by what's in them and the text OCR reads in them (#106).
    /// Needs the vision encoder, a 671 MB download [default: false].
    pub enabled: Option<bool>,
    /// File extensions taken for images [default: DEFAULT_IMAGE_EXTENSIONS].
    pub extensions: Option<Vec<String>>,
    /// Results scoring below this are left out [default: 0.63].
    pub min_score: Option<f32>,
}

/// PDF, Office and OpenDocument files, spreadsheets, CSV (#55), EPUB,
/// subtitles, `.doc`, `.ppt` and RTF (#97). Searched
/// with text, so they share its `min_score`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DocumentFormat {
    /// Index documents' text. Off by default: statements, tax forms and
    /// exports end up in the index, which is plaintext unless `encryption`
    /// is on (#4) [default: false].
    pub enabled: Option<bool>,
}

/// Source code (#110), searched apart from everything else: only queries
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

impl Default for Config {
    fn default() -> Self {
        Self {
            exclude: DEFAULT_EXCLUDE.iter().map(|&e| e.to_owned()).collect(),
            device: None,
            idle_unload_minutes: None,
            index_busy_percent: None,
            index_on_battery: None,
            editor: None,
            formats: Formats::default(),
            encryption: Encryption::default(),
        }
    }
}

/// A `min_score`, checked: a cosine, so -1 to 1.
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

    pub fn code_min_score(&self) -> Result<f32> {
        min_score(self.code.min_score, DEFAULT_CODE_MIN_SCORE)
    }

    pub fn text_min_score(&self) -> Result<f32> {
        min_score(self.text.min_score, DEFAULT_TEXT_MIN_SCORE)
    }

    pub fn image_min_score(&self) -> Result<f32> {
        min_score(self.images.min_score, DEFAULT_IMAGE_MIN_SCORE)
    }

    /// The extensions images are indexed by, lower case and without a dot;
    /// empty while images are off.
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

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("no HOME")?))
}

pub fn default_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home()?.join(".config"),
    };
    Ok(base.join("vinosearch/config.toml"))
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

    /// Write these settings to `path`, keeping the comments and layout of what
    /// is already there. Written whole and renamed into place, so a runner
    /// re-reading the file never sees half of it.
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
        match doc.get_mut("exclude").and_then(|i| i.as_value_mut()) {
            // Assigning the value alone keeps the comments around the key.
            Some(v) => {
                let decor = v.decor().clone();
                *v = exclude.into();
                *v.decor_mut() = decor;
            }
            // Left unwritten while it is the defaults, so a later release's defaults reach it.
            None if self.exclude == Config::default().exclude => {}
            None => doc["exclude"] = toml_edit::value(exclude),
        }
        match &self.device {
            Some(d) => doc["device"] = toml_edit::value(d.as_str()),
            None => drop(doc.remove("device")),
        }
        match self.idle_unload_minutes {
            // Whole minutes stay integers, as people write them.
            Some(m) if m.fract() == 0.0 && m.abs() < i64::MAX as f64 => {
                doc["idle_unload_minutes"] = toml_edit::value(m as i64)
            }
            Some(m) => doc["idle_unload_minutes"] = toml_edit::value(m),
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
        let f = &self.formats;
        // Through its shortest decimal, so 0.7 is written as 0.7, not 0.699999988.
        let score = |s: f32| toml_edit::value(s.to_string().parse::<f64>().unwrap_or(f64::from(s)));
        set(&mut doc, "text", "min_score", f.text.min_score.map(score));
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
            f.images.min_score.map(score),
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
        set(&mut doc, "code", "min_score", f.code.min_score.map(score));
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
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, doc.to_string()).with_context(|| tmp.display().to_string())?;
        fs::rename(&tmp, path).with_context(|| path.display().to_string())
    }

    pub fn excludes(&self) -> Result<Excludes> {
        let home = home()?;
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

/// Set or remove `<table>.<key>`, dropping the table once it is empty.
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
    // A whole value, keeping any comment beside it.
    match t.get_mut(key) {
        Some(old) if old.is_value() => {
            let decor = old.as_value().unwrap().decor().clone();
            *old = value;
            *old.as_value_mut().unwrap().decor_mut() = decor;
        }
        _ => {
            t.insert(key, value);
        }
    }
}

/// Set or remove `formats.<format>.<key>`, dropping tables left empty.
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
    // A whole value, keeping any comment beside it.
    match t
        .as_table_like_mut()
        .expect("a format is a table")
        .get_mut(key)
    {
        Some(old) if old.is_value() => {
            let decor = old.as_value().unwrap().decor().clone();
            *old = value;
            *old.as_value_mut().unwrap().decor_mut() = decor;
        }
        _ => {
            t.as_table_like_mut().unwrap().insert(key, value);
        }
    }
}

/// `~/x` against `home`, made absolute and free of `.`/`..`, then its
/// longest existing ancestor canonicalized, so it compares equal to the
/// canonical paths indexing records, including once a folder that is missing
/// now is created (on Silverblue, `/home` is a symlink to `/var/home`).
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

/// `path` with its longest existing ancestor canonicalized and the rest
/// appended as written.
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
        // Written before Documents/tax exists, through the symlink.
        let ex = resolve("~/Documents/tax", &link.join("u")).unwrap();
        assert_eq!(ex, real.join("Documents/tax"));
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
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("index_on_battery = false\n"), "{text}");
        assert!(
            text.starts_with("# Never indexed or watched.\nexclude = [\n"),
            "{text}"
        );
        assert!(!text.contains("device"), "{text}");
        assert!(text.contains("idle_unload_minutes = 0\n"), "{text}");
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
        c.save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("editor"));
        assert_eq!(Config::load(&path).unwrap(), c);
        fs::remove_dir_all(&dir).unwrap();
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
        // Text can't be turned off yet.
        assert!(toml::from_str::<Config>("[formats.text]\nenabled = false\n").is_err());
        // Code is opt-in (#110).
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
