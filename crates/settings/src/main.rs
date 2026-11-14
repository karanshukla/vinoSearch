//! The vinoSearch settings window. Asks the runner for its state over
//! `dev.vinosearch.Control1` and edits `config.toml`, which the runner re-reads.

mod control;
mod doctor;
mod integration;
mod portal;
mod window;

use std::process::ExitCode;

const USAGE: &str = "\
vinosearch-settings: the vinoSearch settings window

Usage: vinosearch-settings [--page index|folders|search|model|hardware|integration]
       vinosearch-settings --search [QUERY]

--search opens the search window, searching QUERY if given (its words
need no quotes).
";

/// In the Flatpak, tell vino-ui the desktop's light or dark through its
/// `VINO_UI_THEME`: it would ask `busctl`, find none and stay dark.
fn follow_desktop_theme() {
    if vinosearch_shared::sandbox::flatpak_id().is_none()
        || std::env::var_os("VINO_UI_THEME").is_some()
    {
        return;
    }
    if let Some(dark) = portal::prefers_dark() {
        // SAFETY: the first thing main does, before any other thread exists
        // to read the environment.
        unsafe { std::env::set_var("VINO_UI_THEME", if dark { "dark" } else { "light" }) };
    }
}

/// In the Flatpak, Plasma's colours and fonts where vino-ui reads them:
/// `kdeglobals` and `kcmfontsrc` in XDG_CONFIG_HOME, which in the sandbox is
/// the app's own folder and has neither. Links to the home's, so a change
/// in System Settings is followed as it is natively.
fn link_plasma_settings() {
    if vinosearch_shared::sandbox::flatpak_id().is_none() {
        return;
    }
    let (Some(config), Some(home)) = (
        std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()),
        std::env::var_os("HOME"),
    ) else {
        return;
    };
    link_settings(
        &std::path::Path::new(&home).join(".config"),
        std::path::Path::new(&config),
    );
}

fn link_settings(host: &std::path::Path, own: &std::path::Path) {
    if host == own {
        return;
    }
    for name in ["kdeglobals", "kcmfontsrc"] {
        let link = own.join(name);
        // A link there already, or a file someone put there: left alone.
        if link.symlink_metadata().is_err() {
            let _ = std::os::unix::fs::symlink(host.join(name), link);
        }
    }
}

fn main() -> ExitCode {
    follow_desktop_theme();
    link_plasma_settings();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => window::run(&vinosearch_shared::dbus::names().app_id, None),
        ["--page", page] => window::run(&vinosearch_shared::dbus::names().app_id, Some(page)),
        ["--search", ref query @ ..] => {
            window::search::run(&vinosearch_shared::dbus::names().app_id, &query.join(" "))
        }
        ["-h" | "--help"] => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plasmas_settings_are_linked_once_and_follow_the_homes() {
        let dir = std::env::temp_dir().join(format!("vinosearch-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (host, own) = (dir.join("host"), dir.join("own"));
        std::fs::create_dir_all(&host).unwrap();
        std::fs::create_dir_all(&own).unwrap();
        // No kcmfontsrc on the host yet: its link waits for one.
        link_settings(&host, &own);
        assert!(std::fs::read_to_string(own.join("kcmfontsrc")).is_err());
        std::fs::write(host.join("kdeglobals"), "a").unwrap();
        std::fs::write(host.join("kcmfontsrc"), "b").unwrap();
        link_settings(&host, &own);
        assert_eq!(
            std::fs::read_to_string(own.join("kdeglobals")).unwrap(),
            "a"
        );
        assert_eq!(
            std::fs::read_to_string(own.join("kcmfontsrc")).unwrap(),
            "b"
        );
        // The same folder, as natively: nothing to link, nothing overwritten.
        link_settings(&host, &host);
        assert_eq!(
            std::fs::read_to_string(host.join("kdeglobals")).unwrap(),
            "a"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
