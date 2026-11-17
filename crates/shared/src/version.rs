//! The version every vinoSearch binary reports: the release's number when
//! the build was stamped by the release workflow, `dev` for any other build.

/// What `--version` prints after the name and the About page shows. Release
/// builds set `VINOSEARCH_VERSION` to the package version at compile time;
/// anything else is `dev`, which `vino_ui::update::is_newer` never offers an
/// update to.
pub const VERSION: &str = stamped(option_env!("VINOSEARCH_VERSION"));

/// A stamp that disagrees with Cargo.toml is a tag that was not bumped to,
/// so the build stops instead of shipping the wrong number (pinned by
/// `tests::a_stamp_must_equal_the_package_version`).
const _: () = assert!(
    option_env!("VINOSEARCH_VERSION").is_none() || same(VERSION, env!("CARGO_PKG_VERSION")),
    "VINOSEARCH_VERSION must equal the crates' version in Cargo.toml"
);

const fn stamped(stamp: Option<&'static str>) -> &'static str {
    match stamp {
        Some(version) => version,
        None => "dev",
    }
}

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_build_reports_its_version() {
        assert_eq!(stamped(Some("11.0.0")), "11.0.0");
    }

    #[test]
    fn an_unstamped_build_reports_dev() {
        assert_eq!(stamped(None), "dev");
    }

    #[test]
    fn a_stamp_must_equal_the_package_version() {
        assert!(same("11.0.0", "11.0.0"));
        assert!(!same("11.0.1", "11.0.0"));
        assert!(!same("11", "11.0.0"));
    }
}
