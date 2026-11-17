//! Child processes with a deadline: fed on stdin, read on stdout, killed when
//! they take too long. For the work that comes from files anyone could have
//! written (document parsing, image conversion), so it costs that process
//! and not the runner.

use anyhow::{Result, bail};
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Run `cmd` with `input` on its stdin and return its stdout. Killed at
/// `timeout`. Err when it fails to start, is killed (by the deadline or a
/// signal, a resource limit's included), or exits non-zero, with what it
/// wrote to stderr. Each pipe is served on its own thread: a child that fills
/// one while this end waits on another would hang both.
pub fn run(mut cmd: Command, input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    std::thread::scope(|s| {
        s.spawn(move || feed(stdin, input));
        let out = s.spawn(move || {
            let mut v = Vec::new();
            stdout.read_to_end(&mut v).map(|_| v)
        });
        let err = s.spawn(move || {
            let mut v = String::new();
            let _ = stderr.read_to_string(&mut v);
            v
        });
        let status = wait_until(&mut child, Instant::now() + timeout)?;
        let out = out.join().expect("reader thread")?;
        let err = err.join().expect("reader thread");
        let err = err.trim();
        let Some(status) = status else {
            bail!("took over {} s and was stopped", timeout.as_secs());
        };
        if status.success() {
            return Ok(out);
        }
        Err(failure(status, err))
    })
}

/// A child that exits early closes its end; the write's EPIPE is fine (Rust
/// ignores SIGPIPE, so the write returns an error instead).
fn feed(mut stdin: ChildStdin, input: &[u8]) {
    let _ = stdin.write_all(input);
}

/// The child's exit status, or None once it was killed at `deadline`.
fn wait_until(child: &mut Child, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Why a child that exited with `status` failed, with what it wrote to stderr.
fn failure(status: ExitStatus, err: &str) -> anyhow::Error {
    match status.signal() {
        Some(libc::SIGXCPU) => anyhow::anyhow!("ran out of CPU time"),
        Some(sig @ (libc::SIGSEGV | libc::SIGABRT)) => {
            anyhow::anyhow!("crashed (signal {sig}){}", note(err))
        }
        Some(sig) => anyhow::anyhow!("killed by signal {sig}{}", note(err)),
        None if err.is_empty() => anyhow::anyhow!("exited with {}", status.code().unwrap_or(1)),
        None => anyhow::anyhow!("{}", err.strip_prefix("error: ").unwrap_or(err)),
    }
}

fn note(err: &str) -> String {
    if err.is_empty() {
        String::new()
    } else {
        format!(": {err}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdout_comes_back_and_a_slow_child_is_stopped() {
        let out = run(Command::new("cat"), b"hello", Duration::from_secs(10)).unwrap();
        assert_eq!(out, b"hello");
        let t0 = Instant::now();
        let mut sleep = Command::new("sleep");
        sleep.arg("30");
        let e = run(sleep, b"", Duration::from_millis(200)).unwrap_err();
        assert!(e.to_string().contains("stopped"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_failing_child_reports_its_stderr() {
        let mut sh = Command::new("sh");
        sh.args(["-c", "echo 'error: encrypted' >&2; exit 3"]);
        let e = run(sh, b"", Duration::from_secs(10)).unwrap_err();
        assert_eq!(e.to_string(), "encrypted");
        let mut sh = Command::new("sh");
        sh.args(["-c", "exit 2"]);
        let e = run(sh, b"", Duration::from_secs(10)).unwrap_err();
        assert_eq!(e.to_string(), "exited with 2");
    }

    #[test]
    fn a_child_that_ignores_its_input_still_finishes() {
        let more_than_a_pipe_buffer = vec![b'x'; 4 << 20];
        let mut sh = Command::new("sh");
        sh.args(["-c", "echo ok"]);
        let out = run(sh, &more_than_a_pipe_buffer, Duration::from_secs(10)).unwrap();
        assert_eq!(out, b"ok\n");
    }
}
