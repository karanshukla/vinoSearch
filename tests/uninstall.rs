//! Uninstalling deletes the models and their compiled cache and keeps the
//! user's index and settings; `--purge` deletes the index too. And
//! `delete-model` deletes the models with no service running.

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("vinosearch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    home
}

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
        // Another app's, under the same cache.
        "cache/vinospeak/kokoro.onnx",
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
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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
    assert!(home.join("cache/vinospeak/kokoro.onnx").is_file());

    install_sh(&home, &["--uninstall", "--purge"]);
    assert!(!home.join("data/vinosearch").exists());
    assert!(!home.join("runtime").exists());
    assert!(home.join("config/vinosearch/config.toml").is_file());
    assert!(home.join("cache/vinospeak/kokoro.onnx").is_file());

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
    let out = Command::new(env!("CARGO_BIN_EXE_vinosearch"))
        .args(["krunner", "--uninstall"])
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        // No systemctl and no bus: never the real service of whoever runs
        // the tests, and the files go all the same.
        .env("PATH", "")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    let out = Command::new(env!("CARGO_BIN_EXE_vinosearch"))
        .arg("delete-model")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env(
            "VINOSEARCH_CONFIG",
            home.join("config/vinosearch/config.toml"),
        )
        // No bus, so no runner: never the real one of whoever runs the tests.
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!home.join("data/vinosearch/models").exists());
    assert!(home.join("data/vinosearch/index.sqlite").is_file());
    std::fs::remove_dir_all(&home).unwrap();
}
