//! Child processes with a deadline: fed on stdin, read on stdout, killed when
//! they take too long. For the work that comes from files anyone could have
//! written (document parsing, #84; image conversion, #79), so it costs that
//! process and not the runner.

use anyhow::{Result, bail};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run `cmd` with `input` on its stdin and return its stdout. Killed at
/// `timeout`. Err when it fails to start, is killed (by the deadline or a
/// signal, a resource limit's included), or exits non-zero, with what it
/// wrote to stderr.
pub fn run(mut cmd: Command, input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    // Each pipe on its own thread: a child that fills one while this end
    // waits on another would hang both.
    std::thread::scope(|s| {
        // A child that exits early closes its end; EPIPE here is fine (Rust
        // ignores SIGPIPE, so the write returns an error instead).
        s.spawn(move || {
            let _ = stdin.write_all(input);
        });
        let out = s.spawn(move || {
            let mut v = Vec::new();
            stdout.read_to_end(&mut v).map(|_| v)
        });
        let err = s.spawn(move || {
            let mut v = String::new();
            let _ = stderr.read_to_string(&mut v);
            v
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let out = out.join().expect("reader thread")?;
        let err = err.join().expect("reader thread");
        let err = err.trim();
        let Some(status) = status else {
            bail!("took over {} s and was stopped", timeout.as_secs());
        };
        if status.success() {
            return Ok(out);
        }
        use std::os::unix::process::ExitStatusExt;
        match status.signal() {
            Some(libc::SIGXCPU) => bail!("ran out of CPU time"),
            Some(libc::SIGSEGV) | Some(libc::SIGABRT) => {
                bail!("crashed (signal {}){}", status.signal().unwrap(), note(err))
            }
            Some(sig) => bail!("killed by signal {sig}{}", note(err)),
            None if err.is_empty() => bail!("exited with {}", status.code().unwrap_or(1)),
            None => bail!("{}", err.strip_prefix("error: ").unwrap_or(err)),
        }
    })
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
        // More than a pipe buffer, unread: the writer must not hold us up.
        let big = vec![b'x'; 4 << 20];
        let mut sh = Command::new("sh");
        sh.args(["-c", "echo ok"]);
        let out = run(sh, &big, Duration::from_secs(10)).unwrap();
        assert_eq!(out, b"ok\n");
    }
}
