//! `doctor`, `fetch-model`, `delete-model` and `read-text`.

use crate::ml::{embed, model, ocr, parity, runtime, vision};
use crate::service::{krunner, start};
use crate::{Settings, cache_dir, extract};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn doctor(openvino_dir: Option<PathBuf>, s: &Settings) -> Result<bool> {
    let prefix = runtime::default_prefix(openvino_dir)?;
    let ok = runtime::doctor(&prefix, &s.device)?;
    print_parity(&s.device)?;
    // The service's log, not this command's: in the Flatpak it is under ~/.var/app.
    println!("log:       {}", start::log_where()?);
    Ok(ok)
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

pub fn fetch(s: &Settings) -> Result<bool> {
    let mut progress = DownloadProgress::default();
    model::fetch(
        model::Wants::of(&s.file.formats),
        &mut |name, done, total| {
            if let Some(line) = progress.line(name, done, total) {
                eprintln!("{line}");
            }
        },
    )?;
    eprintln!("model ready: {}", model::default_dir()?.display());
    Ok(true)
}

/// A download's progress as lines: one when a file starts, then one each
/// time it is `STEP_PERCENT` further on.
#[derive(Default)]
struct DownloadProgress {
    file: String,
    shown_percent: u64,
}

impl DownloadProgress {
    const STEP_PERCENT: u64 = 10;

    fn line(&mut self, file: &str, done: u64, total: u64) -> Option<String> {
        let percent = done * 100 / total.max(1);
        if file == self.file && percent < self.shown_percent + Self::STEP_PERCENT {
            return None;
        }
        self.file = file.to_owned();
        self.shown_percent = percent - percent % Self::STEP_PERCENT;
        Some(format!("{percent:3}%  {file}"))
    }
}

pub fn delete() -> Result<bool> {
    let megabytes = model::bytes()? / 1_000_000;
    let dir = model::default_dir()?;
    let dir = dir.parent().unwrap_or(&dir);
    if krunner::delete_models()? {
        eprintln!(
            "asked the running service to delete {megabytes} MB of models in {}",
            dir.display()
        );
    } else {
        eprintln!("deleted {megabytes} MB of models in {}", dir.display());
    }
    Ok(true)
}

/// Print the text OCR reads in an image, or in each page of a PDF.
pub fn read_text(path: &Path, openvino_dir: Option<PathBuf>, s: &Settings) -> Result<bool> {
    let prefix = runtime::default_prefix(openvino_dir)?;
    runtime::load(&prefix)?;
    let mut core = openvino::Core::new()?;
    let t0 = Instant::now();
    let mut ocr = ocr::Ocr::new(&mut core, &model::ocr_dir()?, &s.device, &cache_dir()?)?;
    eprintln!(
        "ocr ready on {} in {:.1} s",
        ocr.device,
        t0.elapsed().as_secs_f64()
    );
    let bytes = std::fs::read(path)?;
    let deadline = Instant::now() + ocr::SCAN_WALL;
    let is_document = extract::is_document(path);
    let pages = if is_document {
        ocr::pdf_pages(&bytes, deadline)?.min(ocr::MAX_SCAN_PAGES)
    } else {
        1
    };
    for page in 1..=pages {
        let img = if is_document {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_gets_a_line_when_it_starts() {
        let mut progress = DownloadProgress::default();
        assert_eq!(
            progress.line("model.onnx", 0, 200).as_deref(),
            Some("  0%  model.onnx")
        );
        assert_eq!(
            progress.line("tokenizer.json", 1, 200).as_deref(),
            Some("  0%  tokenizer.json")
        );
    }

    #[test]
    fn nine_points_further_on_prints_nothing() {
        let mut progress = DownloadProgress::default();
        progress.line("model.onnx", 0, 100);
        assert_eq!(progress.line("model.onnx", 9, 100), None);
    }

    #[test]
    fn ten_points_further_on_prints_a_line() {
        let mut progress = DownloadProgress::default();
        progress.line("model.onnx", 0, 100);
        assert_eq!(
            progress.line("model.onnx", 10, 100).as_deref(),
            Some(" 10%  model.onnx")
        );
        assert_eq!(progress.line("model.onnx", 19, 100), None);
    }

    #[test]
    fn an_empty_file_does_not_divide_by_zero() {
        let mut progress = DownloadProgress::default();
        assert_eq!(progress.line("empty", 0, 0).as_deref(), Some("  0%  empty"));
    }
}
