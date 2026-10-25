//! `vinosearch eval` (#107): index a testbed into a throwaway index, run
//! every query in a queries file through the search KRunner uses, and score
//! where the expected files land.

use crate::index::{self, Hit, Kind};
use crate::krunner::{self, Floors};
use crate::{extract, worker};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use vinosearch_shared::config;

/// The queries file: thresholds, then a `[[query]]` table per query.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Queries {
    pub thresholds: Thresholds,
    #[serde(rename = "query")]
    pub queries: Vec<Query>,
}

/// The lowest overall metrics that pass. Recall is a share of queries, 0 to 1.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub mrr: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub query: String,
    /// Paths under the testbed; finding any one of them counts.
    pub expect: Vec<String>,
    /// What the query tests, for the per-kind numbers. `image` queries need
    /// the vision encoder and run only with `--images`.
    #[serde(default = "general")]
    pub kind: String,
    /// The line (text files, CSV rows) or page, slide or chapter (documents)
    /// the expected file's best chunk should cover.
    pub at: Option<i64>,
}

fn general() -> String {
    "general".into()
}

pub const IMAGE_KIND: &str = "image";

impl Queries {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| path.display().to_string())?;
        let q: Self = toml::from_str(&text).with_context(|| path.display().to_string())?;
        if q.queries.is_empty() {
            bail!("{}: no queries", path.display());
        }
        if let Some(bad) = q.queries.iter().find(|q| q.expect.is_empty()) {
            bail!("{:?} expects no file", bad.query);
        }
        Ok(q)
    }
}

/// Where a query's first expected file landed.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub query: String,
    pub kind: String,
    /// 1-based, among the results KRunner would show; None if not shown.
    pub rank: Option<usize>,
    /// Whether the expected file's chunk covers `at`; None without `at` or a hit.
    pub located: Option<bool>,
    /// The top result, relative to the testbed, and its score.
    pub top: Option<(String, f32)>,
    /// The expected file's score, and the line or page of its best chunk.
    pub found: Option<(f32, i64)>,
    /// How far the expected file scores above the best other file shown
    /// (negative when it isn't first): how close the query is to flipping.
    pub margin: Option<f32>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Metrics {
    pub queries: usize,
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub mrr: f64,
}

impl Metrics {
    pub fn of<'a>(outcomes: impl IntoIterator<Item = &'a Outcome>) -> Self {
        let mut m = Metrics::default();
        for o in outcomes {
            m.queries += 1;
            if let Some(r) = o.rank {
                m.recall_at_1 += f64::from(r == 1);
                m.recall_at_5 += f64::from(r <= 5);
                m.mrr += 1.0 / r as f64;
            }
        }
        if m.queries > 0 {
            let n = m.queries as f64;
            m.recall_at_1 /= n;
            m.recall_at_5 /= n;
            m.mrr /= n;
        }
        m
    }

    /// The metrics below their threshold, by name.
    pub fn below(&self, t: &Thresholds) -> Vec<String> {
        [
            ("recall@1", self.recall_at_1, t.recall_at_1),
            ("recall@5", self.recall_at_5, t.recall_at_5),
            ("MRR", self.mrr, t.mrr),
        ]
        .into_iter()
        // A hair of slack, so a threshold written as the measured value passes.
        .filter(|(_, got, min)| *got + 1e-9 < *min)
        .map(|(name, got, min)| format!("{name} {got:.3} is below {min:.3}"))
        .collect()
    }
}

/// The results KRunner would show for `hits`, best first: a file once,
/// above its kind's floor, ordered by the relevance KRunner sorts on.
pub fn shown(hits: Vec<Hit>, floors: Floors) -> Vec<Hit> {
    let mut shown = krunner::per_file(hits, floors);
    shown.sort_by(|a, b| krunner::relevance(b, floors).total_cmp(&krunner::relevance(a, floors)));
    shown
}

/// Whether `hit`, a chunk of `path`, covers `at`: the same page, slide or
/// chapter for documents numbered so, else a line within the chunk.
fn covers(path: &Path, hit: &Hit, at: i64) -> bool {
    let paged = hit.kind == Kind::Document
        && extract::is_document(path)
        && matches!(extract::unit(path), "page" | "slide" | "chapter");
    if paged {
        return hit.line == at;
    }
    let lines = hit.text.lines().count().max(1) as i64;
    (hit.line..hit.line + lines).contains(&at)
}

/// Score `shown` (relative paths, best first) against `q`.
pub fn outcome(q: &Query, root: &Path, shown: &[Hit]) -> Outcome {
    let rel = |h: &Hit| {
        Path::new(&h.path)
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| h.path.clone())
    };
    let found = shown
        .iter()
        .position(|h| q.expect.iter().any(|e| *e == rel(h)));
    Outcome {
        query: q.query.clone(),
        kind: q.kind.clone(),
        rank: found.map(|i| i + 1),
        located: match (found, q.at) {
            (Some(i), Some(at)) => Some(covers(Path::new(&shown[i].path), &shown[i], at)),
            _ => None,
        },
        top: shown.first().map(|h| (rel(h), h.score)),
        found: found.map(|i| (shown[i].score, shown[i].line)),
        margin: found.and_then(|i| {
            let other = shown
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, h)| h.score)
                .fold(None, |m: Option<f32>, s| Some(m.map_or(s, |m| m.max(s))))?;
            Some(shown[i].score - other)
        }),
    }
}

pub struct Options {
    pub testbed: PathBuf,
    pub images: bool,
    pub json: bool,
    /// The index encrypted, with a key held in memory (#4).
    pub encrypted: bool,
    pub thresholds: Thresholds,
}

/// A directory removed when dropped, for the throwaway index.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Index `opts.testbed` into a fresh index under the temp directory, run
/// every query, print the numbers. False if a metric is below its threshold.
pub fn run(
    opts: &Options,
    queries: &Queries,
    device: &str,
    mut emb: crate::embed::Embedder,
) -> Result<bool> {
    let root = fs::canonicalize(&opts.testbed)
        .with_context(|| format!("testbed {}", opts.testbed.display()))?;
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "vinosearch-eval-{}-{}",
        std::process::id(),
        worker::unix_now()
    )));
    // create_dir, not create_dir_all: never reuse an index that's there.
    fs::create_dir(&scratch.0).with_context(|| scratch.0.display().to_string())?;
    let db = scratch.0.join("index.sqlite");
    if opts.encrypted {
        crate::encryption::hold_in_memory(&db)?;
    }
    let mut conn = index::open(&db)?;

    let mut formats = config::Formats::default();
    formats.documents.enabled = Some(true);
    formats.code.enabled = Some(true);
    formats.images.enabled = Some(opts.images);
    let types = index::FileTypes::new(&formats)?;
    let floors = Floors::of_formats(&formats);
    let excludes = config::Config::default().excludes()?;

    let t0 = Instant::now();
    let st = index::index(
        &mut conn,
        &mut emb,
        &root,
        &excludes,
        &types,
        &mut |_, _, _| Ok(()),
    )?;
    let mut chunks = st.chunks;
    while let Some(rest) = index::index_rest(&mut conn, &mut emb, &types, &mut |_, _| Ok(()))? {
        if let index::Rest::Parts { chunks: n, .. } = rest {
            chunks += n;
        }
    }
    let index_secs = t0.elapsed().as_secs_f64();
    eprintln!(
        "indexed {} files, {chunks} chunks in {index_secs:.0} s on {device}",
        st.files_embedded
    );

    let vectors = index::Vectors::load(&conn)?;
    let t0 = Instant::now();
    let mut outcomes = Vec::new();
    let mut skipped = 0;
    for q in &queries.queries {
        if q.kind == IMAGE_KIND && !opts.images {
            skipped += 1;
            continue;
        }
        let hits = worker::matches(&conn, &mut emb, &vectors, &q.query, krunner::RESULTS * 8)?;
        outcomes.push(outcome(q, &root, &shown(hits, floors)));
    }
    let query_ms = t0.elapsed().as_secs_f64() * 1e3 / outcomes.len().max(1) as f64;

    let overall = Metrics::of(&outcomes);
    let mut kinds: BTreeMap<&str, Vec<&Outcome>> = BTreeMap::new();
    for o in &outcomes {
        kinds.entry(&o.kind).or_default().push(o);
    }
    let per_kind: BTreeMap<&str, Metrics> = kinds
        .into_iter()
        .map(|(k, os)| (k, Metrics::of(os)))
        .collect();
    let failures = overall.below(&opts.thresholds);

    if opts.json {
        let report = serde_json::json!({
            "device": device,
            "encrypted": opts.encrypted,
            "files": st.files_embedded,
            "chunks": chunks,
            "index_seconds": index_secs,
            "query_ms": query_ms,
            "skipped_image_queries": skipped,
            "overall": overall,
            "kinds": per_kind,
            "thresholds": opts.thresholds,
            "passed": failures.is_empty(),
            "queries": outcomes,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for o in &outcomes {
            let rank = o.rank.map_or("-".into(), |r| r.to_string());
            let located = match o.located {
                Some(false) => "  (wrong place)",
                _ => "",
            };
            println!("{rank:>2}  [{}] {}{located}", o.kind, o.query);
            if o.rank != Some(1) {
                match &o.top {
                    Some((path, score)) => println!("      top: {path} ({score:.3})"),
                    None => println!("      top: nothing above the floor"),
                }
            }
        }
        println!();
        println!(
            "{:<16} {:>7} {:>9} {:>9} {:>6}",
            "", "queries", "recall@1", "recall@5", "MRR"
        );
        let row = |name: &str, m: &Metrics| {
            println!(
                "{name:<16} {:>7} {:>9.3} {:>9.3} {:>6.3}",
                m.queries, m.recall_at_1, m.recall_at_5, m.mrr
            );
        };
        for (k, m) in &per_kind {
            row(k, m);
        }
        row("overall", &overall);
        let located: Vec<bool> = outcomes.iter().filter_map(|o| o.located).collect();
        if !located.is_empty() {
            println!(
                "right line or page: {} of {}",
                located.iter().filter(|l| **l).count(),
                located.len()
            );
        }
        let enc = if opts.encrypted {
            ", index encrypted"
        } else {
            ""
        };
        println!("{query_ms:.0} ms a query on {device}{enc}");
        if skipped > 0 {
            println!("{skipped} image queries skipped; --images runs them");
        }
    }
    for f in &failures {
        eprintln!("eval: {f}");
    }
    Ok(failures.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: Kind, score: f32, path: &str, line: i64, text: &str) -> Hit {
        Hit {
            kind,
            score,
            path: path.into(),
            line,
            text: text.into(),
            copies: Vec::new(),
        }
    }

    fn query(expect: &[&str], at: Option<i64>) -> Query {
        Query {
            query: "q".into(),
            expect: expect.iter().map(|e| (*e).into()).collect(),
            kind: general(),
            at,
        }
    }

    #[test]
    fn ranks_by_relevance_across_kinds_one_file_once() {
        let hits = vec![
            hit(Kind::Text, 0.70, "/t/a.md", 1, "a"),
            hit(Kind::Text, 0.68, "/t/a.md", 9, "a again"),
            hit(Kind::Text, 0.60, "/t/below.md", 1, "under the floor"),
            hit(Kind::Document, 0.80, "/t/b.pdf", 2, "b"),
        ];
        let shown = shown(hits, Floors::of_formats(&config::Formats::default()));
        let paths: Vec<&str> = shown.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["/t/b.pdf", "/t/a.md"]);

        let o = outcome(&query(&["a.md"], None), Path::new("/t"), &shown);
        assert_eq!(o.rank, Some(2));
        assert_eq!(o.top, Some(("b.pdf".into(), 0.80)));
        assert_eq!(o.found, Some((0.70, 1)));
        assert!((o.margin.unwrap() + 0.10).abs() < 1e-6);
        let o = outcome(&query(&["below.md"], None), Path::new("/t"), &shown);
        assert_eq!(o.rank, None);
    }

    #[test]
    fn location_is_a_page_for_pdfs_and_a_line_range_for_text() {
        let pdf = hit(Kind::Document, 0.8, "/t/r.pdf", 2, "line\nline\nline");
        assert!(covers(Path::new("/t/r.pdf"), &pdf, 2));
        assert!(!covers(Path::new("/t/r.pdf"), &pdf, 3));
        let md = hit(Kind::Text, 0.8, "/t/n.md", 10, "a\nb\nc");
        assert!(covers(Path::new("/t/n.md"), &md, 12));
        assert!(!covers(Path::new("/t/n.md"), &md, 13));
    }

    #[test]
    fn metrics_and_thresholds() {
        let o = |rank| Outcome {
            query: String::new(),
            kind: general(),
            rank,
            located: None,
            top: None,
            found: None,
            margin: None,
        };
        let m = Metrics::of(&[o(Some(1)), o(Some(2)), o(Some(7)), o(None)]);
        assert_eq!(m.queries, 4);
        assert_eq!(m.recall_at_1, 0.25);
        assert_eq!(m.recall_at_5, 0.5);
        assert!((m.mrr - (1.0 + 0.5 + 1.0 / 7.0) / 4.0).abs() < 1e-12);
        let t = Thresholds {
            recall_at_1: 0.25,
            recall_at_5: 0.6,
            mrr: 0.1,
        };
        assert_eq!(m.below(&t), ["recall@5 0.500 is below 0.600"]);
    }

    #[test]
    fn checked_in_queries_parse() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/queries.toml");
        let q = Queries::load(&path).unwrap();
        assert!(q.queries.iter().any(|q| q.kind == IMAGE_KIND));
        assert!(q.queries.iter().any(|q| q.kind == "cross-language"));
    }
}
