//! Running inside the Flatpak. The bus names, file names and paths that
//! differ from a native install are picked from this at run time, so one
//! build serves both, and the two can be installed side by side.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The Flatpak's app id when this runs inside its sandbox; None natively.
pub fn flatpak_id() -> Option<&'static str> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| {
        read_id(
            Path::new("/.flatpak-info"),
            std::env::var("FLATPAK_ID").ok(),
        )
    })
    .as_deref()
}

/// `/.flatpak-info` is what marks a sandbox: FLATPAK_ID alone can be left
/// in the environment of something started from one. The file's
/// `[Application] name` when the variable is unset.
fn read_id(info: &Path, env: Option<String>) -> Option<String> {
    let text = std::fs::read_to_string(info).ok()?;
    if let Some(id) = env.filter(|id| !id.is_empty()) {
        return Some(id);
    }
    ini_value(&text, "Application", "name")
}

/// `key` of `[section]` in `/.flatpak-info`; None natively.
pub fn info_value(section: &str, key: &str) -> Option<String> {
    ini_value(
        &std::fs::read_to_string("/.flatpak-info").ok()?,
        section,
        key,
    )
}

fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = name == section;
        } else if inside
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_owned()).filter(|v| !v.is_empty());
        }
    }
    None
}

/// The branch of a release bundle. A build from a checkout is `master`, and
/// only a release updates itself from the next release's bundle.
pub const RELEASE_BRANCH: &str = "stable";

/// The real home's `.config`: the sandbox's XDG_CONFIG_HOME is the app's
/// own, under ~/.var/app, but the Background portal writes the autostart
/// entry on the host.
pub fn host_autostart_entry(app_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config/autostart")
            .join(format!("{app_id}.desktop")),
    )
}

/// Set once start at login was first asked for (or turned off), so the
/// settings window asks only the first time it opens.
pub fn autostart_asked_path() -> Option<PathBuf> {
    let state = match std::env::var_os("XDG_STATE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(state.join("vinosearch/autostart-asked"))
}

/// GNOME's schema listing the search providers turned on and off.
pub const GNOME_SEARCH: &str = "org.gnome.desktop.search-providers";

/// Run `argv` on the host (`flatpak-spawn --host`), its stdout when it
/// succeeds. Its stderr is dropped.
pub fn host_output(argv: &[&str]) -> Option<String> {
    let out = std::process::Command::new("flatpak-spawn")
        .arg("--host")
        .args(argv)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// GNOME's `enabled` or `disabled` search providers on the host; None
/// without gsettings or the schema.
pub fn gnome_providers(key: &str) -> Option<Vec<String>> {
    Some(parse_strv(&host_output(&[
        "gsettings",
        "get",
        GNOME_SEARCH,
        key,
    ])?))
}

/// gsettings' printing of a string list: `@as []` or `['a', 'b']`.
fn parse_strv(text: &str) -> Vec<String> {
    let text = text.trim().trim_start_matches("@as").trim();
    let inner = text.trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"').to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A string list as gsettings takes it. Desktop ids have no quotes.
pub fn strv(list: &[String]) -> String {
    let quoted: Vec<String> = list.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", quoted.join(", "))
}

/// Whether KRunner's settings on the host turn plugin `id` on: the Flatpak's
/// plugin is off unless they do.
pub fn krunner_plugin_on(id: &str) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let text =
        std::fs::read_to_string(PathBuf::from(home).join(".config/krunnerrc")).unwrap_or_default();
    plugin_on(&text, id)
}

fn plugin_on(krunnerrc: &str, id: &str) -> bool {
    let key = format!("{id}Enabled");
    let mut in_plugins = false;
    let mut on = false;
    for line in krunnerrc.lines().map(str::trim) {
        if line.starts_with('[') {
            in_plugins = line == "[Plugins]";
        } else if in_plugins
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            on = v.trim() == "true";
        }
    }
    on
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gsettings_lists_read_and_write() {
        assert!(parse_strv("@as []\n").is_empty());
        let list = parse_strv("['org.gnome.Nautilus.desktop', 'a.b.desktop']\n");
        assert_eq!(list, ["org.gnome.Nautilus.desktop", "a.b.desktop"]);
        assert_eq!(strv(&list), "['org.gnome.Nautilus.desktop', 'a.b.desktop']");
        assert_eq!(strv(&[]), "[]");
    }

    #[test]
    fn a_krunner_plugin_is_on_only_when_its_group_says_so() {
        let rc =
            "[General]\nx.yEnabled=true\n\n[Plugins]\nbaloosearchEnabled=false\nx.yEnabled=true\n";
        assert!(plugin_on(rc, "x.y"));
        assert!(!plugin_on(rc, "baloosearch"));
        assert!(!plugin_on("[General]\nx.yEnabled=true\n", "x.y"));
    }

    #[test]
    fn the_app_id_comes_from_the_info_file_only_inside_a_sandbox() {
        let dir = std::env::temp_dir().join(format!("vinosearch-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let info = dir.join("flatpak-info");
        assert_eq!(read_id(&info, Some("dev.example.App".into())), None);
        std::fs::write(
            &info,
            "[Application]\nname=dev.example.App\nruntime=x\n\n[Instance]\nname=other\n",
        )
        .unwrap();
        assert_eq!(read_id(&info, None).as_deref(), Some("dev.example.App"));
        assert_eq!(
            read_id(&info, Some("dev.example.Env".into())).as_deref(),
            Some("dev.example.Env")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn info_values_come_from_their_own_section() {
        let info = "[Application]\nname=a.b\n\n[Instance]\nbranch=stable\napp-path=/x/app/a.b/files\nname=\n";
        assert_eq!(
            ini_value(info, "Instance", "branch").as_deref(),
            Some("stable")
        );
        assert_eq!(ini_value(info, "Application", "branch"), None);
        assert_eq!(ini_value(info, "Instance", "name"), None);
        assert_eq!(
            ini_value(info, "Instance", "app-path").as_deref(),
            Some("/x/app/a.b/files")
        );
    }
}
