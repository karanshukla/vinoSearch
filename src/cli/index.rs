//! `index` and `estimate`.

use crate::index::seal::Sealer;
use crate::index::{self, encryption, estimate};
use crate::ml::embed::Embedder;
use crate::service::worker;
use crate::{Settings, config, embedder};
use anyhow::{Result, bail};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Subfolders `estimate` names as taking longest.
const BIGGEST_FOLDERS: usize = 5;

pub fn index(
    paths: &[PathBuf],
    yes: bool,
    openvino_dir: Option<PathBuf>,
    s: &Settings,
) -> Result<bool> {
    if paths.is_empty() {
        bail!("give at least one path to index");
    }
    if !confirm(paths, s, yes)? {
        eprintln!(
            "Nothing indexed. Add folders later with vinosearch index DIR or the settings window."
        );
        return Ok(true);
    }
    let mut emb = embedder(openvino_dir, &s.device)?;
    let db = index::default_db()?;
    let mut conn = open_as_configured(&db, s)?;
    for root in paths {
        index_root(&mut conn, &mut emb, root, s)?;
    }
    encryption::save_roots(&db, &conn)?;
    index_rest_of_long_documents(&mut conn, &mut emb, s)?;
    Ok(true)
}

/// The index, encrypted or not as the config file asks (#4) before anything
/// is written, and cleared of what another model or an older reader left.
fn open_as_configured(db: &Path, s: &Settings) -> Result<Connection> {
    let enc = encryption::apply(db, encryption::Want::of(&s.file), false, &Sealer::default());
    for msg in [&enc.broken, &enc.error] {
        if !msg.is_empty() {
            eprintln!("encryption: {msg}");
        }
    }
    // Held while indexing, so the runner doesn't convert the file underneath.
    encryption::hold_shared(db)?;
    let conn = index::open(db)?;
    if let Some(why) = worker::model_changed(&index::purge_other_models(&conn)?) {
        eprintln!("{why}");
    }
    index::retry_undecodable(&conn)?;
    index::reread_formats(&conn)?;
    Ok(conn)
}

fn index_root(conn: &mut Connection, emb: &mut Embedder, root: &Path, s: &Settings) -> Result<()> {
    let t0 = Instant::now();
    let mut shown = 0;
    let st = index::index(conn, emb, root, &s.excludes, &s.types, &mut |st, _, _| {
        if st.files_embedded != shown {
            shown = st.files_embedded;
            eprint!(
                "\r{} files, {} chunks, {:.1} chunks/s   ",
                st.files_embedded,
                st.chunks,
                st.chunks as f64 / t0.elapsed().as_secs_f64()
            );
        }
        Ok(())
    })?;
    if st.files_embedded > 0 {
        eprintln!();
    }
    println!(
        "{}: {} files seen, {} embedded ({} chunks), {} duplicates, {} removed",
        root.display(),
        st.files_seen,
        st.files_embedded,
        st.chunks,
        st.duplicates,
        st.removed
    );
    if st.no_decoder > 0 {
        eprintln!(
            "{} photos skipped: HEIC and AVIF need heif-convert installed ({})",
            st.no_decoder,
            config::HEIF_CONVERT_PACKAGES
        );
    }
    Ok(())
}

/// What the first pass left of long documents (#108), which the service
/// leaves until nothing else is waiting.
fn index_rest_of_long_documents(
    conn: &mut Connection,
    emb: &mut Embedder,
    s: &Settings,
) -> Result<()> {
    let t0 = Instant::now();
    let mut chunks = 0;
    while let Some(rest) = index::index_rest(conn, emb, &s.types, &mut |_, _| Ok(()))? {
        if let index::Rest::Parts {
            path,
            done,
            total,
            chunks: n,
        } = rest
        {
            chunks += n;
            eprint!(
                "\r{path}: {done} of {total} parts, {:.1} chunks/s   ",
                chunks as f64 / t0.elapsed().as_secs_f64()
            );
            if done >= total {
                eprintln!();
            }
        }
    }
    Ok(())
}

pub fn estimate(paths: &[PathBuf], json: bool, s: &Settings) -> Result<bool> {
    if paths.is_empty() {
        bail!("give at least one path to estimate");
    }
    for est in estimates(paths, s)? {
        if json {
            println!("{}", est.to_json(&s.device));
            continue;
        }
        println!("{}: {}", est.root.display(), est.summary());
        for (dir, secs, files) in &est.folders {
            println!(
                "  {}  {}",
                dir.display(),
                estimate::folder_summary(*secs, *files)
            );
        }
    }
    if !json {
        print_rates_note(&s.device);
    }
    Ok(true)
}

/// What indexing each of `paths` would cost, leaving out files the index
/// already has unchanged.
fn estimates(paths: &[PathBuf], s: &Settings) -> Result<Vec<estimate::Estimate>> {
    let db = index::default_db()?;
    // Read-only, and not created just to look: an estimate must not write
    // to the index or leave an empty one behind.
    let conn = if db.exists() {
        Some(encryption::open_read_only(&db)?)
    } else {
        None
    };
    paths
        .iter()
        .map(|p| {
            let root = index::resolve_root(p, &s.excludes)?;
            let known = match &conn {
                Some(c) => index::stamps_under(c, &root)?,
                None => Default::default(),
            };
            estimate::estimate(&root, &s.excludes, &s.types, &known, BIGGEST_FOLDERS)
        })
        .collect()
}

fn print_rates_note(device: &str) {
    if let Some(note) = estimate::rates_note(device) {
        eprintln!("{note}");
    }
}

/// An estimate this long or longer also names the folders that take longest.
const NAMES_FOLDERS_FROM_SECS: f64 = 60.0;

/// Say what indexing `paths` will cost and, at a terminal and without
/// `yes`, ask. False if the answer was no.
fn confirm(paths: &[PathBuf], s: &Settings, yes: bool) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    let ests = estimates(paths, s)?;
    let ask = !yes && std::io::stdin().is_terminal();
    let total = sum(&ests);
    if ask && total.secs() >= NAMES_FOLDERS_FROM_SECS {
        for est in &ests {
            if !est.folders.is_empty() {
                eprintln!("Longest to index under {}:", est.root.display());
            }
            for (dir, secs, files) in &est.folders {
                eprintln!(
                    "  {}  {}",
                    dir.display(),
                    estimate::folder_summary(*secs, *files)
                );
            }
        }
        eprintln!(
            "Leave one out with `exclude` in config.toml or the settings window's Folders page."
        );
        print_rates_note(&s.device);
    }
    let line = match ests.as_slice() {
        [one] => format!("{}: {}", one.root.display(), one.summary()),
        _ => {
            for est in &ests {
                eprintln!("{}: {}", est.root.display(), est.summary());
            }
            format!("All together: {}", total.summary())
        }
    };
    if !ask || total.files() == 0 {
        eprintln!("{line}");
        return Ok(true);
    }
    eprint!("{line}. Continue? [Y/n] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer)? == 0 {
        eprintln!();
        return Ok(false);
    }
    Ok(is_yes(&answer))
}

fn sum(estimates: &[estimate::Estimate]) -> estimate::Estimate {
    let mut total = estimate::Estimate::default();
    for est in estimates {
        for (t, add) in total.kinds.iter_mut().zip(&est.kinds) {
            t.files += add.files;
            t.bytes += add.bytes;
            t.chunks += add.chunks;
        }
        total.up_to_date += est.up_to_date;
    }
    total
}

/// Enter alone is a yes: the prompt's default.
fn is_yes(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_alone_and_any_case_of_yes_continue() {
        for answer in ["\n", "y\n", "Y\n", " yes \n", "YES"] {
            assert!(is_yes(answer), "{answer:?}");
        }
    }

    #[test]
    fn anything_else_stops() {
        for answer in ["n\n", "no\n", "yess\n", "q\n"] {
            assert!(!is_yes(answer), "{answer:?}");
        }
    }
}
