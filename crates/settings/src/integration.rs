//! Turning each search place, and the service, on and off, by running what
//! the CLI does: `vinosearch integrate kde|gnome [--off]`, and `vinosearch
//! krunner --install` / `--uninstall` for the service with everything (#45).

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use eframe::egui;
use vinosearch_shared::dbus::{DBUS_SERVICE_FILE, GNOME_PROVIDER, KRUNNER_PLUGIN, SEARCH_ENTRY};

/// One switch on the Integration page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    /// KRunner and the Plasma launcher's search.
    Kde,
    /// GNOME Shell's overview search.
    Gnome,
    /// The search window in the app menu.
    Menu,
    /// The service, with the running desktop's place on and every place off.
    Service,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub kde: bool,
    pub gnome: bool,
    pub menu: bool,
    pub service: bool,
    pub running: bool,
    /// What the last run printed.
    pub output: Option<String>,
}

impl State {
    pub fn read() -> Self {
        let on = |file: &str| data_home().is_some_and(|d| d.join(file).exists());
        State {
            kde: on(KRUNNER_PLUGIN),
            gnome: on(GNOME_PROVIDER),
            menu: on(SEARCH_ENTRY),
            service: on(DBUS_SERVICE_FILE),
            ..State::default()
        }
    }

    pub fn is_on(&self, switch: Switch) -> bool {
        match switch {
            Switch::Kde => self.kde,
            Switch::Gnome => self.gnome,
            Switch::Menu => self.menu,
            Switch::Service => self.service,
        }
    }
}

/// Which desktops the session is, from XDG_CURRENT_DESKTOP: (Plasma, GNOME).
pub fn desktops() -> (bool, bool) {
    desktops_in(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref())
}

fn desktops_in(desktop: Option<&str>) -> (bool, bool) {
    let names: Vec<String> = desktop
        .unwrap_or_default()
        .split(':')
        .map(str::to_ascii_uppercase)
        .collect();
    let has = |n: &str| names.iter().any(|d| d == n);
    (has("KDE"), has("GNOME"))
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

fn args(switch: Switch, on: bool) -> Vec<&'static str> {
    match (switch, on) {
        (Switch::Kde, true) => vec!["integrate", "kde"],
        (Switch::Kde, false) => vec!["integrate", "kde", "--off"],
        (Switch::Gnome, true) => vec!["integrate", "gnome"],
        (Switch::Gnome, false) => vec!["integrate", "gnome", "--off"],
        (Switch::Menu, true) => vec!["integrate", "menu"],
        (Switch::Menu, false) => vec!["integrate", "menu", "--off"],
        (Switch::Service, true) => vec!["krunner", "--install"],
        (Switch::Service, false) => vec!["krunner", "--uninstall"],
    }
}

pub fn run(state: Arc<Mutex<State>>, ctx: &egui::Context, switch: Switch, on: bool) {
    state.lock().unwrap().running = true;
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let args = args(switch, on);
        let output = match Command::new(vinosearch()).args(&args).output() {
            Ok(out) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                if !out.status.success() {
                    text.push_str(&format!("\nvinosearch {}: {}", args.join(" "), out.status));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_desktop_is_read_from_the_variable() {
        assert_eq!(desktops_in(Some("KDE")), (true, false));
        assert_eq!(desktops_in(Some("ubuntu:GNOME")), (false, true));
        assert_eq!(desktops_in(Some("XFCE")), (false, false));
        assert_eq!(desktops_in(None), (false, false));
    }

    #[test]
    fn each_switch_runs_the_matching_command() {
        assert_eq!(args(Switch::Kde, false), ["integrate", "kde", "--off"]);
        assert_eq!(args(Switch::Gnome, true), ["integrate", "gnome"]);
        assert_eq!(args(Switch::Menu, false), ["integrate", "menu", "--off"]);
        assert_eq!(args(Switch::Service, false), ["krunner", "--uninstall"]);
    }
}
