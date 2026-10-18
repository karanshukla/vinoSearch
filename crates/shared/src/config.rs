//! `~/.config/vinosearch/config.toml`: settings shared by the CLI, the runner
//! and (later) the settings app, which edits this file rather than keeping
//! state of its own.
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

    pub fn excludes(&self) -> Result<Excludes> {
        let home = home()?;
        self.exclude
            .iter()
            .map(|e| resolve(e, &home))
            .collect::<Result<_>>()
            .map(Excludes)
    }
}

/// `~/x` against `home`, made absolute and free of `.`/`..`, then
/// canonicalized when it exists, so it compares equal to the canonical paths
/// indexing records.
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
    Ok(fs::canonicalize(&clean).unwrap_or(clean))
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
    fn idle_unload_validates() {
        assert_eq!(idle_unload(0.0).unwrap(), None);
        assert_eq!(idle_unload(0.5).unwrap(), Some(Duration::from_secs(30)));
        assert!(idle_unload(-1.0).is_err());
        assert!(idle_unload(f64::NAN).is_err());
        assert!(idle_unload(f64::INFINITY).is_err());
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let c = Config::load(Path::new("/nonexistent-dir/config.toml")).unwrap();
        assert_eq!(c, Config::default());
    }
}
