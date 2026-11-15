//! In the Flatpak, a runner outlives its build: flatpak keeps the files of
//! an uninstalled or replaced build for as long as it runs, and the runner
//! runs until logout. So the supervisor asks the host every CHECK_EVERY
//! which commit is installed, and stops the runner once it isn't this one:
//! for good after an uninstall, onto the new build after an update.
//!
//! Only a clear answer counts. A host that can't be asked, or an answer
//! that doesn't parse, leaves the runner alone: stopping it wrongly would
//! cost search, and leaving it costs memory.

use crate::service::flatpak_update::{Scope, installation};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use vinosearch_shared::sandbox;

const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// An uninstall is asked about twice, this far apart, before the runner
/// stops: a reinstall in between is an update instead.
const CONFIRM_AFTER: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Uninstalled,
    Updated,
}

/// The build this runs from, as `/.flatpak-info` gives it.
struct Build {
    app: &'static str,
    branch: String,
    commit: String,
    scope: Scope,
}

impl Build {
    /// None natively, and for an installation `installation` can't place.
    fn this() -> Option<Self> {
        let app = sandbox::flatpak_id()?;
        let branch = sandbox::info_value("Instance", "branch")?;
        let commit = sandbox::info_value("Instance", "app-commit")?;
        let app_path = sandbox::info_value("Instance", "app-path")?;
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let scope = installation(&app_path, app, &home).ok()?;
        Some(Build {
            app,
            branch,
            commit,
            scope,
        })
    }

    fn check(&self) -> Option<Change> {
        let out = Command::new("flatpak-spawn")
            .current_dir("/")
            // flatpak's messages are translated; "not installed" is matched.
            .args([
                "--host",
                "--env=LC_ALL=C",
                "flatpak",
                "info",
                self.scope.flag(),
            ])
            .arg("--show-commit")
            .arg(format!("{}//{}", self.app, self.branch))
            .stdin(Stdio::null())
            .output()
            .ok()?;
        verdict(
            out.status.code(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
            &self.commit,
        )
    }
}

/// What `flatpak info --show-commit` exiting with `code` says of the
/// running commit.
fn verdict(code: Option<i32>, stdout: &str, stderr: &str, running: &str) -> Option<Change> {
    match code? {
        0 => {
            let commit = stdout.trim();
            let parsed = commit.len() == 64 && commit.bytes().all(|b| b.is_ascii_hexdigit());
            (parsed && commit != running).then_some(Change::Updated)
        }
        1 if stderr.contains("not installed") => Some(Change::Uninstalled),
        _ => None,
    }
}

/// In the Flatpak, check on a thread from now on, and stop the runner when
/// its build goes. What changed, once it has.
pub fn spawn() -> Arc<OnceLock<Change>> {
    let changed = Arc::new(OnceLock::new());
    let Some(build) = Build::this() else {
        return changed;
    };
    let set = changed.clone();
    std::thread::spawn(move || {
        let change = loop {
            std::thread::sleep(CHECK_EVERY);
            match build.check() {
                Some(Change::Uninstalled) => {
                    std::thread::sleep(CONFIRM_AFTER);
                    if let Some(change) = build.check() {
                        break change;
                    }
                }
                Some(Change::Updated) => break Change::Updated,
                None => {}
            }
        };
        match change {
            Change::Uninstalled => eprintln!("this build was uninstalled; stopping"),
            Change::Updated => eprintln!("a newer build is installed; restarting on it"),
        }
        let _ = set.set(change);
        if let Err(e) = crate::service::start::stop_runner() {
            eprintln!("stopping the runner: {e:#}");
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = "f2640a67caab035fb7e2e41540c176af3990bb785a81828826127ea4f3d7c603";
    const NEWER: &str = "bccee4f4992ddc74e6545c489a9cd5875d2ffd836ef7cfde7d87ea1b2e86b9c6";

    #[test]
    fn the_same_commit_installed_changes_nothing() {
        assert_eq!(verdict(Some(0), &format!("{RUNNING}\n"), "", RUNNING), None);
    }

    #[test]
    fn another_commit_installed_is_an_update() {
        assert_eq!(
            verdict(Some(0), &format!("{NEWER}\n"), "", RUNNING),
            Some(Change::Updated)
        );
    }

    #[test]
    fn not_installed_is_an_uninstall() {
        let stderr = "error: dev.vinosearch.VinoSearch/*unspecified*/stable not installed\n";
        assert_eq!(
            verdict(Some(1), "", stderr, RUNNING),
            Some(Change::Uninstalled)
        );
    }

    #[test]
    fn an_unclear_answer_leaves_the_runner_alone() {
        // flatpak-spawn without a portal, a signal, output that isn't a
        // commit, another error.
        assert_eq!(verdict(Some(1), "", "Portal call failed", RUNNING), None);
        assert_eq!(verdict(None, "", "", RUNNING), None);
        assert_eq!(verdict(Some(0), "", "", RUNNING), None);
        assert_eq!(verdict(Some(0), "warning: x\n", "", RUNNING), None);
        assert_eq!(verdict(Some(2), "", "not installed", RUNNING), None);
    }
}
