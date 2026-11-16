//! Every `` `tests::name` `` a comment cites must be a `#[test]` fn, so a rule
//! that names its test fails CI when the test is renamed or deleted. rustdoc
//! can't check these: `#[test]` fns are compiled out of the docs. See
//! docs/comment-style.md.

use std::fs;
use std::path::{Path, PathBuf};

/// The `src/` of each crate in the workspace. A citation resolves within
/// its own crate.
const CRATES: &[&str] = &["src", "crates/shared/src", "crates/settings/src"];

/// What a comment says before the tests it cites.
const PHRASE: &str = "pinned by";

#[test]
fn every_cited_test_exists() {
    let mut broken = Vec::new();
    for src in CRATES.iter().map(Path::new) {
        for file in rust_files(src) {
            let text = fs::read_to_string(&file).unwrap();
            for block in comment_blocks(&text) {
                let cited: Vec<&str> = block.lines.iter().flat_map(|l| cited_tests(l)).collect();
                if !cited.is_empty() && !block.text().contains(PHRASE) {
                    broken.push(format!(
                        "{}:{}: cites a test without \"{PHRASE}\"",
                        file.display(),
                        block.first_line
                    ));
                }
                for c in cited {
                    if !resolves(src, &file, &text, c) {
                        broken.push(format!("{}:{}: `{c}`", file.display(), block.first_line));
                    }
                }
            }
        }
    }
    assert!(broken.is_empty(), "no such test:\n{}", broken.join("\n"));
}

#[test]
fn the_check_finds_a_cited_test_and_rejects_a_missing_one() {
    let text = "/// (pinned by `tests::real`)\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn real() {}\n    fn helper() {}\n}\n";
    assert_eq!(cited_tests(text.lines().next().unwrap()), ["tests::real"]);
    assert!(is_test_fn(text, "real"));
    assert!(!is_test_fn(text, "helper"), "not a #[test]");
    assert!(!is_test_fn(text, "gone"));
}

#[test]
fn a_citation_resolves_in_its_file_its_tests_file_or_a_module_of_its_crate() {
    let src = Path::new("src");
    let index = src.join("index.rs");
    let text = fs::read_to_string(&index).unwrap();
    assert!(resolves(src, &index, &text, "tests::blob_roundtrip"));
    assert!(resolves(
        src,
        &index,
        &text,
        "index::pipeline_tests::an_indexed_tree_is_searchable"
    ));
    assert!(resolves(src, &index, &text, "index::tests::blob_roundtrip"));
    assert!(!resolves(src, &index, &text, "index::tests::no_such_test"));
    assert!(!resolves(
        src,
        &index,
        &text,
        "nowhere::tests::blob_roundtrip"
    ));
}

#[test]
fn a_citation_needs_the_phrase_in_its_comment() {
    let text = "fn a() {}\n/// Some rule\n/// (`tests::real`).\nfn b() {}\n";
    let blocks = comment_blocks(text);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].first_line, 2);
    assert!(!blocks[0].text().contains(PHRASE));
    let split = "/// A rule (pinned\n/// by `tests::real`).\n";
    assert!(comment_blocks(split)[0].text().contains(PHRASE));
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    files
}

/// A run of comment lines, numbered from 1.
struct Block<'a> {
    first_line: usize,
    lines: Vec<&'a str>,
}

impl Block<'_> {
    /// The comment's words, without the `//`, `///` or `//!` of each line.
    fn text(&self) -> String {
        let words: Vec<&str> = self
            .lines
            .iter()
            .map(|l| l.trim_start().trim_start_matches(['/', '!']).trim())
            .collect();
        words.join(" ")
    }
}

fn comment_blocks(text: &str) -> Vec<Block<'_>> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut last = None;
    for (i, line) in text.lines().enumerate() {
        if !line.trim_start().starts_with("//") {
            continue;
        }
        match blocks.last_mut() {
            Some(b) if last.is_some_and(|l| l + 1 == i) => b.lines.push(line),
            _ => blocks.push(Block {
                first_line: i + 1,
                lines: vec![line],
            }),
        }
        last = Some(i);
    }
    blocks
}

/// Code spans like `` `tests::name` ``, `` `module::tests::name` `` or
/// `` `module::pipeline_tests::name` ``.
fn cited_tests(line: &str) -> Vec<&str> {
    line.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| {
            span.rsplit_once("::")
                .is_some_and(|(module, _)| module.ends_with("tests"))
                && span
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        })
        .collect()
}

/// `tests::name` is in `file` (`text`) or its `tests.rs`;
/// `a::b::tests::name` in `src/a/b.rs`, or its tests file;
/// `a::b_tests::name` in `src/a/b_tests.rs`.
fn resolves(src: &Path, file: &Path, text: &str, cited: &str) -> bool {
    let (module, name) = cited.rsplit_once("::").unwrap();
    let candidates: Vec<PathBuf> = match module.rsplit_once("::") {
        None if module == "tests" => {
            if is_test_fn(text, name) {
                return true;
            }
            vec![tests_file_of(file)]
        }
        None => module_files(src, module),
        Some((parent, "tests")) => {
            let mut files = module_files(src, module);
            for f in module_files(src, parent) {
                files.push(tests_file_of(&f));
                files.push(f);
            }
            files
        }
        Some(_) => module_files(src, module),
    };
    candidates
        .iter()
        .any(|f| fs::read_to_string(f).is_ok_and(|text| is_test_fn(&text, name)))
}

/// `a::b` as `src/a/b.rs` or `src/a/b/mod.rs`.
fn module_files(src: &Path, module: &str) -> Vec<PathBuf> {
    let base = src.join(module.replace("::", "/"));
    vec![base.with_extension("rs"), base.join("mod.rs")]
}

/// Where `mod tests;` in `file` lives.
fn tests_file_of(file: &Path) -> PathBuf {
    let stem = file.file_stem().unwrap();
    let dir = file.parent().unwrap();
    if ["mod", "lib", "main"].iter().any(|s| stem == *s) {
        dir.join("tests.rs")
    } else {
        dir.join(stem).join("tests.rs")
    }
}

/// A `fn name(` whose attributes, above it, include `#[test]`.
fn is_test_fn(text: &str, name: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let signature = format!("fn {name}(");
    lines.iter().enumerate().any(|(i, line)| {
        line.trim_start().starts_with(&signature)
            && lines[..i]
                .iter()
                .rev()
                .map(|l| l.trim())
                .take_while(|l| l.starts_with("#[") || l.starts_with("///"))
                .any(|l| l == "#[test]")
    })
}
