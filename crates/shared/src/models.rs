//! Where the downloaded models live, so the settings app can show and delete
//! them while the runner isn't there to ask.

use anyhow::{Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The text model, the vision encoder, the OCR model and the audio encoder:
/// one directory each under `dir()`.
pub const TEXT: &str = "embeddinggemma-2-text-int8-cw-onnx";
pub const VISION: &str = "embeddinggemma-2-vision-static-onnx";
pub const OCR: &str = "pp-ocrv6-small-onnx";
pub const AUDIO: &str = "embeddinggemma-2-audio-static-onnx";
pub const ALL: [&str; 4] = [TEXT, VISION, OCR, AUDIO];

/// `$XDG_DATA_HOME/vinosearch/models`.
pub fn dir() -> Result<PathBuf> {
    Ok(crate::xdg::data_home()?.join("vinosearch/models"))
}

/// Bytes of files in the models' directories under `base`. Only those: a
/// file of the user's own left beside them isn't counted, as `delete` keeps it.
pub fn bytes(base: &Path) -> u64 {
    ALL.iter().map(|name| tree_bytes(&base.join(name))).sum()
}

/// Links aren't followed: one out of the tree isn't ours to count (pinned by
/// `tests::a_link_out_of_the_tree_is_not_counted`).
fn tree_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => tree_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Remove the models' directories under `base`, and `base` itself if that
/// leaves it empty. The caller makes sure nothing has them loaded (pinned by
/// `tests::delete_removes_the_models_and_keeps_anything_else`).
pub fn delete(base: &Path) -> Result<()> {
    for name in ALL {
        let dir = base.join(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| dir.display().to_string()),
        }
    }
    let _ = fs::remove_dir(base);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_removes_the_models_and_keeps_anything_else() {
        let base = std::env::temp_dir().join(format!("vs-models-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for (name, len) in [(TEXT, 10), (VISION, 20), (OCR, 30), (AUDIO, 40)] {
            fs::create_dir_all(base.join(name).join("onnx")).unwrap();
            fs::write(base.join(name).join("onnx/m.onnx"), vec![0u8; len]).unwrap();
        }
        fs::write(base.join("notes.txt"), b"mine").unwrap();
        assert_eq!(bytes(&base), 100);
        delete(&base).unwrap();
        assert_eq!(bytes(&base), 0);
        assert!(ALL.iter().all(|n| !base.join(n).exists()));
        assert!(base.join("notes.txt").is_file());
        fs::remove_file(base.join("notes.txt")).unwrap();
        delete(&base).unwrap();
        assert!(!base.exists(), "nothing left, so the directory goes too");
        delete(&base).expect("a second delete is fine");
    }

    #[test]
    fn a_link_out_of_the_tree_is_not_counted() {
        let base = std::env::temp_dir().join(format!("vs-models-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(TEXT)).unwrap();
        fs::write(base.join(TEXT).join("m.onnx"), vec![0u8; 5]).unwrap();
        fs::write(base.join("outside.bin"), vec![0u8; 50]).unwrap();
        std::os::unix::fs::symlink(base.join("outside.bin"), base.join(TEXT).join("link")).unwrap();
        assert_eq!(bytes(&base), 5);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_partial_set_counts() {
        let base = std::env::temp_dir().join(format!("vs-models-part-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(OCR)).unwrap();
        fs::write(base.join(OCR).join("det.onnx"), vec![0u8; 7]).unwrap();
        assert_eq!(bytes(&base), 7);
        delete(&base).unwrap();
        assert!(!base.exists());
    }
}
