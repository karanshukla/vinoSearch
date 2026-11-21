//! Source code, indexed apart from text and only when it's turned on: which
//! files are code, which of those are left out, and chunks that
//! follow the code's own definitions.
//!
//! Left out by path: files git ignores (`.gitignore`, `.ignore`, `.git/info/exclude`, the global
//! excludes file, inside a git work tree), vendored directories, generated files by name, and files
//! named for secrets. Left out by content: generated files by header, minified ones, and anything
//! holding a private key or API token.

use crate::index::CHUNK_CHARS;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Code by extension; prose and markup are text, in `index::TEXT_EXTENSIONS`.
pub const EXTENSIONS: &[&str] = &[
    "rs", "py", "pyi", "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "vue", "svelte", "go",
    "c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx", "m", "mm", "java", "kt", "kts", "scala",
    "swift", "dart", "rb", "php", "pl", "pm", "lua", "zig", "nix", "cs", "fs", "ex", "exs", "erl",
    "hs", "ml", "mli", "clj", "r", "jl", "sh", "bash", "zsh", "fish", "ps1", "css", "scss", "sass",
    "less", "sql", "qml", "cmake", "gd", "glsl", "wgsl", "proto",
];
/// Directories of other people's code checked in, inside a git work tree. `vendor` and
/// `node_modules` are skipped for every kind (`index::SKIP_DIRS`).
const VENDORED_DIRS: &[&str] = &[
    "third_party",
    "third-party",
    "thirdparty",
    "3rdparty",
    "vendored",
];
const GENERATED_SUFFIXES: &[&str] = &[
    ".pb.go",
    ".pb.cc",
    ".pb.h",
    "_pb2.py",
    "_pb2.pyi",
    "_pb2_grpc.py",
    "_grpc.pb.go",
    ".g.dart",
    ".freezed.dart",
    ".designer.cs",
    ".g.cs",
    ".min.js",
    ".min.css",
    ".bundle.js",
    ".d.ts",
];
const SECRET_NAMES: &[&str] = &["secret", "credential", "password", "passwd"];
const IGNORES_TTL: Duration = Duration::from_secs(60);

pub fn is_code(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

pub fn code_query(query: &str) -> Option<&str> {
    let q = query.trim_start();
    let head = q.get(..5)?;
    head.eq_ignore_ascii_case("code:").then(|| q[5..].trim())
}

/// The ignore files of the directories looked at, read once a minute at most; shared by clones of a
/// `FileTypes`.
#[derive(Default)]
pub struct Ignores {
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    read: Option<Instant>,
    repos: HashMap<PathBuf, Option<PathBuf>>,
    dirs: HashMap<PathBuf, Arc<Vec<Gitignore>>>,
    repo_wide: HashMap<PathBuf, Arc<Vec<Gitignore>>>,
}

impl std::fmt::Debug for Ignores {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Ignores")
    }
}

impl Ignores {
    pub fn left_out(&self, path: &Path) -> bool {
        left_out_by_name(path) || self.ignored(path)
    }

    fn ignored(&self, path: &Path) -> bool {
        let Some(dir) = path.parent() else {
            return false;
        };
        let mut cache = self.cache.lock().unwrap();
        if cache.read.is_none_or(|t| t.elapsed() > IGNORES_TTL) {
            *cache = Cache {
                read: Some(Instant::now()),
                ..Cache::default()
            };
        }
        let Some(repo) = cache.repo_of(dir) else {
            return false;
        };
        let Ok(inside) = path.strip_prefix(&repo) else {
            return false;
        };
        let dirs = inside.parent().map(Path::components).into_iter().flatten();
        if dirs
            .clone()
            .any(|c| VENDORED_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
        {
            return true;
        }
        for d in deepest_first(dir, &repo) {
            let rel = path.strip_prefix(d).expect("an ancestor");
            if let Some(ignored) = verdict(&cache.dir(d), rel) {
                return ignored;
            }
        }
        verdict(&cache.repo_wide(&repo), inside).unwrap_or(false)
    }
}

/// `dir` and its ancestors up to `repo`, deepest first, so a nearer ignore file wins (pinned by
/// `tests::a_nearer_ignore_files_rule_wins`).
fn deepest_first<'a>(dir: &'a Path, repo: &Path) -> Vec<&'a Path> {
    let mut levels: Vec<&Path> = dir
        .ancestors()
        .take_while(|d| d.starts_with(repo))
        .collect();
    levels.dedup();
    levels
}

fn verdict(rules: &[Gitignore], rel: &Path) -> Option<bool> {
    rules
        .iter()
        .find_map(|gi| match gi.matched_path_or_any_parents(rel, false) {
            Match::Ignore(_) => Some(true),
            Match::Whitelist(_) => Some(false),
            Match::None => None,
        })
}

impl Cache {
    /// The nearest directory holding `.git` (a directory, or a file for a worktree or submodule).
    fn repo_of(&mut self, dir: &Path) -> Option<PathBuf> {
        if let Some(r) = self.repos.get(dir) {
            return r.clone();
        }
        let found = if dir.join(".git").exists() {
            Some(dir.to_owned())
        } else {
            dir.parent().and_then(|p| self.repo_of(p))
        };
        self.repos.insert(dir.to_owned(), found.clone());
        found
    }

    fn dir(&mut self, dir: &Path) -> Arc<Vec<Gitignore>> {
        self.dirs
            .entry(dir.to_owned())
            .or_insert_with(|| {
                Arc::new(
                    [".ignore", ".gitignore"]
                        .iter()
                        .map(|f| dir.join(f))
                        .filter(|f| f.is_file())
                        .map(|f| Gitignore::new(f).0)
                        .filter(|g| !g.is_empty())
                        .collect(),
                )
            })
            .clone()
    }

    fn repo_wide(&mut self, repo: &Path) -> Arc<Vec<Gitignore>> {
        self.repo_wide
            .entry(repo.to_owned())
            .or_insert_with(|| {
                let files = [
                    Some(repo.join(".git/info/exclude")),
                    ignore::gitignore::gitconfig_excludes_path(),
                ];
                Arc::new(
                    files
                        .into_iter()
                        .flatten()
                        .filter(|f| f.is_file())
                        .filter_map(|f| {
                            let mut b = GitignoreBuilder::new(repo);
                            b.add(f);
                            b.build().ok()
                        })
                        .filter(|g| !g.is_empty())
                        .collect(),
                )
            })
            .clone()
    }
}

fn left_out_by_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    GENERATED_SUFFIXES.iter().any(|s| name.ends_with(s))
        || name.contains(".generated.")
        || name.contains("_generated.")
        || SECRET_NAMES.iter().any(|s| name.contains(s))
}

pub fn unwanted(text: &str) -> Option<&'static str> {
    if generated(text) {
        Some("generated")
    } else if minified(text) {
        Some("minified")
    } else if private_key(text) {
        Some("holds a private key")
    } else if token(text) {
        Some("holds an API token")
    } else {
        None
    }
}

/// A "do not edit" header near the top: Go's `// Code generated ... DO NOT EDIT.`, `@generated`,
/// protoc's and most generators'.
fn generated(text: &str) -> bool {
    text.lines().take(12).any(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("do not edit")
            || l.contains("@generated")
            || l.contains("autogenerated")
            || l.contains("auto-generated")
            || l.contains("code generated by")
    })
}

fn minified(text: &str) -> bool {
    text.len() > 4096 && text.lines().count() * 400 < text.len()
}

fn private_key(text: &str) -> bool {
    text.match_indices("-----BEGIN ").any(|(i, _)| {
        let line = text[i..].lines().next().unwrap_or_default();
        line.contains("PRIVATE KEY")
    })
}

fn token(text: &str) -> bool {
    const PREFIXES: &[(&str, usize)] = &[
        ("AKIA", 16),
        ("ghp_", 30),
        ("gho_", 30),
        ("ghs_", 30),
        ("ghu_", 30),
        ("github_pat_", 30),
        ("glpat-", 20),
        ("xoxb-", 20),
        ("xoxp-", 20),
        ("sk_live_", 20),
        ("sk-proj-", 30),
        ("AIza", 35),
    ];
    PREFIXES.iter().any(|(prefix, len)| {
        text.match_indices(prefix).any(|(i, _)| {
            starts_a_word(text, i)
                && text[i + prefix.len()..]
                    .chars()
                    .take_while(|&c| token_char(c))
                    .count()
                    >= *len
        })
    })
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Whether byte `i` of `text` isn't mid-word (pinned by
/// `tests::a_token_on_its_own_is_a_token`,
/// `tests::a_token_inside_a_longer_word_is_not_a_token`).
fn starts_a_word(text: &str, i: usize) -> bool {
    text[..i].chars().next_back().is_none_or(|c| !token_char(c))
}

fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// A line that starts a definition in most languages (`fn`, `def`, `class`, `func`...), after any
/// modifiers, which may take arguments as `pub(crate)` and `extern "C"` do (pinned by
/// `tests::a_definition_after_modifiers_with_arguments_starts_one`,
/// `tests::a_statement_is_not_a_definition`).
fn starts_definition(line: &str) -> bool {
    const MODIFIERS: &[&str] = &[
        "pub",
        "pub(crate)",
        "pub(super)",
        "export",
        "default",
        "async",
        "static",
        "public",
        "private",
        "protected",
        "internal",
        "abstract",
        "final",
        "override",
        "unsafe",
        "extern",
        "inline",
        "virtual",
        "open",
        "sealed",
        "data",
        "suspend",
        "local",
        "const",
    ];
    const KEYWORDS: &[&str] = &[
        "fn",
        "def",
        "class",
        "struct",
        "enum",
        "trait",
        "impl",
        "mod",
        "func",
        "function",
        "interface",
        "type",
        "module",
        "namespace",
        "object",
        "record",
        "macro_rules!",
        "sub",
        "union",
        "fun",
        "defmodule",
        "defp",
        "template",
    ];
    let mut words = line.split_whitespace();
    for w in words.by_ref() {
        let w = w.split('(').next().unwrap_or(w);
        if KEYWORDS.contains(&w) {
            return true;
        }
        if !(MODIFIERS.contains(&w) || w.starts_with('"')) {
            return false;
        }
    }
    false
}

fn leads_in(line: &str) -> bool {
    let t = line.trim_start();
    ["//", "#", "/*", "*", "@", "--", ";", "\"\"\"", "'''"]
        .iter()
        .any(|p| t.starts_with(p))
}

fn breaks(lines: &[&str], base: usize) -> Vec<usize> {
    let mut out = vec![0];
    for i in 1..lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || indent(line) != base {
            continue;
        }
        let after_blank = lines[i - 1].trim().is_empty();
        if after_blank {
            out.push(i);
        } else if starts_definition(line.trim_start()) && !leads_in(line) {
            let start = with_lead_in(lines, i, base);
            let split_off_after_a_blank_line = start <= *out.last().expect("starts at 0");
            if !split_off_after_a_blank_line {
                out.push(start);
            }
        }
    }
    out
}

fn with_lead_in(lines: &[&str], i: usize, base: usize) -> usize {
    let mut start = i;
    while start > 0
        && !lines[start - 1].trim().is_empty()
        && indent(lines[start - 1]) == base
        && leads_in(lines[start - 1])
    {
        start -= 1;
    }
    start
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    line: usize,
    lines: usize,
    inside: Option<usize>,
}

const MAX_DEPTH: usize = 32;

/// `lines` as units no longer than CHUNK_CHARS where the code allows: whole definitions, or a long
/// one split at its inner definitions or paragraphs, its first line kept with the first of them.
/// Inside a definition indented `header`, units sit at the least indentation deeper, so a closing
/// brace back at `header` stays with the last.
fn units(lines: &[&str], first: usize, header: Option<usize>, depth: usize, out: &mut Vec<Span>) {
    if depth > MAX_DEPTH {
        return;
    }
    let size = |r: &[&str]| r.iter().map(|l| l.len() + 1).sum::<usize>();
    let indents = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent(l));
    let base = match header {
        Some(h) => {
            let Some(deeper) = indents.filter(|&i| i > h).min() else {
                return;
            };
            deeper
        }
        None => indents.min().unwrap_or(0),
    };
    let mut cuts = breaks(lines, base);
    cuts.push(lines.len());
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let unit = &lines[a..b];
        let whole = Span {
            line: first + a,
            lines: b - a,
            inside: None,
        };
        if size(unit) <= CHUNK_CHARS || unit.len() < 2 {
            out.push(whole);
            continue;
        }
        match split_below_first_line(unit, first + a, depth + 1) {
            Some(inner) => out.extend(inner),
            None => out.push(whole),
        }
    }
}

fn split_below_first_line(unit: &[&str], line: usize, depth: usize) -> Option<Vec<Span>> {
    let mut inner = Vec::new();
    units(
        &unit[1..],
        line + 1,
        Some(indent(unit[0])),
        depth,
        &mut inner,
    );
    if inner.len() < 2 {
        return None;
    }
    inner[0].line -= 1;
    inner[0].lines += 1;
    let def = definition_line(unit).map(|i| line + i);
    for s in &mut inner[1..] {
        s.inside = s.inside.or(def);
    }
    Some(inner)
}

fn definition_line(unit: &[&str]) -> Option<usize> {
    unit.iter()
        .position(|l| !l.trim().is_empty() && !leads_in(l))
}

struct Pending {
    text: String,
    start: usize,
    lead: usize,
}

impl Pending {
    fn flush(&mut self, out: &mut Vec<(usize, String)>) {
        let (head, body) = self.text.split_at(self.lead);
        let skipped = body.lines().take_while(|l| l.trim().is_empty()).count();
        let body: String = body.lines().skip(skipped).flat_map(|l| [l, "\n"]).collect();
        if !body.trim().is_empty() {
            out.push((self.start + skipped, format!("{head}{body}")));
        }
        self.text.clear();
        self.lead = 0;
    }
}

/// Code as chunks of whole definitions up to CHUNK_CHARS: short neighbours share a chunk, a long
/// one is split at its inner definitions or paragraphs, and only a run with no break is cut by
/// length, as `index::chunk` does. A chunk from inside a definition starts with that definition's
/// first line, so the part of a long function says which function it's from.
pub fn chunk(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut spans = Vec::new();
    units(&lines, 1, None, 0, &mut spans);
    let joined = |from: usize, n: usize| -> String {
        lines[from - 1..from - 1 + n]
            .iter()
            .flat_map(|l| [*l, "\n"])
            .collect()
    };
    let mut out = Vec::new();
    let mut cur = Pending {
        text: String::new(),
        start: 1,
        lead: 0,
    };
    for s in spans {
        let unit = joined(s.line, s.lines);
        if unit.len() > CHUNK_CHARS {
            cur.flush(&mut out);
            out.extend(
                crate::index::chunk(&unit)
                    .into_iter()
                    .map(|(l, c)| (s.line + l - 1, c)),
            );
            continue;
        }
        if !cur.text.is_empty() && cur.text.len() + unit.len() > CHUNK_CHARS {
            cur.flush(&mut out);
        }
        if cur.text.is_empty() {
            cur.start = s.line;
            if let Some(def) = s.inside {
                cur.text = joined(def, 1);
                cur.lead = cur.text.len();
            }
        }
        cur.text.push_str(&unit);
    }
    cur.flush(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn code_is_told_from_text() {
        assert!(is_code(Path::new("/a/b.RS")));
        assert!(is_code(Path::new("/a/b.tsx")));
        for text in ["md", "txt", "rst", "org", "tex", "html"] {
            assert!(!is_code(Path::new(&format!("/a/b.{text}"))), "{text}");
        }
        for e in EXTENSIONS {
            assert!(!crate::index::TEXT_EXTENSIONS.contains(e), "{e} is both");
        }
    }

    #[test]
    fn config_and_data_formats_are_not_code() {
        for data in ["toml", "yaml", "json", "env", "pem", "lock", "csv"] {
            assert!(
                !is_code(Path::new(&format!("/a/b.{data}"))),
                "{data} holds credentials"
            );
        }
    }

    #[test]
    fn a_definition_after_modifiers_with_arguments_starts_one() {
        assert!(starts_definition("pub(crate) fn a() {"));
        assert!(starts_definition("extern \"C\" fn b() {"));
    }

    #[test]
    fn a_statement_is_not_a_definition() {
        assert!(!starts_definition("let f = make(fn_name);"));
        assert!(!starts_definition("pub(crate) use a::b;"));
    }

    #[test]
    fn a_token_on_its_own_is_a_token() {
        assert!(token("key: AKIAIOSFODNN7EXAMPLE\n"));
    }

    #[test]
    fn a_token_inside_a_longer_word_is_not_a_token() {
        assert!(!token("key: XAKIAIOSFODNN7EXAMPLE\n"));
    }

    #[test]
    fn a_code_prefix_makes_a_code_query() {
        assert_eq!(code_query("code: parse args"), Some("parse args"));
        assert_eq!(code_query("  CODE:parse"), Some("parse"));
        assert_eq!(code_query("code review notes"), None);
        assert_eq!(code_query("cod"), None);
        assert_eq!(code_query("é"), None);
    }

    #[test]
    fn generated_vendored_and_secret_names_are_left_out() {
        for p in [
            "/r/api.pb.go",
            "/r/msg_pb2.py",
            "/r/types.d.ts",
            "/r/app.min.js",
            "/r/schema.generated.ts",
            "/r/aws_credentials.py",
            "/r/secrets.rs",
        ] {
            assert!(left_out_by_name(Path::new(p)), "{p}");
        }
        assert!(!left_out_by_name(Path::new("/r/main.rs")));
    }

    #[test]
    fn content_that_is_generated_minified_or_secret_is_unwanted() {
        assert_eq!(
            unwanted("// Code generated by protoc-gen-go. DO NOT EDIT.\npackage x\n"),
            Some("generated")
        );
        assert_eq!(unwanted(&"var a=1;".repeat(1000)), Some("minified"));
        let key = "KEY = \"\"\"-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn\n\"\"\"\n";
        assert_eq!(unwanted(key), Some("holds a private key"));
        let aws = "aws_access_key_id = \"AKIAIOSFODNN7EXAMPLE\"\n";
        assert_eq!(unwanted(aws), Some("holds an API token"));
        let gh = format!("TOKEN = 'ghp_{}'\n", "a1B2".repeat(9));
        assert_eq!(unwanted(&gh), Some("holds an API token"));
    }

    #[test]
    fn talking_about_keys_is_not_holding_one() {
        for ok in [
            "fn main() {\n    println!(\"hi\");\n}\n",
            "// -----BEGIN PUBLIC KEY----- is what we expect\n",
            "if line.starts_with(\"ghp_\") { warn() }\n",
            "let task = \"ask-for-help\";\n",
        ] {
            assert_eq!(unwanted(ok), None, "{ok}");
        }
    }

    fn spans(text: &str) -> Vec<(usize, usize)> {
        chunk(text)
            .iter()
            .map(|(l, c)| (*l, c.lines().count()))
            .collect()
    }

    fn function_of_400_bytes(name: &str) -> String {
        format!(
            "/// Does {name}.\nfn {name}() {{\n{}}}\n",
            "    let x = compute_something_long(1, 2, 3);\n".repeat(8)
        )
    }

    #[test]
    fn functions_stay_whole() {
        let f = function_of_400_bytes;
        let text = format!("use std::fs;\n\n{}\n{}\n{}", f("a"), f("b"), f("c"));
        let chunks = chunk(&text);
        assert_eq!(chunks.len(), 2, "{chunks:#?}");
        assert_eq!(chunks[0].0, 1);
        assert!(chunks[0].1.contains("fn a()") && chunks[0].1.contains("fn b()"));
        assert_eq!(
            chunks[1].0, 27,
            "c's doc is line 27: 1 use, 2 blank, 3 a's doc, 15 b's doc"
        );
        assert!(chunks[1].1.starts_with("/// Does c."));
        assert!(chunks.iter().all(|(_, c)| c.len() <= CHUNK_CHARS));
    }

    #[test]
    fn definitions_without_blank_lines_between_still_split() {
        let body = "    x = compute_something_long(1, 2, 3)\n".repeat(12);
        let text = format!("@cache\ndef a():\n{body}@cache\ndef b():\n{body}");
        assert_eq!(spans(&text), [(1, 14), (15, 14)]);
    }

    fn method_of_600_bytes(name: &str) -> String {
        format!(
            "    def {name}(self):\n{}\n",
            "        self.value = compute_something_long(1, 2, 3)\n".repeat(11)
        )
    }

    #[test]
    fn a_long_class_splits_at_its_methods() {
        let method = method_of_600_bytes;
        let text = format!(
            "class Thing:\n{}{}{}",
            method("a"),
            method("b"),
            method("c")
        );
        let chunks = chunk(&text);
        assert_eq!(chunks.len(), 3, "{chunks:#?}");
        assert!(chunks[0].1.starts_with("class Thing:\n    def a"));
        assert_eq!(chunks[1].0, 15);
        assert!(chunks[1].1.starts_with("class Thing:\n    def b"));
        assert!(chunks[2].1.starts_with("class Thing:\n    def c"));
    }

    #[test]
    fn the_part_of_a_long_function_names_the_function() {
        let paragraph = "    let x = compute_something_long(1, 2, 3);\n".repeat(12);
        let text = format!("/// Docs.\nfn long_one() {{\n{paragraph}\n{paragraph}}}\n");
        let chunks = chunk(&text);
        assert_eq!(chunks.len(), 2, "{chunks:#?}");
        assert_eq!(chunks[0].0, 1);
        assert!(
            chunks[0]
                .1
                .starts_with("/// Docs.\nfn long_one() {\n    let x")
        );
        assert_eq!(chunks[1].0, 16, "from after the blank line");
        assert!(
            chunks[1].1.starts_with("fn long_one() {\n    let x"),
            "the function's line first"
        );
        assert!(chunks[1].1.ends_with("}\n"));
    }

    #[test]
    fn a_run_with_no_breaks_is_cut_by_length() {
        let text = "x = 1\n".repeat(400);
        let chunks = chunk(&text);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|(_, c)| c.len() <= CHUNK_CHARS + 1));
        assert_eq!(
            chunks.iter().map(|(_, c)| c.lines().count()).sum::<usize>(),
            400
        );
    }

    #[test]
    fn a_20000_line_flat_indented_file_does_not_overflow_stack() {
        let text = "x = 1\n".repeat(20_000);
        let chunks = chunk(&text);
        assert!(chunks.len() > 10);
        assert_eq!(
            chunks.iter().map(|(_, c)| c.lines().count()).sum::<usize>(),
            20_000
        );
    }

    #[test]
    fn blank_code_has_no_chunks() {
        assert!(chunk("\n\n   \n").is_empty());
    }

    struct Repo(PathBuf);

    impl Repo {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("vinosearch-code-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join(".git/info")).unwrap();
            Repo(dir)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
            p
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn git_ignored_and_vendored_code_is_left_out() {
        let r = Repo::new("ignore");
        r.write(".gitignore", "gen/\n*.out.py\n");
        r.write("sub/.gitignore", "local.rs\n!keep.out.py\n");
        r.write(".ignore", "scratch.rs\n");
        r.write(".git/info/exclude", "mine.rs\n");
        let ig = Ignores::default();
        let out = |p: &str| ig.left_out(&r.0.join(p));
        assert!(!out("src/main.rs"));
        assert!(out("gen/a.rs"));
        assert!(out("src/gen/deep/a.rs"));
        assert!(out("x.out.py"));
        assert!(out("sub/local.rs"));
        assert!(!out("local.rs"));
        assert!(out("scratch.rs"));
        assert!(out("mine.rs"));
        assert!(out("third_party/zlib/inflate.c"));
    }

    #[test]
    fn a_nearer_ignore_files_rule_wins() {
        let r = Repo::new("nearer");
        r.write(".gitignore", "*.out.py\n");
        r.write("sub/.gitignore", "!keep.out.py\n");
        let ig = Ignores::default();
        assert!(!ig.left_out(&r.0.join("sub/keep.out.py")));
        assert!(ig.left_out(&r.0.join("keep.out.py")));
    }

    #[test]
    fn outside_a_work_tree_nothing_is_git_ignored() {
        let ig = Ignores::default();
        assert!(!ig.left_out(Path::new("/nonexistent-vinosearch/third_party/a.rs")));
    }
}
