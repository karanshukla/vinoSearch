//! How the runner starts and stays up, with no service manager: the XDG
//! autostart entry starts `vinosearch krunner` at login, D-Bus activation
//! starts it on first use when it isn't running, and `krunner` itself is a
//! small parent that runs the runner as a child and starts it again after a
//! crash. The same everywhere, systemd or not.
//!
//! Only one runner: whichever `krunner` takes the instance lock runs it; any
//! other started meanwhile (the autostart entry and an activation racing at
//! login) waits for the bus name and exits. Owning the name stays the last
//! word: a runner that can't take it doesn't start.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};
use vinosearch_shared::dbus::{CONTROL_INTERFACE, CONTROL_PATH, ICON, SERVICE};

/// The systemd user unit installs before this one wrote and enabled.
const OLD_UNIT: &str = "vinosearch.service";
/// The autostart entry, under the XDG config directory.
const AUTOSTART: &str = "autostart/dev.vinosearch.Runner.desktop";
/// Exit code of a runner that stopped before it owned the bus name: a
/// broken install or another runner, which starting it again won't fix.
pub const NOT_STARTED: u8 = 3;
/// A runner that crashes is started again after 1 s, then 2, 4... up to a
/// minute while it keeps crashing; one that ran 5 minutes starts the count
/// again.
const RESTART_FIRST: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(60);
const HEALTHY: Duration = Duration::from_secs(5 * 60);
/// The log is moved to `.old` past this, when the runner (re)starts.
const LOG_MAX: u64 = 1 << 20;

pub fn config_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".config"),
    })
}

pub fn data_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".local/share"),
    })
}

/// Where the runner's output goes when it isn't started from a terminal.
pub fn log_path() -> Result<PathBuf> {
    let state = match std::env::var_os("XDG_STATE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".local/state"),
    };
    Ok(state.join("vinosearch/service.log"))
}

pub fn autostart_path() -> Result<PathBuf> {
    Ok(config_home()?.join(AUTOSTART))
}

/// The autostart entry running `exec`, an Exec value already quoted.
pub fn autostart_entry(exec: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=vinoSearch\n\
         Comment=Semantic file search: keeps the index current and answers searches\n\
         Icon={ICON}\nExec={exec}\nTerminal=false\nNoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

/// `vinosearch krunner`: run the runner, `worker` (`vinosearch krunner
/// --worker`), unless another `krunner` already does. Returns the runner's
/// last exit, or None when another runs it.
pub fn supervise(worker: Command) -> Result<Option<ExitStatus>> {
    let lock = instance_lock_path()?;
    let Some(_held) = single_instance(&lock, Duration::from_secs(30))? else {
        return Ok(None);
    };
    supervised(worker).map(Some)
}

/// `$XDG_RUNTIME_DIR`, which logind empties at logout, else the cache.
fn instance_lock_path() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => match std::env::var_os("XDG_CACHE_HOME") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".cache"),
        },
    };
    Ok(dir.join(format!("{SERVICE}.lock")))
}

/// The instance lock, held while the file is open; None once the runner
/// another `krunner` holds it for owns the bus name, or after `wait`. Tried
/// again meanwhile, so a `krunner` started while the last one exits (a
/// restart) takes over from it.
fn single_instance(path: &Path, wait: Duration) -> Result<Option<File>> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(e).with_context(|| format!("locking {}", path.display()));
        }
        // An activation that started this one is answered once the other's
        // runner owns the name: exiting before then would fail it.
        if Instant::now() >= deadline || owned(SERVICE) == Some(true) {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What to do once the runner exits.
#[derive(Debug, PartialEq, Eq)]
enum After {
    Stop,
    Restart,
}

/// As systemd's Restart=on-failure: not after a clean exit or a stop
/// signal; nor when it never came up, as starting it again won't help.
fn after(status: ExitStatus) -> After {
    match (status.code(), status.signal()) {
        (Some(0), _) => After::Stop,
        (Some(c), _) if c == i32::from(NOT_STARTED) => After::Stop,
        (None, Some(libc::SIGTERM | libc::SIGINT | libc::SIGHUP | libc::SIGPIPE)) => After::Stop,
        _ => After::Restart,
    }
}

/// The wait before starting again a runner that crashed after `ran`, the
/// last wait being `last`.
fn backoff(last: Option<Duration>, ran: Duration) -> Duration {
    match last {
        Some(d) if ran < HEALTHY => (d * 2).min(RESTART_MAX),
        _ => RESTART_FIRST,
    }
}

fn supervised(mut worker: Command) -> Result<ExitStatus> {
    let log = (!std::io::stderr().is_terminal())
        .then(log_path)
        .transpose()?;
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: prctl and getppid are async-signal-safe, and nothing allocates.
    unsafe {
        worker.pre_exec(move || {
            // Stopping this process (logout, a signal) stops the runner.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    let mut wait = None;
    loop {
        if let Some(log) = &log {
            log_to(log)?;
        }
        let started = Instant::now();
        let status = worker.status().context("starting the runner")?;
        if after(status) == After::Stop {
            return Ok(status);
        }
        let d = backoff(wait, started.elapsed());
        eprintln!("the runner stopped ({status}); starting it again in {d:?}");
        std::thread::sleep(d);
        wait = Some(d);
    }
}

/// Send this process's stdout and stderr, which the runner inherits, to the
/// end of `path`, moving it to `.old` first when it is over LOG_MAX.
fn log_to(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > LOG_MAX) {
        fs::rename(path, path.with_extension("log.old"))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: dup2 onto the standard descriptors, from one this owns.
        if unsafe { libc::dup2(file.as_raw_fd(), fd) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Whether something owns `name`; None without a session bus.
pub fn owned(name: &str) -> Option<bool> {
    zbus::block_on(async {
        let conn = zbus::Connection::session().await.ok()?;
        let bus = zbus::fdo::DBusProxy::new(&conn).await.ok()?;
        bus.name_has_owner(name.try_into().ok()?).await.ok()
    })
}

/// Wait up to `wait` for `name` to be owned (or not); true once it is.
fn wait_owned(name: &str, want: bool, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if owned(name) == Some(want) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stop whatever owns `name`: call `quit` (path, interface, method) on it,
/// or signal it when it has no such method (a runner from before Quit), and
/// wait for the name to go. True when something was running.
pub fn stop(name: &str, quit: (&str, &str, &str)) -> Result<bool> {
    if owned(name) != Some(true) {
        return Ok(false);
    }
    let (path, interface, method) = quit;
    let unknown = zbus::block_on(async {
        let conn = zbus::Connection::session().await?;
        let reply = conn
            .call_method(Some(name), path, Some(interface), method, &())
            .await;
        // Exiting can beat the reply, which the bus then answers NoReply for.
        Ok::<_, zbus::Error>(matches!(
            reply,
            Err(zbus::Error::MethodError(ref e, _, _))
                if e.as_str().starts_with("org.freedesktop.DBus.Error.Unknown")
        ))
    })?;
    if unknown {
        let pid = zbus::block_on(async {
            let conn = zbus::Connection::session().await?;
            let bus = zbus::fdo::DBusProxy::new(&conn).await?;
            Ok::<_, zbus::Error>(bus.get_connection_unix_process_id(name.try_into()?).await?)
        })?;
        // SAFETY: kill with a pid the bus gave for the name's owner.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    if !wait_owned(name, false, Duration::from_secs(10)) {
        anyhow::bail!("{name} is still running 10 s after being asked to stop");
    }
    Ok(true)
}

/// Stop the runner, through Control1's Quit.
pub fn stop_runner() -> Result<bool> {
    stop(SERVICE, (CONTROL_PATH, CONTROL_INTERFACE, "Quit"))
}

/// Have the bus start `name` from its activation file, rereading the files
/// first so one just written counts. The bus starts it, so it doesn't
/// depend on the process asking (a terminal, the settings window).
pub fn start(name: &str) -> Result<()> {
    zbus::block_on(async {
        let conn = zbus::Connection::session().await?;
        let bus = zbus::fdo::DBusProxy::new(&conn).await?;
        // dbus-broker rereads them only when asked; dbus-daemon also on its
        // own.
        let _ = bus.reload_config().await;
        bus.start_service_by_name(name.try_into()?, 0).await?;
        Ok::<_, zbus::Error>(())
    })?;
    Ok(())
}

/// Stop, disable and remove the systemd user unit an older install wrote,
/// so it and the autostart entry never both start the runner. Quiet where
/// there is no systemctl. The unit's path when there was one.
pub fn remove_old_unit() -> Result<Option<PathBuf>> {
    let dir = config_home()?.join("systemd/user");
    let unit = dir.join(OLD_UNIT);
    if !unit.exists() {
        return Ok(None);
    }
    let systemctl = |args: &[&str]| {
        Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    systemctl(&["disable", "--now", OLD_UNIT]);
    fs::remove_file(&unit)?;
    // What `disable` removes, for when systemctl couldn't.
    let wants = dir.join("default.target.wants").join(OLD_UNIT);
    if wants.symlink_metadata().is_ok() {
        fs::remove_file(&wants)?;
    }
    systemctl(&["daemon-reload"]);
    Ok(Some(unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    fn killed(signal: i32) -> ExitStatus {
        ExitStatus::from_raw(signal)
    }

    #[test]
    fn a_crash_restarts_the_runner_and_a_stop_does_not() {
        assert_eq!(after(exited(0)), After::Stop);
        assert_eq!(after(exited(NOT_STARTED.into())), After::Stop);
        assert_eq!(after(killed(libc::SIGTERM)), After::Stop);
        assert_eq!(after(killed(libc::SIGINT)), After::Stop);
        assert_eq!(after(exited(1)), After::Restart);
        assert_eq!(after(exited(101)), After::Restart);
        assert_eq!(after(killed(libc::SIGSEGV)), After::Restart);
        assert_eq!(after(killed(libc::SIGABRT)), After::Restart);
        assert_eq!(after(killed(libc::SIGKILL)), After::Restart);
    }

    #[test]
    fn crashes_in_a_row_wait_longer_up_to_a_minute() {
        let quick = Duration::from_secs(2);
        let mut d = backoff(None, quick);
        assert_eq!(d, RESTART_FIRST);
        let mut waits = vec![d];
        for _ in 0..8 {
            d = backoff(Some(d), quick);
            waits.push(d);
        }
        let secs: Vec<u64> = waits.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(backoff(Some(RESTART_MAX), HEALTHY), RESTART_FIRST);
    }

    #[test]
    fn one_krunner_takes_the_lock_and_the_next_waits_for_it() {
        let dir = std::env::temp_dir().join(format!("vinosearch-lock-{}", std::process::id()));
        let path = dir.join("runner.lock");
        let held = single_instance(&path, Duration::ZERO).unwrap();
        assert!(held.is_some());
        // Another open file description, as another process would have.
        assert!(single_instance(&path, Duration::ZERO).unwrap().is_none());
        drop(held);
        assert!(single_instance(&path, Duration::ZERO).unwrap().is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_autostart_entry_runs_the_runner_and_stays_out_of_menus() {
        let entry = autostart_entry(r#""/opt/my apps/vinosearch" "krunner""#);
        assert!(
            entry.contains("\nExec=\"/opt/my apps/vinosearch\" \"krunner\"\n"),
            "{entry}"
        );
        assert!(entry.contains("\nNoDisplay=true\n"), "{entry}");
    }
}
