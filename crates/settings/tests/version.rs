//! `vinosearch-settings --version` is `vinosearch-settings <number>` in a release build
//! (`VINOSEARCH_VERSION` set at build time) and `vinosearch-settings dev` in any other.

use std::process::Command;

fn version_of(bin: &str) -> String {
    let out = Command::new(bin).arg("--version").output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn a_release_build_reports_its_version() {
    let Some(stamp) = option_env!("VINOSEARCH_VERSION") else {
        return;
    };
    assert_eq!(
        version_of(env!("CARGO_BIN_EXE_vinosearch-settings")),
        format!("vinosearch-settings {stamp}")
    );
    assert_eq!(stamp, env!("CARGO_PKG_VERSION"));
}

#[test]
fn an_unstamped_build_reports_dev() {
    if option_env!("VINOSEARCH_VERSION").is_some() {
        return;
    }
    assert_eq!(
        version_of(env!("CARGO_BIN_EXE_vinosearch-settings")),
        "vinosearch-settings dev"
    );
}
