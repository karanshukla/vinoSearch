//! Uninstalling deletes the models and their compiled cache and keeps the
//! user's index and settings; `--purge` deletes the index too. And
//! `delete-model` deletes the models with no service running.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn temp_home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("vinosearch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    home
}

/// A file of another app's, under the same cache: never touched.
const ANOTHER_APPS: &str = "cache/vinospeak/kokoro.onnx";

fn put(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"").unwrap();
}

/// Everything an install leaves in `home`, with the XDG directories moved.
fn installed(home: &Path) {
    for file in [
        "data/vinosearch/models/embeddinggemma-2-text-int8-cw-onnx/onnx/model_text.onnx",
        "data/vinosearch/models/pp-ocrv6-small-onnx/det.onnx",
        "data/vinosearch/index.sqlite",
        "cache/vinosearch/ov-cache/blob.cl",
        "config/vinosearch/config.toml",
        "runtime/lib/libopenvino.so",
        ANOTHER_APPS,
    ] {
        put(&home.join(file));
    }
}

/// `scripts/install.sh` with everything it touches under `home`. No
/// `vinosearch` is installed there, so no service is stopped for real.
fn install_sh(home: &Path, args: &[&str]) {
    let out = Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install.sh"))
        .args(args)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("CARGO_HOME", home.join("cargo"))
        .env("CARGO_INSTALL_ROOT", home.join("cargo"))
        .env("VINOSEARCH_OPENVINO_DIR", home.join("runtime"))
        .env_remove("VINOSEARCH_CONFIG")
        .output()
        .unwrap();
    assert_ok(&out);
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `vinosearch` with its files under `home`, and no session bus: never the
/// runner of whoever runs the tests.
fn vinosearch_in(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vinosearch"));
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent");
    command
}

/// No systemctl either: never the user's service, and the files go all the
/// same.
const NO_SYSTEMCTL: &str = "";

#[test]
fn uninstall_deletes_the_models_and_keeps_the_index() {
    let home = temp_home("uninstall");
    installed(&home);

    install_sh(&home, &["--uninstall"]);
    assert!(!home.join("data/vinosearch/models").exists());
    assert!(!home.join("cache/vinosearch").exists());
    assert!(home.join("data/vinosearch/index.sqlite").is_file());
    assert!(home.join("config/vinosearch/config.toml").is_file());
    assert!(home.join("runtime/lib/libopenvino.so").is_file());
    assert!(home.join(ANOTHER_APPS).is_file());

    install_sh(&home, &["--uninstall", "--purge"]);
    assert!(!home.join("data/vinosearch").exists());
    assert!(!home.join("runtime").exists());
    assert!(home.join("config/vinosearch/config.toml").is_file());
    assert!(home.join(ANOTHER_APPS).is_file());

    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn uninstall_removes_the_autostart_entry_and_an_older_installs_unit() {
    let home = temp_home("uninstall-start");
    let files = [
        "config/autostart/dev.vinosearch.Runner.desktop",
        "config/systemd/user/vinosearch.service",
        "data/dbus-1/services/dev.vinosearch.Runner.service",
    ];
    for f in files {
        put(&home.join(f));
    }
    let wants = home.join("config/systemd/user/default.target.wants");
    std::fs::create_dir_all(&wants).unwrap();
    std::os::unix::fs::symlink("../vinosearch.service", wants.join("vinosearch.service")).unwrap();
    let out = vinosearch_in(&home)
        .args(["krunner", "--uninstall"])
        .env("PATH", NO_SYSTEMCTL)
        .output()
        .unwrap();
    assert_ok(&out);
    for f in files {
        assert!(!home.join(f).exists(), "{f} is still there");
    }
    assert!(wants.join("vinosearch.service").symlink_metadata().is_err());
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn delete_model_deletes_them_without_a_service() {
    let home = temp_home("delete-model");
    installed(&home);
    let out = vinosearch_in(&home)
        .arg("delete-model")
        .env(
            "VINOSEARCH_CONFIG",
            home.join("config/vinosearch/config.toml"),
        )
        .output()
        .unwrap();
    assert_ok(&out);
    assert!(!home.join("data/vinosearch/models").exists());
    assert!(home.join("data/vinosearch/index.sqlite").is_file());
    std::fs::remove_dir_all(&home).unwrap();
}
