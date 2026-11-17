mod cli;
mod eval;
mod extract;
mod index;
mod ml;
mod proc;
mod search;
mod service;

use cli::service::KrunnerMode;
use ml::{embed, model, runtime};
use search::menu;
use service::{install, krunner, worker};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;
use vinosearch_shared::config;

/// Semantic file search, run on the NPU.
#[derive(Parser)]
#[command(version = vinosearch_shared::version::VERSION)]
struct Cli {
    /// ovfetch prefix holding the OpenVINO runtime [default: ~/.local/lib/vinosearch/openvino]
    #[arg(long, global = true, env = "VINOSEARCH_OPENVINO_DIR")]
    openvino_dir: Option<PathBuf>,
    /// Settings file [default: ~/.config/vinosearch/config.toml]
    #[arg(long, global = true, env = "VINOSEARCH_CONFIG")]
    config: Option<PathBuf>,
    /// Device the model runs on [default: config, else the first of NPU, GPU and CPU found]
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
    /// Delete the downloaded models (text, vision, OCR and audio). A running service is asked
    /// to, so it unloads them first. The index is kept; search needs fetch-model again.
    DeleteModel,
    /// Index text files under each path. Unchanged files are skipped. Says first how long it
    /// will take and, at a terminal, asks.
    Index {
        paths: Vec<PathBuf>,
        /// Don't ask before indexing.
        #[arg(short, long)]
        yes: bool,
    },
    /// Say how long indexing each path would take and how much index it would make, from a
    /// walk that reads no file contents, and which subfolders take longest.
    Estimate {
        paths: Vec<PathBuf>,
        /// One JSON object per path, for the settings window.
        #[arg(long)]
        json: bool,
    },
    /// Serve KRunner, GNOME Shell and the settings app over D-Bus, starting the service again
    /// if it crashes, or install/remove the service.
    Krunner {
        /// Write the service files and the running desktop's search place (KRunner on Plasma,
        /// GNOME Shell on GNOME, none on other desktops, both when there's no desktop session),
        /// start the service (again, if it is running), then exit. `integrate` adds or removes
        /// one.
        #[arg(long)]
        install: bool,
        /// Stop the service and remove it and every search place, then exit.
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
        /// Serve, without the parent that starts it again: what `krunner` runs.
        #[arg(long, hide = true, conflicts_with_all = ["install", "uninstall"])]
        worker: bool,
    },
    /// Turn one search place on (or off with --off): kde is KRunner and the Plasma launcher,
    /// gnome is GNOME Shell's overview search, from the next login, menu is the search window
    /// in the app menu.
    Integrate {
        place: install::Place,
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
    /// Index a testbed (scripts/make-testbed.sh) into a throwaway index, run
    /// every query in the queries file through KRunner's search, and print
    /// recall@1, recall@5 and MRR. Fails when one is below its threshold.
    Eval {
        testbed: PathBuf,
        /// Queries, expected files and thresholds.
        #[arg(long, default_value = "evals/queries.toml")]
        queries: PathBuf,
        /// Index the pictures too and run the image queries. Needs the vision
        /// encoder (fetch-model with images on).
        #[arg(long)]
        images: bool,
        /// One JSON object instead of the table.
        #[arg(long)]
        json: bool,
        /// Encrypt the throwaway index (SQLCipher, a key held in memory), to
        /// see what encryption costs.
        #[arg(long)]
        encrypted: bool,
        /// Lowest recall@1 that passes [default: the queries file's]
        #[arg(long)]
        min_recall_at_1: Option<f64>,
        /// Lowest recall@5 that passes [default: the queries file's]
        #[arg(long)]
        min_recall_at_5: Option<f64>,
        /// Lowest MRR that passes [default: the queries file's]
        #[arg(long)]
        min_mrr: Option<f64>,
    },
    /// Print the text OCR reads in an image, or a PDF's rendered pages, as
    /// indexing reads it (#106).
    ReadText { path: PathBuf },
    /// Parse the document on stdin as PATH's type and write the chunks of
    /// parts FROM..FROM+COUNT (pages, say) to stdout as JSON, under memory
    /// and CPU limits. The indexer runs this on itself for every document
    /// (#84), a run of parts at a time (#108).
    #[command(hide = true)]
    Extract {
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        from: usize,
        #[arg(long, default_value_t = usize::MAX)]
        count: usize,
    },
    /// Write the files the Flatpak exports (the app menu entry, icon, D-Bus
    /// activation file and search places) under SHARE, for the manifest.
    #[command(hide = true)]
    FlatpakExports {
        share: PathBuf,
        #[arg(long)]
        app_id: String,
    },
    /// In the Flatpak: install release TAG's bundle [default: $VINO_UPDATE_TAG] and restart
    /// the service on it. The settings window's Update button.
    #[command(hide = true)]
    FlatpakUpdate { tag: Option<String> },
}

/// OpenVINO's compiled-model cache.
pub(crate) fn cache_dir() -> Result<PathBuf> {
    Ok(vinosearch_shared::xdg::cache_home()?.join("vinosearch/ov-cache"))
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
        &model::ocr_dir()?,
        device,
        &cache,
    )?;
    eprintln!(
        "model ready on {device} in {:.1} s",
        t0.elapsed().as_secs_f64()
    );
    Ok(emb)
}

/// Flags over the config file over defaults.
struct Settings {
    path: PathBuf,
    file: config::Config,
    device: String,
    /// What a config file with no device means: the best device found, for
    /// commands that load a model.
    default_device: String,
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
    let default_device = if loads_model(&cli.command) && cli.device.is_none() {
        runtime::best_device(&runtime::default_prefix(cli.openvino_dir.clone())?)
    } else {
        config::DEFAULT_DEVICE.into()
    };
    Ok(Settings {
        device: cli
            .device
            .clone()
            .or(file.device.clone())
            .unwrap_or_else(|| default_device.clone()),
        default_device,
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

/// Whether `command` runs a model, so needs a device. The device isn't
/// looked for otherwise: finding it starts OpenVINO.
fn loads_model(command: &Command) -> bool {
    match command {
        Command::Doctor
        | Command::Index { .. }
        | Command::Estimate { .. }
        | Command::Watch
        | Command::Search { .. }
        | Command::Similar { .. }
        | Command::Eval { .. }
        | Command::ReadText { .. } => true,
        Command::Krunner { worker, .. } => *worker,
        Command::FetchModel
        | Command::DeleteModel
        | Command::Integrate { .. }
        | Command::Menu { .. }
        | Command::Extract { .. }
        | Command::FlatpakExports { .. }
        | Command::FlatpakUpdate { .. } => false,
    }
}

fn worker_options(openvino_dir: Option<PathBuf>, s: Settings) -> Result<worker::Options> {
    Ok(worker::Options {
        openvino_dir,
        status: std::sync::Arc::new(worker::Status::new(&s.device)),
        device: s.device,
        default_device: s.default_device,
        idle: config::idle_unload(s.idle_unload_minutes).expect("checked in settings()"),
        excludes: s.excludes,
        types: s.types,
        config: s.path,
        file: s.file,
        pinned: s.pinned,
        db: index::default_db()?,
    })
}

pub(crate) fn snippet(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(140) {
        Some((i, _)) => format!("{}...", &flat[..i]),
        None => flat,
    }
}

/// Commands that must work whatever the config file says, so they run
/// before it is read: every document's parser child, and the update that
/// is how a broken release gets fixed.
fn run_without_settings(command: &Command) -> Option<Result<bool>> {
    match command {
        Command::Extract { path, from, count } => {
            Some(extract::child_main(path, *from, *count).map(|()| true))
        }
        Command::FlatpakExports { share, app_id } => {
            Some(service::flatpak::export(share, app_id).map(|()| true))
        }
        Command::FlatpakUpdate { tag } => Some(service::flatpak_update::run(tag.clone())),
        _ => None,
    }
}

fn run(cli: Cli) -> Result<bool> {
    if let Some(done) = run_without_settings(&cli.command) {
        return done;
    }
    let s = settings(&cli)?;
    let Cli {
        openvino_dir,
        device,
        idle_unload_minutes,
        command,
        ..
    } = cli;
    match command {
        Command::Doctor => cli::models::doctor(openvino_dir, &s),
        Command::FetchModel => cli::models::fetch(&s),
        Command::DeleteModel => cli::models::delete(),
        Command::Index { paths, yes } => cli::index::index(&paths, yes, openvino_dir, &s),
        Command::Estimate { paths, json } => cli::index::estimate(&paths, json, &s),
        Command::Krunner {
            install,
            uninstall,
            worker,
        } => match KrunnerMode::of(install, uninstall, worker) {
            KrunnerMode::Install => {
                install::install(
                    openvino_dir.as_deref(),
                    device.as_deref(),
                    idle_unload_minutes,
                )?;
                Ok(true)
            }
            KrunnerMode::Uninstall => {
                install::uninstall()?;
                Ok(true)
            }
            KrunnerMode::Worker => cli::service::serve_and_exit(worker_options(openvino_dir, s)?),
            KrunnerMode::Supervise => krunner::supervise(),
        },
        Command::Integrate { place, off } => {
            install::integrate(place, !off)?;
            Ok(true)
        }
        Command::Menu { launcher } => {
            menu::run(
                launcher.as_deref(),
                s.file.editor.as_deref(),
                s.file.player.as_deref(),
            )?;
            Ok(true)
        }
        Command::Watch => cli::service::watch(worker_options(openvino_dir, s)?),
        Command::Search { query, n } => cli::search::search(&query, n, openvino_dir, &s),
        Command::Eval {
            testbed,
            queries,
            images,
            json,
            encrypted,
            min_recall_at_1,
            min_recall_at_5,
            min_mrr,
        } => {
            let q = eval::Queries::load(&queries)?;
            if images && !model::vision_present(&model::default_dir()?, &model::vision_dir()?) {
                bail!("--images needs the vision encoder; run fetch-model with images on");
            }
            let t = q.thresholds;
            let opts = eval::Options {
                testbed,
                images,
                json,
                encrypted,
                thresholds: eval::Thresholds {
                    recall_at_1: min_recall_at_1.unwrap_or(t.recall_at_1),
                    recall_at_5: min_recall_at_5.unwrap_or(t.recall_at_5),
                    mrr: min_mrr.unwrap_or(t.mrr),
                },
            };
            let emb = embedder(openvino_dir, &s.device)?;
            eval::run(&opts, &q, &s.device, emb)
        }
        Command::ReadText { path } => cli::models::read_text(&path, openvino_dir, &s),
        Command::Similar {
            image,
            n,
            min_score,
        } => cli::search::similar(image, n, min_score, openvino_dir, &s),
        Command::Extract { .. }
        | Command::FlatpakExports { .. }
        | Command::FlatpakUpdate { .. } => {
            unreachable!("handled before the settings")
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(cli::service::failure_code(&e))
        }
    }
}
