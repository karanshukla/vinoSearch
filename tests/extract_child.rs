//! `vinosearch extract`, the child the indexer runs for every document (#84),
//! through the built binary.

use std::io::Write;
use std::process::{Command, Stdio};

fn extract(name: &str, input: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vinosearch"))
        .args(["extract", name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn chunks_come_back_as_json() {
    let out = extract("/x/a.csv", b"n,payee\n1,shop\n2,cafe\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let chunks: Vec<(usize, String)> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(chunks, [(2, "n,payee\n1,shop\n2,cafe\n".to_owned())]);
}

#[test]
fn a_broken_document_is_an_error_on_stderr() {
    let out = extract("/x/a.pdf", b"not a pdf");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("error: "),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_document_over_the_size_cap_is_refused() {
    // Larger than the indexer would ever send (MAX_DOCUMENT_BYTES).
    let big = vec![b'a'; (32 << 20) + 1];
    let out = extract("/x/a.csv", &big);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bytes"));
}
