//! The app menu entry belongs to the install, not to the GNOME search place:
//! `integrate gnome --off` used to delete it, taking vinoSearch out of
//! Plasma's menu too.

use std::path::{Path, PathBuf};
use std::process::Command;

fn integrate(home: &Path, args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_vinosearch"))
        .arg("integrate")
        .args(args)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn temp_home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("vinosearch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    // The service counts as installed, so `integrate` doesn't write a unit
    // or call systemctl for the user running the tests.
    let service = home.join("data/dbus-1/services/dev.vinosearch.Runner.service");
    std::fs::create_dir_all(service.parent().unwrap()).unwrap();
    std::fs::write(&service, "").unwrap();
    home
}

#[test]
fn turning_gnome_off_keeps_the_app_menu_entry() {
    let home = temp_home("launcher");
    let launcher = home.join("data/applications/dev.vinosearch.desktop");
    let provider = home.join(
        "data/vinosearch/xdg/gnome-shell/search-providers/dev.vinosearch.search-provider.ini",
    );

    integrate(&home, &["gnome"]);
    assert!(launcher.is_file() && provider.is_file());

    integrate(&home, &["gnome", "--off"]);
    assert!(!provider.exists(), "GNOME's provider is still there");
    assert!(launcher.is_file(), "the app menu entry went with GNOME");

    std::fs::remove_dir_all(&home).unwrap();
}
