mod embed;
mod index;
mod krunner;
mod model;
mod runtime;
mod watch;
mod worker;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

/// Semantic file search, run on the NPU.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// ovfetch prefix holding the OpenVINO runtime [default: ~/.local/lib/vinosearch/openvino]
    #[arg(long, global = true, env = "VINOSEARCH_OPENVINO_DIR")]
    openvino_dir: Option<PathBuf>,
    /// Device the model runs on.
    #[arg(long, global = true, default_value = "NPU")]
    device: String,
    /// Free the model after this many minutes without a query; 0 keeps it loaded.
    #[arg(
        long,
        global = true,
        env = "VINOSEARCH_IDLE_UNLOAD_MINUTES",
        default_value_t = 5.0
    )]
    idle_unload_minutes: f64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check the NPU, its driver, and the OpenVINO runtime, then compile and run a probe model.
    Doctor,
    /// Download the embedding model, checking every file against its pinned SHA-256.
    FetchModel,
    /// Index text files under each path. Unchanged files are skipped.
    Index { paths: Vec<PathBuf> },
    /// Serve KRunner over D-Bus, or register/unregister the runner with it.
    Krunner {
        /// Write the KRunner plugin and D-Bus service files, then exit.
        #[arg(long)]
        install: bool,
        /// Remove those files, then exit.
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// Keep the index current: watch every indexed root and re-index changes.
    Watch,
    /// Search the index.
    Search {
        query: String,
        /// Number of results.
        #[arg(short, default_value_t = 10)]
        n: usize,
    },
}

pub(crate) fn embedder(openvino_dir: Option<PathBuf>, device: &str) -> Result<embed::Embedder> {
    let prefix = runtime::default_prefix(openvino_dir)?;
    runtime::load(&prefix)?;
    let dir = model::default_dir()?;
    if !model::present(&dir) {
        bail!(
            "model not found in {}; run: vinosearch fetch-model",
            dir.display()
        );
    }
    let cache = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".cache/vinosearch/ov-cache");
    let t0 = Instant::now();
    let emb = embed::Embedder::new(openvino::Core::new()?, &dir, device, &cache)?;
    eprintln!(
        "model ready on {device} in {:.1} s",
        t0.elapsed().as_secs_f64()
    );
    Ok(emb)
}

fn worker_options(cli: &Cli, preload: bool) -> Result<worker::Options> {
    let m = cli.idle_unload_minutes;
    if !(m >= 0.0 && m.is_finite()) {
        bail!("--idle-unload-minutes must be 0 or more");
    }
    Ok(worker::Options {
        openvino_dir: cli.openvino_dir.clone(),
        device: cli.device.clone(),
        idle: (m > 0.0).then(|| std::time::Duration::from_secs_f64(m * 60.0)),
        preload,
    })
}

pub(crate) fn snippet(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(140) {
        Some((i, _)) => format!("{}...", &flat[..i]),
        None => flat,
    }
}

fn run(cli: Cli) -> Result<bool> {
    match cli.command {
        Command::Doctor => {
            let prefix = runtime::default_prefix(cli.openvino_dir)?;
            runtime::doctor(&prefix, &cli.device)
        }
        Command::FetchModel => {
            model::fetch(&model::default_dir()?)?;
            Ok(true)
        }
        Command::Index { paths } => {
            if paths.is_empty() {
                bail!("give at least one path to index");
            }
            let mut emb = embedder(cli.openvino_dir, &cli.device)?;
            let mut conn = index::open(&index::default_db()?)?;
            for p in &paths {
                let s = index::index(&mut conn, &mut emb, Path::new(p))?;
                println!(
                    "{}: {} files seen, {} embedded ({} chunks), {} removed",
                    p.display(),
                    s.files_seen,
                    s.files_embedded,
                    s.chunks,
                    s.removed
                );
            }
            Ok(true)
        }
        Command::Krunner { install, uninstall } => {
            if install {
                krunner::install(
                    cli.openvino_dir.as_deref(),
                    &cli.device,
                    cli.idle_unload_minutes,
                )?;
            } else if uninstall {
                krunner::uninstall()?;
            } else {
                krunner::serve(worker_options(&cli, true)?)?;
            }
            Ok(true)
        }
        Command::Watch => {
            let (tx, rx) = std::sync::mpsc::channel();
            let _watcher = watch::spawn(tx)?;
            worker::run(rx, worker_options(&cli, false)?)?;
            Ok(true)
        }
        Command::Search { query, n } => {
            let mut emb = embedder(cli.openvino_dir, &cli.device)?;
            let conn = index::open(&index::default_db()?)?;
            let t0 = Instant::now();
            let q = emb.query(&query)?;
            let t1 = Instant::now();
            let vectors = index::Vectors::load(&conn)?;
            let hits = index::search(&conn, &vectors, &q, n)?;
            eprintln!(
                "query {:.1} ms, search {:.1} ms",
                (t1 - t0).as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
            for h in hits {
                println!(
                    "{:.3}  {}:{}\n       {}",
                    h.score,
                    h.path,
                    h.line,
                    snippet(&h.text)
                );
            }
            Ok(true)
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
