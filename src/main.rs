mod config;
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
    /// Settings file [default: ~/.config/vinosearch/config.toml]
    #[arg(long, global = true, env = "VINOSEARCH_CONFIG")]
    config: Option<PathBuf>,
    /// Device the model runs on [default: config, else NPU]
    #[arg(long, global = true)]
    device: Option<String>,
    /// Free the model after this many minutes without a query; 0 keeps it loaded [default: config, else 5]
    #[arg(long, global = true, env = "VINOSEARCH_IDLE_UNLOAD_MINUTES")]
    idle_unload_minutes: Option<f64>,
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

/// Flags over the config file over defaults.
struct Settings {
    path: PathBuf,
    device: String,
    idle_unload_minutes: f64,
    excludes: config::Excludes,
}

fn settings(cli: &Cli) -> Result<Settings> {
    let path = match &cli.config {
        Some(p) => p.clone(),
        None => config::default_path()?,
    };
    let file = config::Config::load(&path)?;
    let idle_unload_minutes = cli
        .idle_unload_minutes
        .or(file.idle_unload_minutes)
        .unwrap_or(5.0);
    if !(idle_unload_minutes >= 0.0 && idle_unload_minutes.is_finite()) {
        bail!("idle_unload_minutes must be 0 or more");
    }
    Ok(Settings {
        device: cli
            .device
            .clone()
            .or(file.device.clone())
            .unwrap_or_else(|| "NPU".into()),
        idle_unload_minutes,
        excludes: file.excludes()?,
        path,
    })
}

fn worker_options(cli: &Cli, s: Settings, preload: bool) -> worker::Options {
    let m = s.idle_unload_minutes;
    worker::Options {
        openvino_dir: cli.openvino_dir.clone(),
        device: s.device,
        idle: (m > 0.0).then(|| std::time::Duration::from_secs_f64(m * 60.0)),
        preload,
        excludes: s.excludes,
        config: s.path,
    }
}

pub(crate) fn snippet(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(140) {
        Some((i, _)) => format!("{}...", &flat[..i]),
        None => flat,
    }
}

fn run(cli: Cli) -> Result<bool> {
    let s = settings(&cli)?;
    match cli.command {
        Command::Doctor => {
            let prefix = runtime::default_prefix(cli.openvino_dir)?;
            runtime::doctor(&prefix, &s.device)
        }
        Command::FetchModel => {
            model::fetch(&model::default_dir()?)?;
            Ok(true)
        }
        Command::Index { paths } => {
            if paths.is_empty() {
                bail!("give at least one path to index");
            }
            let mut emb = embedder(cli.openvino_dir.clone(), &s.device)?;
            let mut conn = index::open(&index::default_db()?)?;
            for p in &paths {
                let st = index::index(&mut conn, &mut emb, Path::new(p), &s.excludes)?;
                println!(
                    "{}: {} files seen, {} embedded ({} chunks), {} removed",
                    p.display(),
                    st.files_seen,
                    st.files_embedded,
                    st.chunks,
                    st.removed
                );
            }
            Ok(true)
        }
        Command::Krunner { install, uninstall } => {
            if install {
                krunner::install(
                    cli.openvino_dir.as_deref(),
                    cli.device.as_deref(),
                    cli.idle_unload_minutes,
                )?;
            } else if uninstall {
                krunner::uninstall()?;
            } else {
                krunner::serve(worker_options(&cli, s, true))?;
            }
            Ok(true)
        }
        Command::Watch => {
            let (tx, rx) = std::sync::mpsc::channel();
            let opts = worker_options(&cli, s, false);
            let _watcher = watch::spawn(tx, opts.config.clone(), opts.excludes.clone())?;
            worker::run(rx, opts)?;
            Ok(true)
        }
        Command::Search { query, n } => {
            let mut emb = embedder(cli.openvino_dir, &s.device)?;
            let conn = index::open(&index::default_db()?)?;
            index::purge(&conn, &s.excludes)?;
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
