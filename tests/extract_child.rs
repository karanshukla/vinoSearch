//! `vinosearch extract`, the child the indexer runs for every document (#84),
//! through the built binary.

use std::io::Write;
use std::process::{Command, Stdio};

fn extract(name: &str, input: &[u8]) -> std::process::Output {
    extract_with(&[name], input)
}

fn extract_with(args: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vinosearch"))
        .arg("extract")
        .args(args)
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
    let out: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        out,
        serde_json::json!({ "parts": 1, "chunks": [[2, "n,payee\n1,shop\n2,cafe\n"]] })
    );
}

#[test]
fn a_run_of_parts_comes_back_with_the_count() {
    // 1,000 rows of ~600 bytes: a chunk each, so 1,000 chunks and 334 parts.
    let mut csv = String::from("n,text\n");
    for i in 0..1000 {
        csv.push_str(&format!("{i},{}\n", "w".repeat(600 + i % 7)));
    }
    let out = extract_with(
        &["/x/a.csv", "--from", "300", "--count", "30"],
        csv.as_bytes(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let chunks = out["chunks"].as_array().unwrap();
    assert!(out["parts"].as_u64().unwrap() > 300, "{}", out["parts"]);
    assert!(!chunks.is_empty() && chunks.len() <= 90, "{}", chunks.len());
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
