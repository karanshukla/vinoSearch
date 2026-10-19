//! Turning KRunner and GNOME search on and off, by running what the CLI does:
//! `vinosearch krunner --install` and `--uninstall`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use eframe::egui;
use vinosearch_shared::dbus::KRUNNER_PLUGIN;

#[derive(Clone, Debug, Default)]
pub struct State {
    pub registered: bool,
    pub running: bool,
    /// What the last run printed.
    pub output: Option<String>,
}

impl State {
    pub fn read() -> Self {
        State {
            registered: data_home().is_some_and(|d| d.join(KRUNNER_PLUGIN).exists()),
            ..State::default()
        }
    }
}

fn data_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")),
    }
}

/// The `vinosearch` installed beside this binary, as `cargo install` puts
/// them; else whichever is first on PATH.
pub fn vinosearch() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("vinosearch")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("vinosearch"))
}

pub fn run(state: Arc<Mutex<State>>, ctx: &egui::Context, install: bool) {
    state.lock().unwrap().running = true;
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let flag = if install { "--install" } else { "--uninstall" };
        let output = match Command::new(vinosearch()).args(["krunner", flag]).output() {
            Ok(out) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                if !out.status.success() {
                    text.push_str(&format!("\nvinosearch krunner {flag}: {}", out.status));
                }
                text.trim().to_owned()
            }
            Err(e) => format!("Couldn't run vinosearch: {e}"),
        };
        let mut s = state.lock().unwrap();
        *s = State {
            output: Some(output),
            ..State::read()
        };
        ctx.request_repaint();
    });
}
