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

fn main() -> ExitCode {
    follow_desktop_theme();
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
