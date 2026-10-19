//! File-type icons for results, from shared-mime-info: `foo.rs` gets
//! `text-rust`, `bar.png` `image-png`, where an installed icon theme has them.
//!
//! KRunner takes one icon name and shows nothing for one the theme lacks, so
//! only names some installed theme has are used; anything else gets the
//! kind's generic icon.

use crate::index::Kind;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The best icon name for `path`, then the generic one for its kind.
pub fn for_path(path: &Path, kind: Kind) -> (&'static str, &'static str) {
    static ICONS: OnceLock<HashMap<String, String>> = OnceLock::new();
    let generic = match kind {
        Kind::Text => "text-x-generic",
        Kind::Image => "image-x-generic",
    };
    let specific = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| {
            ICONS
                .get_or_init(load)
                .get(&e.to_ascii_lowercase())
                .map(String::as_str)
        })
        .unwrap_or(generic);
    (specific, generic)
}

/// $XDG_DATA_HOME, then $XDG_DATA_DIRS, as the spec orders them.
fn data_dirs() -> Vec<PathBuf> {
    let home = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")),
    };
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(dirs.split(':').map(PathBuf::from))
        .collect()
}

fn load() -> HashMap<String, String> {
    let dirs = data_dirs();
    let read = |name: &str| -> String {
        dirs.iter()
            .filter_map(|d| std::fs::read_to_string(d.join("mime").join(name)).ok())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut available = HashSet::new();
    for d in &dirs {
        theme_icons(&d.join("icons"), &mut available);
    }
    icon_names(
        &read("globs2"),
        &read("icons"),
        &read("generic-icons"),
        &available,
    )
}

/// Names of the mimetype icons under an icons directory (`<theme>/mimetypes/<size>/`,
/// `<theme>/<size>/mimetypes/`).
fn theme_icons(icons: &Path, out: &mut HashSet<String>) {
    let walk = walkdir::WalkDir::new(icons)
        .max_depth(4)
        .into_iter()
        // Past the theme and size levels, only into mimetypes directories.
        .filter_entry(|e| {
            e.depth() <= 2 || e.path().components().any(|c| c.as_os_str() == "mimetypes")
        });
    for e in walk.flatten() {
        if e.file_type().is_file()
            && e.path().components().any(|c| c.as_os_str() == "mimetypes")
            && let Some(stem) = e.path().file_stem().and_then(|s| s.to_str())
        {
            out.insert(stem.to_owned());
        }
    }
}

/// Extension -> icon name, from globs2 (`weight:type:glob`, heaviest first),
/// icons and generic-icons (`type:icon`), keeping names in `available`.
fn icon_names(
    globs2: &str,
    icons: &str,
    generic: &str,
    available: &HashSet<String>,
) -> HashMap<String, String> {
    let pairs = |text: &str| -> HashMap<String, String> {
        text.lines()
            .filter_map(|l| l.split_once(':'))
            .map(|(t, i)| (t.to_owned(), i.to_owned()))
            .collect()
    };
    let (icons, generic) = (pairs(icons), pairs(generic));
    let mut out = HashMap::new();
    for line in globs2.lines().filter(|l| !l.starts_with('#')) {
        let mut parts = line.splitn(3, ':');
        let (Some(_), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Some(ext) = glob.strip_prefix("*.") else {
            continue;
        };
        if !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let ext = ext.to_ascii_lowercase();
        if out.contains_key(&ext) {
            continue;
        }
        let candidates = [
            icons.get(mime).cloned(),
            Some(mime.replace('/', "-")),
            generic.get(mime).cloned(),
        ];
        if let Some(name) = candidates
            .into_iter()
            .flatten()
            .find(|n| available.contains(n))
        {
            out.insert(ext, name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_take_the_heaviest_type_with_an_installed_icon() {
        let globs2 = "# generated\n\
            60:text/x-python:*.py\n\
            50:text/x-python3:*.py\n\
            50:text/rust:*.rs\n\
            50:image/png:*.png\n\
            50:application/x-weird:*.wrd\n\
            50:text/x-csrc:*.c\n\
            50:text/plain:*,v\n";
        let generic = "application/x-weird:text-x-generic-template\n";
        let available: HashSet<String> = [
            "text-x-python",
            "text-rust",
            "image-png",
            "text-x-generic-template",
        ]
        .map(String::from)
        .into();
        let got = icon_names(globs2, "", generic, &available);
        assert_eq!(got["py"], "text-x-python");
        assert_eq!(got["rs"], "text-rust");
        assert_eq!(got["png"], "image-png");
        assert_eq!(got["wrd"], "text-x-generic-template");
        // No installed icon: left to the generic fallback.
        assert!(!got.contains_key("c"));
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn an_unknown_extension_gets_the_kind_icon() {
        assert_eq!(
            for_path(Path::new("/a/b.zzzunknown"), Kind::Image),
            ("image-x-generic", "image-x-generic")
        );
    }
}
