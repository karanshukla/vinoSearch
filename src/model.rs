//! The embedding model: EmbeddingGemma 2's text model, rotary-fixed and INT8,
//! pinned by Hugging Face commit and SHA-256.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const REPO: &str = "arankays/embeddinggemma-2-text-int8-cw-onnx";
const REVISION: &str = "99f038330ae02144b23b8768506dafd511344fa4";

/// (path in the repo, sha256)
const FILES: &[(&str, &str)] = &[
    // The text-only graph: OpenVINO's C API cannot build the zero-size media
    // inputs model.onnx takes.
    (
        "onnx/model_text.onnx",
        "cab66c5d0bf25beaadca4095c7c4cb2e06b05cb360059f5854d9e4b110b70af3",
    ),
    (
        "onnx/model.onnx_data",
        "5fa7999279f92b8e97dd74dee5478b58b58c2c28bfdcfd343c7e4e4cb4103dd7",
    ),
    (
        "tokenizer.json",
        "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4",
    ),
];

pub fn default_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".local/share"),
    };
    Ok(base.join("vinosearch/models/embeddinggemma-2-text-int8-cw-onnx"))
}

pub fn onnx_path(dir: &Path) -> PathBuf {
    dir.join("onnx/model_text.onnx")
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

/// Download whatever is missing or does not match its pinned hash. Files are
/// hashed as they stream and only renamed into place once they match.
pub fn fetch(dir: &Path) -> Result<()> {
    for (name, want) in FILES {
        let dest = dir.join(name);
        if dest.is_file() && sha256_file(&dest)? == *want {
            println!("ok        {name}");
            continue;
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        let url = format!("https://huggingface.co/{REPO}/resolve/{REVISION}/{name}");
        println!("download  {name}");
        let mut resp = ureq::get(&url)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let mut body = resp.body_mut().as_reader();
        let part = dest.with_extension("part");
        let mut out = fs::File::create(&part)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
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

/// Every pinned file is present. Hashes are checked by `fetch`, not on every load.
pub fn present(dir: &Path) -> bool {
    FILES.iter().all(|(name, _)| dir.join(name).is_file())
}
