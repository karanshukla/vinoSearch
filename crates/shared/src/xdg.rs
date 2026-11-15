//! The XDG base directories: `$XDG_*_HOME` when set to something, else the
//! spec's default under `$HOME`.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;

pub fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("no HOME")?))
}

pub fn config_home() -> Result<PathBuf> {
    base_dir("XDG_CONFIG_HOME", ".config")
}

pub fn data_home() -> Result<PathBuf> {
    base_dir("XDG_DATA_HOME", ".local/share")
}

pub fn cache_home() -> Result<PathBuf> {
    base_dir("XDG_CACHE_HOME", ".cache")
}

pub fn state_home() -> Result<PathBuf> {
    base_dir("XDG_STATE_HOME", ".local/state")
}

fn base_dir(variable: &str, default_under_home: &str) -> Result<PathBuf> {
    resolve(
        std::env::var_os(variable),
        std::env::var_os("HOME"),
        default_under_home,
    )
    .context("no HOME")
}

/// The spec treats an empty variable as unset.
/// `an_empty_variable_counts_as_unset` and `a_set_variable_wins_over_home`
/// pin both sides.
pub fn resolve(
    variable: Option<OsString>,
    home: Option<OsString>,
    default_under_home: &str,
) -> Option<PathBuf> {
    match variable {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home.map(|home| PathBuf::from(home).join(default_under_home)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(text: &str) -> Option<OsString> {
        Some(OsString::from(text))
    }

    #[test]
    fn a_set_variable_wins_over_home() {
        assert_eq!(
            resolve(os("/data"), os("/home/u"), ".local/share"),
            Some(PathBuf::from("/data"))
        );
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(
            resolve(os(""), os("/home/u"), ".local/share"),
            Some(PathBuf::from("/home/u/.local/share"))
        );
    }

    #[test]
    fn an_unset_variable_falls_back_to_the_default_under_home() {
        assert_eq!(
            resolve(None, os("/home/u"), ".cache"),
            Some(PathBuf::from("/home/u/.cache"))
        );
    }

    #[test]
    fn a_set_variable_needs_no_home() {
        assert_eq!(
            resolve(os("/state"), None, ".local/state"),
            Some(PathBuf::from("/state"))
        );
    }

    #[test]
    fn neither_set_is_no_directory() {
        assert_eq!(resolve(None, None, ".config"), None);
    }
}
