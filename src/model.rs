//! The embedding model: EmbeddingGemma 2's text model, rotary-fixed and INT8,
//! and for images its vision encoder with static shapes, each pinned by
//! Hugging Face commit and SHA-256.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const REPO: &str = "arankays/embeddinggemma-2-text-int8-cw-onnx";
pub(crate) const REVISION: &str = "88aed926c26344fa2ffed5f44d016fb36e2fcce3";
const VISION_REPO: &str = "arankays/embeddinggemma-2-vision-static-onnx";
pub(crate) const VISION_REVISION: &str = "16414206c27ee4bc435a7c03467155063fee1d9f";

/// (path in the repo, sha256, size in bytes)
const FILES: &[(&str, &str, u64)] = &[
    // The text-only graph: OpenVINO's C API cannot build the zero-size media
    // inputs model.onnx takes.
    (
        "onnx/model_text.onnx",
        "cab66c5d0bf25beaadca4095c7c4cb2e06b05cb360059f5854d9e4b110b70af3",
        713_515,
    ),
    (
        "onnx/model.onnx_data",
        "5fa7999279f92b8e97dd74dee5478b58b58c2c28bfdcfd343c7e4e4cb4103dd7",
        273_296_384,
    ),
    // model_text.onnx with its inputs fixed at [1, 32] and [1, 256]
    // (bench/static_shapes.py), so the runner can compile straight from the
    // path and a cache hit never reads the weights.
    (
        "onnx/model_text_q32.onnx",
        "8ecbd32473fce1d7805ab4ef8ab9be69d12b3b95639231001cd30054096245bc",
        713_465,
    ),
    (
        "onnx/model_text_d256.onnx",
        "65e91c59827cfa791f090dab4b9091ff5a37e5393adb1fd77d6289ef89985049",
        713_467,
    ),
    (
        "tokenizer.json",
        "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4",
        32_170_510,
    ),
    // model_text.onnx taking the vision encoder's output, for images
    // (bench/static_image_text.py). Small, so fetched whether or not images are on.
    (
        "onnx/model_image_140.onnx",
        "e160498a7564bfa0f53b37bf648d0d72377206294acd0a4e48334fbea31cb144",
        713_476,
    ),
];

/// The vision encoder, fetched only while images are indexed.
const VISION_FILES: &[(&str, &str, u64)] = &[
    (
        "onnx/vision_encoder_static_140.onnx",
        "edfc29b9451ab3da75d8030b55285ec12006438b89d6aa8cd4f9729ffc35d813",
        121_271,
    ),
    (
        "onnx/vision_encoder.onnx_data",
        "ca8d28c99feb8129618f141978a27adae25090e0dadb68a30c235e191c01e37e",
        671_026_176,
    ),
];

/// Bytes the model takes on disk once fetched, the vision encoder included
/// if `images`.
pub fn total_bytes(images: bool) -> u64 {
    let size = |files: &[(&str, &str, u64)]| files.iter().map(|(_, _, size)| size).sum::<u64>();
    size(FILES) + if images { size(VISION_FILES) } else { 0 }
}

fn models_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".local/share"),
    };
    Ok(base.join("vinosearch/models"))
}

pub fn default_dir() -> Result<PathBuf> {
    Ok(models_dir()?.join("embeddinggemma-2-text-int8-cw-onnx"))
}

pub fn vision_dir() -> Result<PathBuf> {
    Ok(models_dir()?.join("embeddinggemma-2-vision-static-onnx"))
}

/// The vision encoder, in `vision_dir()`.
pub fn vision_path(dir: &Path) -> PathBuf {
    dir.join("onnx/vision_encoder_static_140.onnx")
}

/// The text model taking image features, in `default_dir()`.
pub fn image_text_path(dir: &Path) -> PathBuf {
    dir.join("onnx/model_image_140.onnx")
}

pub fn onnx_path(dir: &Path) -> PathBuf {
    dir.join("onnx/model_text.onnx")
}

/// The graph with inputs fixed at `[1, len]`, if one was published for `len`.
pub fn static_path(dir: &Path, len: usize) -> Option<PathBuf> {
    let tag = match len {
        32 => "q32",
        256 => "d256",
        _ => return None,
    };
    Some(dir.join(format!("onnx/model_text_{tag}.onnx")))
}

pub fn tokenizer_path(dir: &Path) -> PathBuf {
    dir.join("tokenizer.json")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Download whatever is missing or does not match its pinned hash, the vision
/// encoder too if `images`. Files are hashed as they stream and only renamed
/// into place once they match; one larger than its pinned size is abandoned
/// as it streams. `progress` gets (bytes done, bytes in total) across all
/// files, files already in place included.
pub fn fetch(images: bool, progress: &mut dyn FnMut(&str, u64, u64)) -> Result<()> {
    let total = total_bytes(images);
    let mut done = 0;
    fetch_files(
        REPO,
        REVISION,
        FILES,
        &default_dir()?,
        &mut done,
        total,
        progress,
    )?;
    if images {
        fetch_files(
            VISION_REPO,
            VISION_REVISION,
            VISION_FILES,
            &vision_dir()?,
            &mut done,
            total,
            progress,
        )?;
    }
    Ok(())
}

fn fetch_files(
    repo: &str,
    revision: &str,
    files: &[(&str, &str, u64)],
    dir: &Path,
    done: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> Result<()> {
    for (name, want, size) in files {
        let dest = dir.join(name);
        if dest.is_file() && sha256_file(&dest)? == *want {
            *done += size;
            progress(name, *done, total);
            continue;
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        let url = format!("https://huggingface.co/{repo}/resolve/{revision}/{name}");
        let mut resp = ureq::get(&url)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let mut body = resp.body_mut().as_reader();
        let part = dest.with_extension("part");
        let mut out = fs::File::create(&part)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut got_bytes = 0;
        loop {
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            got_bytes += n as u64;
            if got_bytes > *size {
                drop(out);
                let _ = fs::remove_file(&part);
                bail!("{name}: more than the pinned {size} bytes");
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            progress(name, *done + got_bytes, total);
        }
        *done += size;
        out.sync_all()?;
        let got = hex::encode(hasher.finalize());
        if got != *want {
            let _ = fs::remove_file(&part);
            bail!("{name}: sha256 {got}, expected {want}");
        }
        fs::rename(&part, &dest)?;
    }
    Ok(())
}

/// The files a text load needs are present. Hashes are checked by `fetch`,
/// not on every load. The static-shape graphs are optional: a model fetched
/// before they were published loads through the reshape path instead. So is
/// the image graph, which only images need.
pub fn present(dir: &Path) -> bool {
    FILES
        .iter()
        .filter(|(name, _, _)| !name.contains("model_text_") && !name.contains("model_image_"))
        .all(|(name, _, _)| dir.join(name).is_file())
}

/// What embedding an image needs, besides the text model, is present.
pub fn images_present(dir: &Path, vision_dir: &Path) -> bool {
    image_text_path(dir).is_file()
        && VISION_FILES
            .iter()
            .all(|(name, _, _)| vision_dir.join(name).is_file())
}

/// Remove the model's files, the vision encoder's included. The caller makes
/// sure nothing has them loaded.
pub fn delete() -> Result<()> {
    for dir in [default_dir()?, vision_dir()?] {
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| dir.display().to_string()),
        }
    }
    Ok(())
}
