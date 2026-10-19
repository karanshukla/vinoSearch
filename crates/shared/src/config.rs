//! `~/.config/vinosearch/config.toml`: settings shared by the CLI, the runner
//! and (later) the settings app, which edits this file rather than keeping
//! state of its own, through [`Config::save`].
//!
//! ```toml
//! exclude = ["~/Downloads", "~/Documents/tax"]
//! device = "NPU"
//! idle_unload_minutes = 5
//! ```
//!
//! A missing file is the defaults. Flags and environment variables override it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_DEVICE: &str = "NPU";
pub const DEFAULT_IDLE_UNLOAD_MINUTES: f64 = 5.0;

/// How long the model may sit idle before it is freed; None keeps it loaded.
pub fn idle_unload(minutes: f64) -> Result<Option<Duration>> {
    if !(minutes >= 0.0 && minutes.is_finite()) {
        bail!("idle_unload_minutes must be 0 or more");
    }
    Ok((minutes > 0.0).then(|| Duration::from_secs_f64(minutes * 60.0)))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
        c.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# Never indexed or watched.\nexclude = [\n"),
            "{text}"
        );
        assert!(!text.contains("device"), "{text}");
        assert!(text.contains("idle_unload_minutes = 0\n"), "{text}");
        assert_eq!(Config::load(&path).unwrap(), c);

        c.exclude.clear();
        c.idle_unload_minutes = Some(2.5);
        c.save(&path).unwrap();
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
    fn a_missing_file_is_the_defaults() {
        let c = Config::load(Path::new("/nonexistent-dir/config.toml")).unwrap();
        assert_eq!(c, Config::default());
    }
}
