//! `krunner` (the service, and installing it) and `watch`.

use crate::service::{krunner, power, start, watch, worker};
use anyhow::Result;

/// What `krunner` was asked for; with no flag it supervises the runner.
pub enum KrunnerMode {
    Install,
    Uninstall,
    Worker,
    Supervise,
}

impl KrunnerMode {
    pub fn of(install: bool, uninstall: bool, worker: bool) -> Self {
        if install {
            Self::Install
        } else if uninstall {
            Self::Uninstall
        } else if worker {
            Self::Worker
        } else {
            Self::Supervise
        }
    }
}

/// Serve D-Bus as the runner, then leave without unwinding.
///
/// The worker's thread is still running, in OpenVINO as likely as not:
/// returning would run the process's destructors under it, which segfaults,
/// and a runner asked to quit that dies of a signal is started again. Left
/// as a kill leaves it, which the index (WAL) is written for.
pub fn serve_and_exit(options: worker::Options) -> ! {
    use std::io::Write;
    let code = match krunner::serve(options) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            i32::from(failure_code(&e))
        }
    };
    let _ = std::io::stderr().flush();
    let _ = std::io::stdout().flush();
    // SAFETY: _exit takes no pointers and doesn't return.
    unsafe { libc::_exit(code) }
}

/// The exit status for `error`: `start::NOT_STARTED` tells the supervisor
/// not to start the runner again.
pub fn failure_code(error: &anyhow::Error) -> u8 {
    if error.downcast_ref::<krunner::NotStarted>().is_some() {
        start::NOT_STARTED
    } else {
        1
    }
}

/// Index and watch in the foreground, without D-Bus.
pub fn watch(options: worker::Options) -> Result<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    worker::prepare(&options.status, &options.file)?;
    power::spawn(tx.clone(), options.status.clone());
    let _watcher = watch::spawn(
        tx,
        options.config.clone(),
        options.file.clone(),
        options.status.clone(),
    )?;
    worker::run(rx, options)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_that_could_not_start_exits_with_not_started() {
        let error = anyhow::Error::new(krunner::NotStarted(anyhow::anyhow!("no session bus")));
        assert_eq!(failure_code(&error), start::NOT_STARTED);
        assert_eq!(failure_code(&error.context("serving")), start::NOT_STARTED);
    }

    #[test]
    fn any_other_failure_exits_with_one() {
        assert_eq!(failure_code(&anyhow::anyhow!("boom")), 1);
    }

    #[test]
    fn krunner_without_a_flag_supervises() {
        assert!(matches!(
            KrunnerMode::of(false, false, false),
            KrunnerMode::Supervise
        ));
    }

    #[test]
    fn each_krunner_flag_picks_its_mode() {
        assert!(matches!(
            KrunnerMode::of(true, false, false),
            KrunnerMode::Install
        ));
        assert!(matches!(
            KrunnerMode::of(false, true, false),
            KrunnerMode::Uninstall
        ));
        assert!(matches!(
            KrunnerMode::of(false, false, true),
            KrunnerMode::Worker
        ));
    }
}
