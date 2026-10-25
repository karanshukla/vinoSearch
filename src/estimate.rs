//! How long indexing a folder will take and how much index it makes, before
//! it starts (#112). The folder is walked with the indexer's own rules
//! (`index::walk` and `index::indexable`: exclusions, skipped directories,
//! the formats turned on, each kind's size cap), reading no file contents,
//! and what it finds is priced at rates measured on the NPU.

use crate::config::Excludes;
use crate::index::{self, FileTypes, Kind};
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Text and document chunks embedded per second on the NPU: 113,341 chunks
/// in 66 min indexing `~/Development` (README's Performance table, Core 5
/// 320, 2026-10-18). Other devices differ, the CPU by a lot.
pub const CHUNKS_PER_SEC: f64 = 28.6;
/// Seconds per image on the NPU: 1.3 for both vision stages (README,
/// `vision.rs`), and OCR's 0.1 for a photo to 0.4 for a screenshot (#106).
pub const SECS_PER_IMAGE: f64 = 1.5;
/// Index bytes per chunk: the same run's 470 MB for 113,341 chunks. That is
/// the 768-float vector (3 KB), the chunk's text and SQLite's overhead. An
/// image's chunk keeps no text but is priced the same.
pub const INDEX_BYTES_PER_CHUNK: u64 = 4_150;
/// File bytes per text chunk, per file, rounded up. `index::chunk` packs
/// whole lines up to CHUNK_CHARS (900), so chunks run short of it: 25.5 MB
/// of Rust, Python, Markdown and shell in 2,743 files made 30,733 chunks,
/// 830 bytes each. 800 puts that estimate 8% high.
const TEXT_BYTES_PER_CHUNK: u64 = 800;

/// The share of a document's file that comes out as text. Guesses, not
/// measured, set high so the estimate errs long: a PDF is mostly fonts,
/// images and compressed streams; zipped Office and EPUB files deflate
/// their XML about as much as the markup inflates it, and Word 97,
/// PowerPoint 97 and RTF carry about as much formatting; the plain formats
/// are nearly all text.
fn document_text_share(path: &Path) -> f64 {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "csv" | "tsv" | "srt" | "vtt" => 1.0,
        "pdf" => 0.25,
        _ => 0.5,
    }
}

/// The chunks a file of `bytes` is expected to make.
fn chunks_of(kind: Kind, path: &Path, bytes: u64) -> u64 {
    let text = match kind {
        Kind::Image => return 1,
        Kind::Text => bytes,
        // Text past MAX_FILE_BYTES is skipped, so a document makes at most
        // that much.
        Kind::Document => {
            ((bytes as f64 * document_text_share(path)) as u64).min(index::MAX_FILE_BYTES)
        }
    };
    text.div_ceil(TEXT_BYTES_PER_CHUNK)
}

/// Seconds to embed `chunks` of `kind`.
fn secs_of(kind: Kind, chunks: u64) -> f64 {
    match kind {
        Kind::Image => chunks as f64 * SECS_PER_IMAGE,
        Kind::Text | Kind::Document => chunks as f64 / CHUNKS_PER_SEC,
    }
}

/// Files of one kind still to index.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Tally {
    pub files: u64,
    pub bytes: u64,
    pub chunks: u64,
}

#[derive(Debug, Default)]
pub struct Estimate {
    pub root: PathBuf,
    /// In `Kind::ALL`'s order: text, documents, images.
    pub kinds: [Tally; 3],
    /// Files already indexed and unchanged since, which cost nothing.
    pub up_to_date: u64,
    /// The subfolders taking longest, longest first: (folder, seconds, files).
    pub folders: Vec<(PathBuf, f64, u64)>,
}

impl Estimate {
    pub fn files(&self) -> u64 {
        self.kinds.iter().map(|t| t.files).sum()
    }

    pub fn secs(&self) -> f64 {
        Kind::ALL
            .iter()
            .zip(&self.kinds)
            .map(|(k, t)| secs_of(*k, t.chunks))
            .sum()
    }

    pub fn index_bytes(&self) -> u64 {
        self.kinds.iter().map(|t| t.chunks).sum::<u64>() * INDEX_BYTES_PER_CHUNK
    }

    /// "~4,200 files, ~35 min, ~180 MB".
    pub fn summary(&self) -> String {
        if self.files() == 0 {
            return if self.up_to_date > 0 {
                format!("{}, all indexed already", files(self.up_to_date))
            } else {
                "nothing to index".to_owned()
            };
        }
        let n = self.files();
        let files = if self.up_to_date > 0 {
            let s = if n == 1 { "" } else { "s" };
            format!(
                "{} new or changed file{s} ({} indexed already)",
                about(n),
                count(self.up_to_date)
            )
        } else {
            format!("{} file{}", about(n), if n == 1 { "" } else { "s" })
        };
        format!(
            "{files}, {}, {}",
            duration(self.secs()),
            size(self.index_bytes())
        )
    }

    /// `device` is the one indexing will run on, for `rates_note`.
    pub fn to_json(&self, device: &str) -> serde_json::Value {
        let kinds: serde_json::Map<_, _> = Kind::ALL
            .iter()
            .zip(&self.kinds)
            .map(|(k, t)| {
                let name = match k {
                    Kind::Text => "text",
                    Kind::Document => "documents",
                    Kind::Image => "images",
                };
                (
                    name.to_owned(),
                    serde_json::json!({ "files": t.files, "bytes": t.bytes, "chunks": t.chunks }),
                )
            })
            .collect();
        serde_json::json!({
            "root": self.root,
            "files": self.files(),
            "seconds": self.secs().round() as u64,
            "index_bytes": self.index_bytes(),
            "up_to_date": self.up_to_date,
            "summary": self.summary(),
            "note": rates_note(device),
            "kinds": kinds,
            "folders": self.folders.iter().map(|(path, secs, files)| serde_json::json!({
                "path": path,
                "seconds": secs.round() as u64,
                "files": files,
                "summary": folder_summary(*secs, *files),
            })).collect::<Vec<_>>(),
        })
    }
}

/// Said beside an estimate for a device other than the NPU, whose rates it
/// uses. The CPU's factor is how-it-works.md's "about 5x" on the Device row.
pub fn rates_note(device: &str) -> Option<String> {
    if device.eq_ignore_ascii_case("NPU") {
        None
    } else if device.eq_ignore_ascii_case("CPU") {
        Some("Times are at NPU rates; on the CPU indexing takes about 5x longer.".to_owned())
    } else {
        Some(format!(
            "Times are at NPU rates; {device} rates aren't measured."
        ))
    }
}

/// "~12 min, 1,300 files", for one of the biggest subfolders.
pub fn folder_summary(secs: f64, files: u64) -> String {
    format!("{}, {}", duration(secs), self::files(files))
}

/// "1 file", "1,300 files".
fn files(n: u64) -> String {
    format!("{} file{}", count(n), if n == 1 { "" } else { "s" })
}

/// Walk `root` as indexing it would, counting what is still to index:
/// files `known` (path to recorded mtime and size, from the index) records
/// unchanged are left out. `limit` subfolders are named as the biggest.
/// Copies of a file are counted in full, though indexing embeds one.
pub fn estimate(
    root: &Path,
    ex: &Excludes,
    types: &FileTypes,
    known: &HashMap<String, (i64, i64)>,
    limit: usize,
) -> Result<Estimate> {
    let root = index::resolve_root(root, ex)?;
    let mut est = Estimate {
        root: root.clone(),
        ..Estimate::default()
    };
    // Milliseconds and files under each folder below the root.
    let mut millis: HashMap<PathBuf, u64> = HashMap::new();
    let mut files: HashMap<PathBuf, u64> = HashMap::new();
    let mut total_ms = 0;
    for entry in index::walk(&root, ex).filter(|e| e.file_type().is_file()) {
        let Some((kind, meta)) = index::indexable(&entry, types) else {
            continue;
        };
        let path = entry.path();
        let Some(key) = path.to_str() else { continue };
        if known
            .get(key)
            .is_some_and(|k| Some(*k) == index::stamp_of(&meta))
        {
            est.up_to_date += 1;
            continue;
        }
        let chunks = chunks_of(kind, path, meta.len());
        let i = Kind::ALL.iter().position(|k| *k == kind).expect("listed");
        let t = &mut est.kinds[i];
        t.files += 1;
        t.bytes += meta.len();
        t.chunks += chunks;
        let ms = (secs_of(kind, chunks) * 1000.0).ceil() as u64;
        total_ms += ms;
        for dir in path.ancestors().skip(1).take_while(|d| *d != root) {
            *millis.entry(dir.to_owned()).or_default() += ms;
            *files.entry(dir.to_owned()).or_default() += 1;
        }
    }
    // A folder with under 2% of the time isn't worth excluding.
    millis.retain(|_, ms| *ms * 50 >= total_ms);
    est.folders = index::biggest(&millis, limit)
        .into_iter()
        .map(|(dir, ms)| {
            let n = files.get(&dir).copied().unwrap_or_default();
            (dir, ms as f64 / 1000.0, n)
        })
        .collect();
    Ok(est)
}

/// `n` with thousands separators.
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `n` to two significant figures past 1,000, since it is an estimate:
/// 4,213 reads ~4,200.
fn about(n: u64) -> String {
    if n < 1000 {
        return count(n);
    }
    let step = 10u64.pow(n.ilog10() - 1);
    format!("~{}", count((n + step / 2) / step * step))
}

/// "under a minute", "~35 min", "~1.5 h", "~12 h".
pub fn duration(secs: f64) -> String {
    let min = (secs / 60.0).round();
    if secs < 60.0 {
        "under a minute".to_owned()
    } else if min < 90.0 {
        format!("~{min} min")
    } else if min < 600.0 {
        format!("~{:.1} h", min / 60.0)
    } else {
        format!("~{:.0} h", min / 60.0)
    }
}

/// "~180 MB", "~1.2 GB", "under 1 MB".
fn size(bytes: u64) -> String {
    let mb = bytes as f64 / 1e6;
    if mb < 1.0 {
        "under 1 MB".to_owned()
    } else if mb < 1000.0 {
        format!("~{mb:.0} MB")
    } else {
        format!("~{:.1} GB", mb / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vinosearch-estimate-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn excluding(dir: &Path) -> Excludes {
        crate::config::Config {
            exclude: vec![dir.display().to_string()],
            ..Default::default()
        }
        .excludes()
        .unwrap()
    }

    fn put(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x".repeat(bytes)).unwrap();
    }

    fn types(images: bool, documents: bool) -> FileTypes {
        let mut formats = crate::config::Formats::default();
        formats.images.enabled = Some(images);
        formats.documents.enabled = Some(documents);
        FileTypes::new(&formats).unwrap()
    }

    #[test]
    fn counts_what_indexing_would_and_skips_the_rest() {
        let dir = tree("rules");
        put(&dir.join("notes.md"), 1600); // 2 chunks
        put(&dir.join("a/main.rs"), 801); // 2 chunks
        put(&dir.join("a/empty.txt"), 0); // none
        put(&dir.join("target/gen.rs"), 5000); // build output
        put(&dir.join(".git/x.md"), 5000); // hidden
        put(&dir.join("app.min.js"), 5000); // minified
        put(&dir.join("cfg.toml"), 5000); // config
        put(&dir.join("big.rs"), (index::MAX_FILE_BYTES + 1) as usize); // over the cap for code
        put(&dir.join("photo.jpg"), 5000); // images off
        put(&dir.join("private/secret.md"), 5000); // excluded
        let ex = excluding(&dir.join("private"));
        let est = estimate(&dir, &ex, &types(false, false), &HashMap::new(), 5).unwrap();
        assert_eq!(
            est.kinds[0],
            Tally {
                files: 3,
                bytes: 2401,
                chunks: 4
            }
        );
        assert_eq!(est.files(), 3);
        assert_eq!(est.kinds[2].files, 0);
        assert_eq!(est.index_bytes(), 4 * INDEX_BYTES_PER_CHUNK);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn images_and_documents_count_while_on() {
        let dir = tree("kinds");
        put(&dir.join("photo.jpg"), 5000);
        put(&dir.join("report.pdf"), 32_000); // 8,000 bytes of text: 10 chunks
        put(&dir.join("huge.pdf"), 30 << 20); // text capped at MAX_FILE_BYTES
        let est = estimate(
            &dir,
            &Excludes::default(),
            &types(true, true),
            &HashMap::new(),
            5,
        )
        .unwrap();
        assert_eq!(est.kinds[2].files, 1);
        assert_eq!(est.kinds[2].chunks, 1);
        assert_eq!(est.kinds[1].files, 2);
        assert_eq!(
            est.kinds[1].chunks,
            10 + index::MAX_FILE_BYTES.div_ceil(TEXT_BYTES_PER_CHUNK)
        );
        let text = est.kinds[1].chunks as f64 / CHUNKS_PER_SEC;
        assert!((est.secs() - (text + SECS_PER_IMAGE)).abs() < 1e-9);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn indexed_unchanged_files_cost_nothing() {
        let dir = tree("known");
        put(&dir.join("old.md"), 4000);
        put(&dir.join("new.md"), 4000);
        let old = dir.join("old.md");
        let stamp = index::stamp_of(&fs::metadata(&old).unwrap()).unwrap();
        let known = HashMap::from([(old.display().to_string(), stamp)]);
        let est = estimate(&dir, &Excludes::default(), &types(false, false), &known, 5).unwrap();
        assert_eq!(est.files(), 1);
        assert_eq!(est.up_to_date, 1);
        assert!(
            est.summary().contains("indexed already"),
            "{}",
            est.summary()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_biggest_subfolders_are_named() {
        let dir = tree("folders");
        put(&dir.join("small/a.md"), 800);
        put(&dir.join("data/set/a.txt"), 800_000);
        put(&dir.join("data/set/b.txt"), 800_000);
        put(&dir.join("data/c.txt"), 800);
        put(&dir.join("src/lib.rs"), 80_000);
        let est = estimate(
            &dir,
            &Excludes::default(),
            &types(false, false),
            &HashMap::new(),
            2,
        )
        .unwrap();
        let names: Vec<_> = est.folders.iter().map(|(p, _, _)| p.clone()).collect();
        // data/set holds nearly all of data, so it is named instead.
        assert_eq!(names, [dir.join("data/set"), dir.join("src")]);
        assert_eq!(est.folders[0].2, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_excluded_root_is_refused() {
        let dir = tree("excluded");
        let ex = excluding(&dir);
        assert!(estimate(&dir, &ex, &types(false, false), &HashMap::new(), 5).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn it_reads_as_one_line() {
        assert_eq!(about(999), "999");
        assert_eq!(about(4213), "~4,200");
        assert_eq!(about(4250), "~4,300");
        assert_eq!(about(113_341), "~110,000");
        assert_eq!(folder_summary(30.0, 1), "under a minute, 1 file");
        assert_eq!(duration(30.0), "under a minute");
        assert_eq!(duration(35.0 * 60.0), "~35 min");
        assert_eq!(duration(66.0 * 60.0), "~66 min");
        assert_eq!(duration(150.0 * 60.0), "~2.5 h");
        assert_eq!(duration(13.0 * 3600.0), "~13 h");
        assert_eq!(size(180_000_000), "~180 MB");
        assert_eq!(size(1_234_000_000), "~1.2 GB");
        let est = Estimate {
            kinds: [
                Tally {
                    files: 4213,
                    bytes: 0,
                    chunks: 60_000,
                },
                Tally::default(),
                Tally::default(),
            ],
            ..Estimate::default()
        };
        assert_eq!(est.summary(), "~4,200 files, ~35 min, ~249 MB");
        assert_eq!(Estimate::default().summary(), "nothing to index");
        assert_eq!(rates_note("NPU"), None);
        assert!(rates_note("CPU").unwrap().contains("5x"));
    }

    #[test]
    fn the_readme_run_is_priced_at_its_own_rates() {
        let est = Estimate {
            kinds: [
                Tally {
                    files: 9763,
                    bytes: 0,
                    chunks: 113_341,
                },
                Tally::default(),
                Tally::default(),
            ],
            ..Estimate::default()
        };
        assert_eq!(duration(est.secs()), "~66 min");
        assert_eq!(size(est.index_bytes()), "~470 MB");
    }
}
