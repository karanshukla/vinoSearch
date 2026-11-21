//! Child processes with a deadline: fed on stdin, read on stdout, killed when
//! they take too long. For the work that comes from files anyone could have
//! written (document parsing, image conversion), so it costs that process
//! and not the runner.

use anyhow::{Result, bail};
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Maximum stderr retained from a child process (64 KiB); any excess is drained
/// and discarded so the child never blocks on a full stderr pipe.
const MAX_STDERR_BYTES: u64 = 64 * 1024;
/// Default maximum stdout buffered from a child process (64 MiB).
const MAX_STDOUT_BYTES: u64 = 64 * 1024 * 1024;

/// Run `cmd` with `input` on its stdin and return its stdout. Killed at
/// `timeout` or if stdout exceeds `MAX_STDOUT_BYTES`. Err when it fails to
/// start, is killed (by the deadline or a signal, a resource limit's included),
/// or exits non-zero, with what it wrote to stderr. Each pipe is served on its
/// own thread: a child that fills one while this end waits on another would
/// hang both.
pub fn run(cmd: Command, input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    run_with_max_stdout(cmd, input, timeout, MAX_STDOUT_BYTES)
}

fn run_with_max_stdout(
    mut cmd: Command,
    input: &[u8],
    timeout: Duration,
    max_stdout: u64,
) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    std::thread::scope(|s| {
        let overflowed = AtomicBool::new(false);
        let overflow_flag = &overflowed;
        s.spawn(move || feed(stdin, input));
        let out = s.spawn(move || -> std::io::Result<Vec<u8>> {
            let mut v = Vec::new();
            (&mut stdout).take(max_stdout + 1).read_to_end(&mut v)?;
            if v.len() as u64 > max_stdout {
                overflow_flag.store(true, Ordering::Release);
                let _ = std::io::copy(&mut stdout, &mut std::io::sink());
            }
            Ok(v)
        });
        let err = s.spawn(move || {
            let mut v = Vec::new();
            let _ = (&mut stderr).take(MAX_STDERR_BYTES).read_to_end(&mut v);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            String::from_utf8_lossy(&v).into_owned()
        });
        let status = wait_until(&mut child, Instant::now() + timeout, &overflowed)?;
        let out = out.join().expect("reader thread")?;
        let err = err.join().expect("reader thread");
        let err = err.trim();
        if overflowed.load(Ordering::Acquire) || out.len() as u64 > max_stdout {
            bail!("output exceeded {max_stdout} bytes");
        }
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

/// The child's exit status, or None once it was killed at `deadline` or
/// `overflowed`.
fn wait_until(
    child: &mut Child,
    deadline: Instant,
    overflowed: &AtomicBool,
) -> std::io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if overflowed.load(Ordering::Acquire) || Instant::now() >= deadline {
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

    #[test]
    fn excessive_stderr_is_capped_and_drained_without_blocking() {
        let mut sh = Command::new("sh");
        sh.args(["-c", "head -c 262144 /dev/zero | tr '\\0' 'e' >&2; echo done; exit 1"]);
        let e = run(sh, b"", Duration::from_secs(10)).unwrap_err();
        assert!(e.to_string().len() <= MAX_STDERR_BYTES as usize);
        assert_eq!(e.to_string().len(), MAX_STDERR_BYTES as usize);
    }

    #[test]
    fn stdout_over_limit_stops_child_and_returns_error() {
        let mut yes = Command::new("yes");
        let e = run_with_max_stdout(yes, b"", Duration::from_secs(10), 4096).unwrap_err();
        assert!(e.to_string().contains("exceeded 4096 bytes"), "{e}");
    }
}
