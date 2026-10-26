mod code;
mod embed;
mod encryption;
mod estimate;
mod eval;
mod extract;
mod icons;
mod index;
mod krunner;
mod menu;
mod model;
mod ocr;
mod open;
mod parity;
mod power;
mod proc;
mod results;
mod runtime;
mod sched;
mod seal;
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

fn worker_options(cli: &Cli, s: Settings) -> Result<worker::Options> {
    Ok(worker::Options {
        openvino_dir: cli.openvino_dir.clone(),
        status: std::sync::Arc::new(worker::Status::new(&s.device)),
        device: s.device,
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

/// Subfolders `estimate` names as taking longest.
const BIGGEST_FOLDERS: usize = 5;

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

/// Say what indexing `paths` will cost and, at a terminal and without
/// `yes`, ask. False if the answer was no.
fn confirm_index(paths: &[PathBuf], s: &Settings, yes: bool) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    let ests = estimates(paths, s)?;
    let ask = !yes && std::io::stdin().is_terminal();
    let mut total = estimate::Estimate::default();
    for est in &ests {
        for (t, add) in total.kinds.iter_mut().zip(&est.kinds) {
            t.files += add.files;
            t.bytes += add.bytes;
            t.chunks += add.chunks;
        }
        total.up_to_date += est.up_to_date;
    }
    if ask && total.secs() >= 60.0 {
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
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    ))
}

fn run(cli: Cli) -> Result<bool> {
    // Before the settings: a broken config file must not stop every document.
    if let Command::Extract { path, from, count } = &cli.command {
        extract::child_main(path, *from, *count)?;
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
        Command::Index { paths, yes } => {
            if paths.is_empty() {
                bail!("give at least one path to index");
            }
            if !confirm_index(&paths, &s, yes)? {
                eprintln!(
                    "Nothing indexed. Add folders later with vinosearch index DIR or the settings window."
                );
                return Ok(true);
            }
            let mut emb = embedder(cli.openvino_dir.clone(), &s.device)?;
            let db = index::default_db()?;
            // As the config file asks before anything is written (#4); held
            // meanwhile, so the runner doesn't convert it underneath.
            let enc = encryption::apply(
                &db,
                encryption::Want::of(&s.file),
                false,
                &seal::Sealer::default(),
            );
            for msg in [&enc.broken, &enc.error] {
                if !msg.is_empty() {
                    eprintln!("encryption: {msg}");
                }
            }
            encryption::hold_shared(&db)?;
            let mut conn = index::open(&db)?;
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
            encryption::save_roots(&db, &conn)?;
            // Then the rest of long documents (#108), which the service
            // leaves until nothing else is waiting.
            let t0 = Instant::now();
            let mut chunks = 0;
            while let Some(rest) =
                index::index_rest(&mut conn, &mut emb, &s.types, &mut |_, _| Ok(()))?
            {
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
            Ok(true)
        }
        Command::Estimate { paths, json } => {
            if paths.is_empty() {
                bail!("give at least one path to estimate");
            }
            for est in estimates(&paths, &s)? {
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
                krunner::serve(worker_options(&cli, s)?)?;
            }
            Ok(true)
        }
        Command::Integrate { place, off } => {
            krunner::integrate(place, !off)?;
            Ok(true)
        }
        Command::Menu { launcher } => {
            menu::run(launcher.as_deref(), s.file.editor.as_deref())?;
            Ok(true)
        }
        Command::Watch => {
            let (tx, rx) = std::sync::mpsc::channel();
            let opts = worker_options(&cli, s)?;
            worker::prepare(&opts.status, &opts.file)?;
            power::spawn(tx.clone(), opts.status.clone());
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
            let (text, kinds) = index::scope(&query);
            let q = emb.query_for(text, kinds)?;
            let t1 = Instant::now();
            let vectors = index::Vectors::load(&conn)?;
            let words = index::Words::find(&conn, text);
            let mut hits = Vec::new();
            for kind in kinds {
                hits.extend(index::search(
                    &conn,
                    &vectors,
                    &q,
                    words.as_ref(),
                    *kind,
                    n,
                )?);
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
                    index::Kind::Document if !extract::is_document(Path::new(&h.path)) => {
                        " (text in the image)".to_owned()
                    }
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
            let emb = embedder(cli.openvino_dir, &s.device)?;
            eval::run(&opts, &q, &s.device, emb)
        }
        Command::ReadText { path } => {
            let prefix = runtime::default_prefix(cli.openvino_dir)?;
            runtime::load(&prefix)?;
            let mut core = openvino::Core::new()?;
            let t0 = Instant::now();
            let mut ocr = ocr::Ocr::new(&mut core, &model::ocr_dir()?, &s.device, &cache_dir()?)?;
            eprintln!(
                "ocr ready on {} in {:.1} s",
                ocr.device,
                t0.elapsed().as_secs_f64()
            );
            let bytes = std::fs::read(&path)?;
            let deadline = Instant::now() + ocr::SCAN_WALL;
            let pages = if extract::is_document(&path) {
                ocr::pdf_pages(&bytes, deadline)?.min(ocr::MAX_SCAN_PAGES)
            } else {
                1
            };
            for page in 1..=pages {
                let img = if extract::is_document(&path) {
                    ocr::render_page(&bytes, page, deadline)?
                } else {
                    vision::load(&bytes)?
                };
                let t = Instant::now();
                let lines = ocr.read(&img, 1.0)?;
                eprintln!(
                    "{}x{}: {} lines in {:.0} ms",
                    img.width(),
                    img.height(),
                    lines.len(),
                    t.elapsed().as_secs_f64() * 1e3
                );
                if pages > 1 {
                    println!("--- page {page}");
                }
                for l in lines {
                    println!("{l}");
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
            let hits = index::search(&conn, &vectors, &q, None, index::Kind::Image, n + 1)?;
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
