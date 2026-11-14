//! Installed apps, from their desktop entries: the editors and players the
//! settings app offers for the `editor` and `player` settings, and where the
//! runner finds the entry an `editor = "org.kde.kate.desktop"` names.

use std::path::PathBuf;

/// An app that opens text files, or plays media, as the settings app lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    /// The desktop entry id, `org.kde.kate.desktop`: what the setting is set to.
    pub id: String,
    pub name: String,
}

/// The user's data directory, then the system's, in XDG order. In the
/// Flatpak, the host's: the apps that open files run there.
pub fn data_dirs() -> Vec<PathBuf> {
    if crate::sandbox::flatpak_id().is_some() {
        return host_data_dirs();
    }
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => dirs.push(PathBuf::from(d)),
        _ => {
            if let Some(h) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(h).join(".local/share"));
            }
        }
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

/// The host's data directories as the sandbox sees them: the home's own
/// (`--filesystem=home`), Flatpak's exports (the system's by their own
/// permission), and /usr under /run/host (`--filesystem=host-os:ro`).
fn host_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(h) = std::env::var_os("HOME") {
        let share = PathBuf::from(h).join(".local/share");
        dirs.push(share.join("flatpak/exports/share"));
        dirs.insert(0, share);
    }
    dirs.extend(
        [
            "/var/lib/flatpak/exports/share",
            "/run/host/usr/local/share",
            "/run/host/usr/share",
        ]
        .map(PathBuf::from),
    );
    dirs
}

/// Whether `editor` names a desktop entry rather than a command.
pub fn is_entry_id(editor: &str) -> bool {
    let editor = editor.trim();
    editor.ends_with(".desktop") && !editor.contains(['/', ' '])
}

/// Every app that says it opens text/plain or is a text editor (VS Code
/// claims only its workspace files), by name, from `dirs`' `applications`. An id in an earlier directory hides the same id later, as
/// in the menu; hidden entries and terminal apps (which need a terminal to
/// run in) are left out.
pub fn editors_in(dirs: &[PathBuf]) -> Vec<App> {
    apps_in(dirs, for_text)
}

/// Every app that says it plays video or audio files, as `editors_in` lists
/// editors.
pub fn players_in(dirs: &[PathBuf]) -> Vec<App> {
    apps_in(dirs, for_media)
}

fn apps_in(dirs: &[PathBuf], wanted: fn(&str, &str) -> bool) -> Vec<App> {
    let mut seen = std::collections::HashSet::new();
    let mut apps = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir.join("applications")) else {
            continue;
        };
        let mut found: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
            .filter(|(id, _)| id.ends_with(".desktop"))
            .collect();
        found.sort();
        for (id, path) in found {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(name) = app_name(&text, wanted) {
                apps.push(App { id, name });
            }
        }
    }
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    apps
}

pub fn editors() -> Vec<App> {
    editors_in(&data_dirs())
}

pub fn players() -> Vec<App> {
    players_in(&data_dirs())
}

/// Whether a desktop entry's `key=value` says it opens text.
fn for_text(key: &str, value: &str) -> bool {
    match key {
        "MimeType" => value.split(';').any(|m| m == "text/plain"),
        "Categories" => value.split(';').any(|c| c == "TextEditor"),
        _ => false,
    }
}

/// Whether a desktop entry's `key=value` says it plays media files. A
/// `Player` category alone isn't enough: a streaming client has it too.
fn for_media(key: &str, value: &str) -> bool {
    key == "MimeType"
        && value
            .split(';')
            .any(|m| m.starts_with("video/") || m.starts_with("audio/"))
}

/// The entry's Name when it is a shown, graphical app `wanted` accepts.
fn app_name(text: &str, wanted: fn(&str, &str) -> bool) -> Option<String> {
    let mut in_entry = false;
    let (mut name, mut fits, mut app) = (None, false, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_entry) else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        fits |= wanted(key, value);
        match key {
            "Name" => name = Some(value.to_owned()),
            "Type" => app = value == "Application",
            "NoDisplay" | "Hidden" | "Terminal" if value == "true" => return None,
            _ => {}
        }
    }
    name.filter(|n| app && fits && !n.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn editors_are_shown_graphical_text_apps_by_name() {
        let root = std::env::temp_dir().join(format!("vinosearch-apps-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (user, system) = (root.join("user"), root.join("system"));
        let write = |dir: &PathBuf, id: &str, body: &str| {
            fs::create_dir_all(dir.join("applications")).unwrap();
            fs::write(
                dir.join("applications").join(id),
                format!("[Desktop Entry]\nType=Application\n{body}"),
            )
            .unwrap();
        };
        write(
            &system,
            "org.kde.kate.desktop",
            "Name=Kate\nMimeType=text/plain;text/x-c++src;\n",
        );
        write(
            &system,
            "code.desktop",
            "Name=Visual Studio Code\nMimeType=text/plain;\n",
        );
        write(
            &system,
            "nvim.desktop",
            "Name=Neovim\nTerminal=true\nMimeType=text/plain;\n",
        );
        write(
            &system,
            "hidden.desktop",
            "Name=Old\nNoDisplay=true\nMimeType=text/plain;\n",
        );
        write(
            &system,
            "okular.desktop",
            "Name=Okular\nMimeType=application/pdf;\n",
        );
        write(
            &system,
            "com.microsoft.VSCode.desktop",
            "Name=Visual Studio Code\nCategories=TextEditor;Development;IDE;\n\
             MimeType=application/x-code-workspace;\n\n\
             [Desktop Action new-empty-window]\nName=New Empty Window\n",
        );
        // The user's copy hides the system's.
        write(
            &user,
            "code.desktop",
            "Name=Code (mine)\nMimeType=text/plain;\n",
        );
        let apps = editors_in(&[user, system]);
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Code (mine)", "Kate", "Visual Studio Code"]);
        assert_eq!(apps[1].id, "org.kde.kate.desktop");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn players_are_shown_graphical_apps_for_video_or_audio() {
        let root = std::env::temp_dir().join(format!("vinosearch-players-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let write = |id: &str, body: &str| {
            fs::create_dir_all(root.join("applications")).unwrap();
            fs::write(
                root.join("applications").join(id),
                format!("[Desktop Entry]\nType=Application\n{body}"),
            )
            .unwrap();
        };
        write(
            "mpv.desktop",
            "Name=mpv Media Player\nMimeType=video/mp4;audio/mpeg;\n",
        );
        write(
            "org.kde.elisa.desktop",
            "Name=Elisa\nMimeType=audio/flac;\n",
        );
        write("org.kde.kate.desktop", "Name=Kate\nMimeType=text/plain;\n");
        // A streaming client plays nothing from disk.
        write(
            "stream.desktop",
            "Name=Stream\nCategories=AudioVideo;Player;\n",
        );
        write(
            "cli.desktop",
            "Name=Terminal player\nTerminal=true\nMimeType=video/mp4;\n",
        );
        let apps = players_in(std::slice::from_ref(&root));
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Elisa", "mpv Media Player"]);
        assert_eq!(apps[1].id, "mpv.desktop");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_entry_id_is_a_bare_desktop_file_name() {
        assert!(is_entry_id("org.kde.kate.desktop"));
        assert!(!is_entry_id("code"));
        assert!(!is_entry_id("/usr/share/applications/kate.desktop"));
        assert!(!is_entry_id("foot -e nvim {file}.desktop"));
    }
}
