//! The vinoSearch settings window. Asks the runner for its state over
//! `dev.vinosearch.Control1` and edits `config.toml`, which the runner re-reads.

mod control;
mod doctor;
mod integration;
mod portal;
mod theme;
mod window;

use std::process::ExitCode;

const USAGE: &str = "\
vinosearch-settings: the vinoSearch settings window

Usage: vinosearch-settings [--page index|folders|search|model|hardware|integration]
       vinosearch-settings --search [QUERY]

--search opens the search window, searching QUERY if given (its words
need no quotes).
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => window::run(vinosearch_shared::dbus::SETTINGS_APP_ID, None),
        ["--page", page] => window::run(vinosearch_shared::dbus::SETTINGS_APP_ID, Some(page)),
        ["--search", ref query @ ..] => {
            window::search::run(vinosearch_shared::dbus::SETTINGS_APP_ID, &query.join(" "))
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
