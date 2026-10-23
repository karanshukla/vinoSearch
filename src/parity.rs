//! Whether the model runs right on a device (#76). The NPU can compile a graph
//! and answer wrongly without an error: the INT8 vision encoder gave cosine
//! 0.67 against the CPU, and a static batch over 1 mixed rows. So each stage,
//! when it compiles fresh on a device other than the CPU, embeds a fixed input
//! and is compared with the CPU's vector for it, checked in under `parity/`.

use anyhow::{Context, Result, ensure};
use std::path::Path;

/// Below this the stage runs on the CPU instead. The NPU computes in f16 and
/// gives 0.9995 to 0.99998 on the shipped stages; the failures seen were far
/// lower, so the bar leaves room for a driver's rounding to drift.
pub const MIN_COSINE: f32 = 0.995;

pub const QUERY: &str = "where did I save the restaurant's tax receipts";
pub const DOCUMENT_TITLE: &str = "notes";
pub const DOCUMENT: &str =
    "The quick brown fox jumps over the lazy dog. Invoices for October are in the shared folder.";

/// A checkerboard over two gradients: every patch differs, and nothing to decode.
pub fn image() -> image::RgbImage {
    image::RgbImage::from_fn(448, 336, |x, y| {
        let block = ((x / 56) + (y / 56)) % 2 == 0;
        image::Rgb([
            (x * 255 / 447) as u8,
            (y * 255 / 335) as u8,
            if block { 220 } else { 30 },
        ])
    })
}

/// The stages checked, by the name doctor shows, with their CPU reference.
pub const STAGES: [(&str, &[u8]); 3] = [
    ("query", include_bytes!("parity/query.f32")),
    ("document", include_bytes!("parity/document.f32")),
    ("image", include_bytes!("parity/image.f32")),
];

pub fn reference(stage: &str) -> Option<Vec<f32>> {
    let (_, bytes) = STAGES.iter().find(|(name, _)| *name == stage)?;
    decode(bytes).ok()
}

/// A reference file: the model revisions it came from, then little-endian f32s.
fn header() -> String {
    format!(
        "{}{}",
        &crate::model::REVISION[..12],
        &crate::model::VISION_REVISION[..12]
    )
}

#[cfg(test)]
pub fn encode(vector: &[f32]) -> Vec<u8> {
    let mut out = header().into_bytes();
    for x in vector {
        out.extend(x.to_le_bytes());
    }
    out
}

/// A reference for another model revision is no reference at all.
fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
    let header = header();
    let body = bytes
        .strip_prefix(header.as_bytes())
        .context("for another model revision")?;
    ensure!(
        body.len() == crate::embed::DIM * 4,
        "{} bytes of vector",
        body.len()
    );
    Ok(body
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

/// Both vectors are L2-normalised.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Kept in the stage's cache directory: the OpenVINO build it was checked
/// under, and the cosine. A new build recompiles, which checks again anyway.
const FILE: &str = "parity";

pub fn recorded(cache: &Path) -> Option<(String, f32)> {
    let text = std::fs::read_to_string(cache.join(FILE)).ok()?;
    let (build, cosine) = text.trim().rsplit_once('\t')?;
    Some((build.to_owned(), cosine.parse().ok()?))
}

pub fn record(cache: &Path, build: &str, cosine: f32) -> Result<()> {
    std::fs::write(cache.join(FILE), format!("{build}\t{cosine}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reference_is_for_this_model() {
        for (name, bytes) in STAGES {
            let v = decode(bytes).unwrap_or_else(|e| {
                panic!("parity/{name}.f32: {e:#}; regenerate with write_parity_references")
            });
            let norm = cosine(&v, &v);
            assert!((norm - 1.0).abs() < 1e-4, "{name}: norm {norm}");
        }
    }

    #[test]
    fn a_reference_for_another_revision_is_refused() {
        let mut bytes = encode(&vec![0.0; crate::embed::DIM]);
        bytes[0] ^= 1;
        assert!(decode(&bytes).is_err());
        assert!(decode(&encode(&[1.0])).is_err());
    }

    #[test]
    fn a_check_is_recorded_per_build() {
        let dir = std::env::temp_dir().join(format!("vinosearch-parity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(recorded(&dir), None);
        record(&dir, "2026.4.1-1-abc", 0.99952).unwrap();
        assert_eq!(recorded(&dir), Some(("2026.4.1-1-abc".into(), 0.99952)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
