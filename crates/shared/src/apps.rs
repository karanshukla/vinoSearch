//! Installed apps, from their desktop entries: the editors the settings app
//! offers for the `editor` setting, and where the runner finds the entry an
//! `editor = "org.kde.kate.desktop"` names.

use std::path::PathBuf;

/// An app that opens text files, as the settings app lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    /// The desktop entry id, `org.kde.kate.desktop`: what `editor` is set to.
    pub id: String,
    pub name: String,
}

/// The user's data directory, then the system's, in XDG order.
pub fn data_dirs() -> Vec<PathBuf> {
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
            if let Some(name) = editor_name(&text) {
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

/// The entry's Name when it is a shown, graphical app for text.
fn editor_name(text: &str) -> Option<String> {
    let mut in_entry = false;
    let (mut name, mut for_text, mut app) = (None, false, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_entry) else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Name" => name = Some(value.to_owned()),
            "Type" => app = value == "Application",
            "MimeType" => for_text |= value.split(';').any(|m| m == "text/plain"),
            "Categories" => for_text |= value.split(';').any(|c| c == "TextEditor"),
            "NoDisplay" | "Hidden" | "Terminal" if value == "true" => return None,
            _ => {}
        }
    }
    name.filter(|n| app && for_text && !n.is_empty())
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
    fn an_entry_id_is_a_bare_desktop_file_name() {
        assert!(is_entry_id("org.kde.kate.desktop"));
        assert!(!is_entry_id("code"));
        assert!(!is_entry_id("/usr/share/applications/kate.desktop"));
        assert!(!is_entry_id("foot -e nvim {file}.desktop"));
    }
}
