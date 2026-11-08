//! Source code, indexed apart from text and only when it's turned on (#110):
//! which files are code, which of those are left out, and chunks that
//! follow the code's own definitions.
//!
//! Left out by path: files git ignores (`.gitignore`, `.ignore`,
//! `.git/info/exclude` and the global excludes file, inside a git work
//! tree), vendored directories, generated files by their names, and files
//! named for secrets. Left out by content, once read: generated files by
//! their header, minified ones, and anything holding a private key or an
//! API token.

use crate::index::CHUNK_CHARS;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Code, by extension. Prose and markup (`md`, `txt`, `rst`, `org`, `tex`,
/// `html`) are text, in `index::TEXT_EXTENSIONS`.
pub const EXTENSIONS: &[&str] = &[
    "rs", "py", "pyi", "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "vue", "svelte", "go",
    "c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx", "m", "mm", "java", "kt", "kts", "scala",
    "swift", "dart", "rb", "php", "pl", "pm", "lua", "zig", "nix", "cs", "fs", "ex", "exs", "erl",
    "hs", "ml", "mli", "clj", "r", "jl", "sh", "bash", "zsh", "fish", "ps1", "css", "scss", "sass",
    "less", "sql", "qml", "cmake", "gd", "glsl", "wgsl", "proto",
];
/// Directories of other people's code checked into a repository, inside a
/// git work tree. `vendor` and `node_modules` are skipped for every kind
/// (`index::SKIP_DIRS`).
const VENDORED_DIRS: &[&str] = &[
    "third_party",
    "third-party",
    "thirdparty",
    "3rdparty",
    "vendored",
];
/// Generated files, by how their names end.
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
/// Parts of a file name that say what's inside is a secret.
const SECRET_NAMES: &[&str] = &["secret", "credential", "password", "passwd"];
/// How long the ignore files read stay trusted before they're read again.
const IGNORES_TTL: Duration = Duration::from_secs(60);

/// Whether `path` is code, by its extension.
pub fn is_code(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// A query for code only: what follows a leading `code:`, or None.
pub fn code_query(query: &str) -> Option<&str> {
    let q = query.trim_start();
    let head = q.get(..5)?;
    head.eq_ignore_ascii_case("code:").then(|| q[5..].trim())
}

/// The `.gitignore` and `.ignore` files of the directories looked at, read
/// once a minute at most. Shared by the clones of a `FileTypes`.
#[derive(Default)]
pub struct Ignores {
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    read: Option<Instant>,
    /// The git work tree each directory is in, if any.
    repos: HashMap<PathBuf, Option<PathBuf>>,
    /// Each directory's own ignore files, `.ignore` first (it wins).
    dirs: HashMap<PathBuf, Arc<Vec<Gitignore>>>,
    /// Each work tree's `.git/info/exclude` and the global excludes file.
    repo_wide: HashMap<PathBuf, Arc<Vec<Gitignore>>>,
}

impl std::fmt::Debug for Ignores {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Ignores")
    }
}

impl Ignores {
    /// Whether code at `path` is left out by its path: git ignores it, it is
    /// in a vendored directory of its work tree, or its name says it is
    /// generated or a secret.
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
        // Deepest first: a nearer file's rules win over a further one's.
        let mut levels: Vec<&Path> = dir
            .ancestors()
            .take_while(|d| d.starts_with(&repo))
            .collect();
        levels.dedup();
        for d in levels {
            let rel = path.strip_prefix(d).expect("an ancestor");
            for gi in cache.dir(d).iter() {
                match gi.matched_path_or_any_parents(rel, false) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
        }
        for gi in cache.repo_wide(&repo).iter() {
            match gi.matched_path_or_any_parents(inside, false) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }
}

impl Cache {
    /// The work tree `dir` is in: the nearest directory holding `.git` (a
    /// directory, or a file for a worktree or submodule).
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

/// Generated or secret by its name alone.
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

/// Why code with this content is left out, or None to index it.
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

/// A "do not edit" header near the top: Go's `// Code generated ... DO NOT
/// EDIT.`, `@generated`, protoc's and most other generators'.
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

/// A few enormous lines: a bundle or minified file without `.min` in its name.
fn minified(text: &str) -> bool {
    text.len() > 4096 && text.lines().count() * 400 < text.len()
}

fn private_key(text: &str) -> bool {
    text.match_indices("-----BEGIN ").any(|(i, _)| {
        let line = text[i..].lines().next().unwrap_or_default();
        line.contains("PRIVATE KEY")
    })
}

/// Token formats that can't be mistaken for anything else: AWS access key
/// ids, GitHub, GitLab, Slack, Stripe, OpenAI and Google API keys.
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
    let body = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    PREFIXES.iter().any(|(prefix, len)| {
        text.match_indices(prefix).any(|(i, _)| {
            // Not the middle of a longer word.
            let before = text[..i].chars().next_back();
            before.is_none_or(|c| !body(c))
                && text[i + prefix.len()..]
                    .chars()
                    .take_while(|&c| body(c))
                    .count()
                    >= *len
        })
    })
}

/// Leading whitespace, a tab as four.
fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// A line that starts a definition in most languages: `fn`, `def`,
/// `class`, `func`, `function` and the like, after any modifiers.
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
        // `pub(crate)`, `extern "C"`
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

/// A line that belongs to the definition after it: a comment, a doc
/// comment, an attribute or a decorator.
fn leads_in(line: &str) -> bool {
    let t = line.trim_start();
    ["//", "#", "/*", "*", "@", "--", ";", "\"\"\"", "'''"]
        .iter()
        .any(|p| t.starts_with(p))
}

/// Where `lines` splits into units at indentation `base`: a line there
/// after a blank one, or one that starts a definition (with the comments
/// and attributes right above it).
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
            let mut start = i;
            while start > 0
                && !lines[start - 1].trim().is_empty()
                && indent(lines[start - 1]) == base
                && leads_in(lines[start - 1])
            {
                start -= 1;
            }
            // Joined to a lead-in already split off after a blank line.
            if out.last() != Some(&start) && start > *out.last().unwrap() {
                out.push(start);
            }
        }
    }
    out
}

/// A run of lines that belongs together, and the definition line it's
/// inside of when that isn't in it (both counted from 1).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    line: usize,
    lines: usize,
    inside: Option<usize>,
}

/// `lines` (from line `first`, counted from 1) as units no longer than
/// CHUNK_CHARS where the code allows: whole definitions, or a definition
/// that's too long split at its own inner definitions or paragraphs, its
/// first line kept with the first of them. Inside a definition indented
/// `header`, the units are at the least indentation deeper than it, so
/// a closing brace back at `header` stays with the last of them.
fn units(lines: &[&str], first: usize, header: Option<usize>, out: &mut Vec<Span>) {
    let size = |r: &[&str]| r.iter().map(|l| l.len() + 1).sum::<usize>();
    let indents = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent(l));
    let base = indents
        .clone()
        .filter(|&i| header.is_none_or(|h| i > h))
        .min()
        .or_else(|| indents.min())
        .unwrap_or(0);
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
        // Too long: split inside, below its first line.
        let mut inner = Vec::new();
        units(&unit[1..], first + a + 1, Some(indent(unit[0])), &mut inner);
        if inner.len() < 2 {
            out.push(whole);
            continue;
        }
        inner[0].line -= 1;
        inner[0].lines += 1;
        // The definition itself, past the comments and attributes above it.
        let def = unit
            .iter()
            .position(|l| !l.trim().is_empty() && !leads_in(l))
            .map(|i| first + a + i);
        for s in &mut inner[1..] {
            s.inside = s.inside.or(def);
        }
        out.extend(inner);
    }
}

/// Code as chunks of whole definitions, up to CHUNK_CHARS each, with the
/// line each starts on: neighbouring short ones share a chunk, a long one
/// is split at its inner definitions (methods of a class or impl) or its
/// paragraphs, and only a run of lines with no break in it is cut by
/// length, as `index::chunk` cuts text. A chunk from inside a definition
/// whose first line it doesn't have starts with that line, so the part of
/// a long function says which function it's from.
pub fn chunk(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut spans = Vec::new();
    units(&lines, 1, None, &mut spans);
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut cur = String::new();
    // The chunk's first line, and the bytes of the definition line it
    // starts with, if it does.
    let mut start = 1;
    let mut lead = 0;
    let flush = |cur: &mut String, start: usize, lead: usize, out: &mut Vec<(usize, String)>| {
        // From its first line with anything on it.
        let (head, body) = cur.split_at(lead);
        let skipped = body.lines().take_while(|l| l.trim().is_empty()).count();
        let body: String = body.lines().skip(skipped).flat_map(|l| [l, "\n"]).collect();
        if !body.trim().is_empty() {
            out.push((start + skipped, format!("{head}{body}")));
        }
        cur.clear();
    };
    let joined = |from: usize, n: usize| -> String {
        lines[from - 1..from - 1 + n]
            .iter()
            .flat_map(|l| [*l, "\n"])
            .collect()
    };
    for s in spans {
        let unit = joined(s.line, s.lines);
        if unit.len() > CHUNK_CHARS {
            flush(&mut cur, start, lead, &mut out);
            lead = 0;
            out.extend(
                crate::index::chunk(&unit)
                    .into_iter()
                    .map(|(l, c)| (s.line + l - 1, c)),
            );
            continue;
        }
        if !cur.is_empty() && cur.len() + unit.len() > CHUNK_CHARS {
            flush(&mut cur, start, lead, &mut out);
            lead = 0;
        }
        if cur.is_empty() {
            start = s.line;
            if let Some(def) = s.inside {
                cur = joined(def, 1);
                lead = cur.len();
            }
        }
        cur.push_str(&unit);
    }
    flush(&mut cur, start, lead, &mut out);
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
        // Config and data formats are no kind at all: they hold credentials.
        for data in ["toml", "yaml", "json", "env", "pem", "lock", "csv"] {
            assert!(!is_code(Path::new(&format!("/a/b.{data}"))), "{data}");
        }
        for e in EXTENSIONS {
            assert!(!crate::index::TEXT_EXTENSIONS.contains(e), "{e} is both");
        }
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
        // Talking about keys is fine.
        for ok in [
            "fn main() {\n    println!(\"hi\");\n}\n",
            "// -----BEGIN PUBLIC KEY----- is what we expect\n",
            "if line.starts_with(\"ghp_\") { warn() }\n",
            "let task = \"ask-for-help\";\n",
        ] {
            assert_eq!(unwanted(ok), None, "{ok}");
        }
    }

    /// The chunks' lines, each chunk's first line number and line count.
    fn spans(text: &str) -> Vec<(usize, usize)> {
        chunk(text)
            .iter()
            .map(|(l, c)| (*l, c.lines().count()))
            .collect()
    }

    #[test]
    fn functions_stay_whole() {
        // Each about 400 bytes: two fit in a chunk, three don't.
        let f = |name: &str| {
            format!(
                "/// Does {name}.\nfn {name}() {{\n{}}}\n",
                "    let x = compute_something_long(1, 2, 3);\n".repeat(8)
            )
        };
        let text = format!("use std::fs;\n\n{}\n{}\n{}", f("a"), f("b"), f("c"));
        let chunks = chunk(&text);
        assert_eq!(chunks.len(), 2, "{chunks:#?}");
        // Lines: 1 use, 2 blank, 3 doc of a, ..., 15 doc of b, 27 doc of c.
        assert_eq!(chunks[0].0, 1);
        assert!(chunks[0].1.contains("fn a()") && chunks[0].1.contains("fn b()"));
        assert_eq!(chunks[1].0, 27);
        assert!(chunks[1].1.starts_with("/// Does c."));
        assert!(chunks.iter().all(|(_, c)| c.len() <= CHUNK_CHARS));
    }

    #[test]
    fn definitions_without_blank_lines_between_still_split() {
        let body = "    x = compute_something_long(1, 2, 3)\n".repeat(12);
        let text = format!("@cache\ndef a():\n{body}@cache\ndef b():\n{body}");
        assert_eq!(spans(&text), [(1, 14), (15, 14)]);
    }

    #[test]
    fn a_long_class_splits_at_its_methods() {
        // Each about 600 bytes: one to a chunk.
        let method = |name: &str| {
            format!(
                "    def {name}(self):\n{}\n",
                "        self.value = compute_something_long(1, 2, 3)\n".repeat(11)
            )
        };
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
        // From line 16, after the blank line, with the function's line first.
        assert_eq!(chunks[1].0, 16);
        assert!(chunks[1].1.starts_with("fn long_one() {\n    let x"));
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
    fn blank_code_has_no_chunks() {
        assert!(chunk("\n\n   \n").is_empty());
    }

    /// A git work tree under the temp dir, removed when dropped.
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
        // A nearer file's rule wins.
        assert!(!out("sub/keep.out.py"));
        assert!(out("scratch.rs"));
        assert!(out("mine.rs"));
        assert!(out("third_party/zlib/inflate.c"));
        // Outside a work tree, nothing is git-ignored.
        assert!(!ig.left_out(Path::new("/nonexistent-vinosearch/third_party/a.rs")));
    }
}
