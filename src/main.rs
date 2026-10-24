mod embed;
mod extract;
mod icons;
mod index;
mod krunner;
mod menu;
mod model;
mod parity;
mod proc;
mod runtime;
mod sched;
mod vision;
mod watch;
mod worker;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;
use vinosearch_shared::config;

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
    /// Serve KRunner, GNOME Shell and the settings app over D-Bus, or install/remove the service.
    Krunner {
        /// Write the service files and the running desktop's search place (KRunner on Plasma,
        /// GNOME Shell on GNOME, none on other desktops, both when there's no desktop session),
        /// then exit. `integrate` adds or removes one.
        #[arg(long)]
        install: bool,
        /// Remove the service and every search place, then exit.
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// Turn one search place on (or off with --off): kde is KRunner and the Plasma launcher,
    /// gnome is GNOME Shell's overview search, from the next login.
    Integrate {
        place: krunner::Place,
        /// Remove this place's files instead of writing them. The service stays.
        #[arg(long)]
        off: bool,
    },
    /// Keep the index current: watch every indexed root and re-index changes.
    Watch,
    /// Search from a dmenu-style launcher (fuzzel, wofi, rofi, tofi, bemenu) and open the
    /// pick, through the running service. For sway, Hyprland and other desktops without
    /// KRunner or GNOME Shell: bind it to a key.
    Menu {
        /// A dmenu-style command to use instead (choices on stdin, the pick or typed text on
        /// stdout), split on spaces [default: the first of fuzzel, wofi, rofi, tofi, bemenu found]
        #[arg(long, env = "VINOSEARCH_LAUNCHER")]
        launcher: Option<String>,
    },
    /// Search the index.
    Search {
        query: String,
        /// Number of results.
        #[arg(short, default_value_t = 10)]
        n: usize,
    },
    /// Indexed images that look like one: an indexed image costs nothing to
    /// encode, any other image one encode on the device.
    Similar {
        image: PathBuf,
        /// Number of results.
        #[arg(short, default_value_t = 10)]
        n: usize,
        /// Leave out images scoring below this (cosine, -1 to 1). Image to
        /// image scores sit well above text to image ones [default: none]
        #[arg(long)]
        min_score: Option<f32>,
    },
    /// Parse the document on stdin as PATH's type and write its chunks to
    /// stdout as JSON, under memory and CPU limits. The indexer runs this on
    /// itself for every document (#84).
    #[command(hide = true)]
    Extract { path: PathBuf },
}

/// OpenVINO's compiled-model cache.
pub(crate) fn cache_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("no HOME"))?)
            .join(".cache"),
    };
    Ok(base.join("vinosearch/ov-cache"))
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
    let cache = cache_dir()?;
    let t0 = Instant::now();
    let emb = embed::Embedder::new(
        openvino::Core::new()?,
        &dir,
        &model::vision_dir()?,
        device,
        &cache,
    )?;
    eprintln!(
        "model ready on {device} in {:.1} s",
        t0.elapsed().as_secs_f64()
    );
    Ok(emb)
}

/// What the runner found when it last compiled each stage on `device` (#76).
fn print_parity(device: &str) -> Result<()> {
    if device == "CPU" {
        println!("parity:    CPU is the reference");
        return Ok(());
    }
    let min = parity::MIN_COSINE;
    let checks = embed::parity(&cache_dir()?, device);
    if checks.iter().all(|(_, cosine)| cosine.is_none()) {
        println!("parity:    not checked yet; each stage is the next time it loads on {device}");
        return Ok(());
    }
    let found: Vec<String> = checks
        .iter()
        .map(|(stage, cosine)| match cosine {
            Some(c) => format!("{stage} {c:.5}"),
            None => format!("{stage} not checked yet"),
        })
        .collect();
    println!(
        "parity:    {} against the CPU (min {min})",
        found.join(", ")
    );
    for (stage, cosine) in &checks {
        if let Some(c) = cosine.filter(|c| *c < min) {
            println!("warning:   the {stage} stage gave {c:.4} on {device}, so it runs on CPU");
        }
    }
    Ok(())
}

/// Flags over the config file over defaults.
struct Settings {
    path: PathBuf,
    file: config::Config,
    device: String,
    idle_unload_minutes: f64,
    excludes: config::Excludes,
    types: index::FileTypes,
    pinned: worker::Pinned,
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
        .unwrap_or(config::DEFAULT_IDLE_UNLOAD_MINUTES);
    config::idle_unload(idle_unload_minutes)?;
    Ok(Settings {
        device: cli
            .device
            .clone()
            .or(file.device.clone())
            .unwrap_or_else(|| config::DEFAULT_DEVICE.into()),
        idle_unload_minutes,
        excludes: file.excludes()?,
        types: index::FileTypes::new(&file.formats)?,
        pinned: worker::Pinned {
            device: cli.device.is_some(),
            idle_unload: cli.idle_unload_minutes.is_some(),
        },
        file,
        path,
    })
}

fn worker_options(cli: &Cli, s: Settings) -> worker::Options {
    worker::Options {
        openvino_dir: cli.openvino_dir.clone(),
        status: std::sync::Arc::new(worker::Status::new(&s.device)),
        device: s.device,
        idle: config::idle_unload(s.idle_unload_minutes).expect("checked in settings()"),
        excludes: s.excludes,
        types: s.types,
        config: s.path,
        file: s.file,
        pinned: s.pinned,
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
    // Before the settings: a broken config file must not stop every document.
    if let Command::Extract { path } = &cli.command {
        extract::child_main(path)?;
        return Ok(true);
    }
    let s = settings(&cli)?;
    match cli.command {
        Command::Doctor => {
            let prefix = runtime::default_prefix(cli.openvino_dir)?;
            let ok = runtime::doctor(&prefix, &s.device)?;
            print_parity(&s.device)?;
            Ok(ok)
        }
        Command::FetchModel => {
            let mut last = String::new();
            let mut shown = 0;
            model::fetch(s.file.formats.images_enabled(), &mut |name, done, total| {
                // A line per file, then a percentage every 10%.
                let pct = done * 100 / total.max(1);
                if name != last || pct >= shown + 10 {
                    eprintln!("{pct:3}%  {name}");
                    last = name.to_owned();
                    shown = pct - pct % 10;
                }
            })?;
            eprintln!("model ready: {}", model::default_dir()?.display());
            Ok(true)
        }
        Command::Index { paths } => {
            if paths.is_empty() {
                bail!("give at least one path to index");
            }
            let mut emb = embedder(cli.openvino_dir.clone(), &s.device)?;
            let mut conn = index::open(&index::default_db()?)?;
            if let Some(why) = worker::model_changed(&index::purge_other_models(&conn)?) {
                eprintln!("{why}");
            }
            index::retry_undecodable(&conn)?;
            index::reread_formats(&conn)?;
            for p in &paths {
                let t0 = Instant::now();
                let mut shown = 0;
                let st = index::index(
                    &mut conn,
                    &mut emb,
                    Path::new(p),
                    &s.excludes,
                    &s.types,
                    &mut |st, _, _| {
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
                    },
                )?;
                if st.files_embedded > 0 {
                    eprintln!();
                }
                println!(
                    "{}: {} files seen, {} embedded ({} chunks), {} duplicates, {} removed",
                    p.display(),
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
                krunner::serve(worker_options(&cli, s))?;
            }
            Ok(true)
        }
        Command::Integrate { place, off } => {
            krunner::integrate(place, !off)?;
            Ok(true)
        }
        Command::Menu { launcher } => {
            menu::run(launcher.as_deref())?;
            Ok(true)
        }
        Command::Watch => {
            let (tx, rx) = std::sync::mpsc::channel();
            let opts = worker_options(&cli, s);
            worker::prepare(&opts.status)?;
            let _watcher = watch::spawn(
                tx,
                opts.config.clone(),
                opts.file.clone(),
                opts.status.clone(),
            )?;
            worker::run(rx, opts)?;
            Ok(true)
        }
        Command::Search { query, n } => {
            let mut emb = embedder(cli.openvino_dir, &s.device)?;
            let conn = index::open(&index::default_db()?)?;
            index::purge(&conn, &s.excludes)?;
            index::purge_types(&conn, &s.types)?;
            index::purge_other_models(&conn)?;
            let t0 = Instant::now();
            let q = emb.query(&query)?;
            let t1 = Instant::now();
            let vectors = index::Vectors::load(&conn)?;
            let mut hits = Vec::new();
            for kind in [index::Kind::Text, index::Kind::Document, index::Kind::Image] {
                hits.extend(index::search(&conn, &vectors, &q, kind, n)?);
            }
            eprintln!(
                "query {:.1} ms, search {:.1} ms",
                (t1 - t0).as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
            for h in hits {
                if h.kind == index::Kind::Image {
                    println!("{:.3}  {}  (image)", h.score, h.path);
                    continue;
                }
                let at = match h.kind {
                    index::Kind::Document => {
                        format!(", {} {}", extract::unit(Path::new(&h.path)), h.line)
                    }
                    _ => format!(":{}", h.line),
                };
                println!(
                    "{:.3}  {}{at}\n       {}",
                    h.score,
                    h.path,
                    snippet(&h.text)
                );
                if !h.copies.is_empty() {
                    println!("       same as: {}", h.copies.join(", "));
                }
            }
            Ok(true)
        }
        Command::Similar {
            image,
            n,
            min_score,
        } => {
            let conn = index::open(&index::default_db()?)?;
            let path = std::fs::canonicalize(&image).unwrap_or(image);
            let key = path.to_string_lossy().into_owned();
            let t0 = Instant::now();
            let q = match index::image_vector(&conn, &key)? {
                Some(v) => v,
                None => {
                    let bytes = std::fs::read(&path)?;
                    let pixels = vision::prepare(&vision::load(&bytes)?)?;
                    let mut emb = embedder(cli.openvino_dir, &s.device)?;
                    emb.image(&pixels)?
                }
            };
            let t1 = Instant::now();
            let vectors = index::Vectors::load(&conn)?;
            // One more than asked, for the image itself.
            let hits = index::search(&conn, &vectors, &q, index::Kind::Image, n + 1)?;
            eprintln!(
                "vector {:.1} ms, search {:.1} ms",
                (t1 - t0).as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
            let shown = hits
                .into_iter()
                .filter(|h| h.path != key && min_score.is_none_or(|m| h.score >= m))
                .take(n);
            for h in shown {
                println!("{:.3}  {}", h.score, h.path);
                if !h.copies.is_empty() {
                    println!("       same as: {}", h.copies.join(", "));
                }
            }
            Ok(true)
        }
        Command::Extract { .. } => unreachable!("handled before the settings"),
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
