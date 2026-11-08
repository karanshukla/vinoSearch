//! The embedding model: EmbeddingGemma 2's text model, rotary-fixed and INT8,
//! and for images its vision encoder with static shapes; and the OCR model
//! for the text in images and scans (#106). Each is pinned by Hugging Face
//! commit and SHA-256.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const HUB: &str = "https://huggingface.co";
const REPO: &str = "arankays/embeddinggemma-2-text-int8-cw-onnx";
pub(crate) const REVISION: &str = "88aed926c26344fa2ffed5f44d016fb36e2fcce3";
const VISION_REPO: &str = "arankays/embeddinggemma-2-vision-static-onnx";
pub(crate) const VISION_REVISION: &str = "16414206c27ee4bc435a7c03467155063fee1d9f";
const OCR_REPO: &str = "arankays/pp-ocrv6-small-onnx";
pub(crate) const OCR_REVISION: &str = "5bc17817a3879e7e494cd71b43d66b7b608abfac";

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

/// PP-OCRv6 small, as RapidOCR 3.10.0 publishes it, and its dict. Small, so
/// fetched whether or not images are on: scanned PDFs need it too.
const OCR_FILES: &[(&str, &str, u64)] = &[
    (
        "onnx/PP-OCRv6_det_small.onnx",
        "090f04abcd9d9a7498bc4ebf677e4cb9bdce1fe4197ddb7e529f1ef44e1ff94f",
        9_929_594,
    ),
    (
        "onnx/PP-OCRv6_rec_small.onnx",
        "6f327246b50388f3c176ae304bd95767ea6dc0c9ae92153ef8cbe210b3c14884",
        21_234_383,
    ),
    (
        "ppocrv6_dict.txt",
        "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d",
        74_947,
    ),
];

/// Bytes the model takes on disk once fetched, the vision encoder included
/// if `images`.
pub fn total_bytes(images: bool) -> u64 {
    let size = |files: &[(&str, &str, u64)]| files.iter().map(|(_, _, size)| size).sum::<u64>();
    size(FILES) + size(OCR_FILES) + if images { size(VISION_FILES) } else { 0 }
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

pub fn ocr_dir() -> Result<PathBuf> {
    Ok(models_dir()?.join("pp-ocrv6-small-onnx"))
}

pub fn ocr_det_path(dir: &Path) -> PathBuf {
    dir.join(OCR_FILES[0].0)
}

pub fn ocr_rec_path(dir: &Path) -> PathBuf {
    dir.join(OCR_FILES[1].0)
}

pub fn ocr_dict_path(dir: &Path) -> PathBuf {
    dir.join(OCR_FILES[2].0)
}

/// Every file reading text from pixels needs is in `dir`.
pub fn ocr_present(dir: &Path) -> bool {
    OCR_FILES
        .iter()
        .all(|(name, _, _)| dir.join(name).is_file())
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

/// Where Hugging Face serves `repo`'s files at `revision`.
fn hub(repo: &str, revision: &str) -> String {
    format!("{HUB}/{repo}/resolve/{revision}")
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
        &hub(REPO, REVISION),
        FILES,
        &default_dir()?,
        &mut done,
        total,
        progress,
    )?;
    fetch_files(
        &hub(OCR_REPO, OCR_REVISION),
        OCR_FILES,
        &ocr_dir()?,
        &mut done,
        total,
        progress,
    )?;
    if images {
        fetch_files(
            &hub(VISION_REPO, VISION_REVISION),
            VISION_FILES,
            &vision_dir()?,
            &mut done,
            total,
            progress,
        )?;
    }
    Ok(())
}

/// `files` from `source`, the URL their paths are under.
fn fetch_files(
    source: &str,
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
        let url = format!("{source}/{name}");
        let part = dest.with_extension("part");
        let at = *done;
        let got = download(&url, &part, name, want, *size, &mut |got| {
            progress(name, at + got, total);
        });
        *done += size;
        if let Err(e) = got {
            // However it stopped (a dropped connection, a full disk), a
            // half-written file of up to the file's size is not left behind.
            let _ = fs::remove_file(&part);
            return Err(e);
        }
        fs::rename(&part, &dest)?;
    }
    Ok(())
}

/// `url` into `part`, hashed as it streams; Err if it runs past `size`
/// bytes or doesn't hash to `want`. `progress` gets the bytes so far.
fn download(
    url: &str,
    part: &Path,
    name: &str,
    want: &str,
    size: u64,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let mut resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut body = resp.body_mut().as_reader();
    let mut out = fs::File::create(part)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut got_bytes = 0;
    loop {
        let n = body.read(&mut buf).with_context(|| format!("GET {url}"))?;
        if n == 0 {
            break;
        }
        got_bytes += n as u64;
        if got_bytes > size {
            bail!("{name}: more than the pinned {size} bytes");
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        progress(got_bytes);
    }
    out.sync_all()?;
    let got = hex::encode(hasher.finalize());
    if got != want {
        bail!("{name}: sha256 {got}, expected {want}");
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
pub fn vision_present(dir: &Path, vision_dir: &Path) -> bool {
    image_text_path(dir).is_file()
        && VISION_FILES
            .iter()
            .all(|(name, _, _)| vision_dir.join(name).is_file())
}

/// What indexing an image needs, besides the text model, is present: the
/// vision encoder, and the OCR model for its text.
pub fn images_present(dir: &Path, vision_dir: &Path, ocr_dir: &Path) -> bool {
    vision_present(dir, vision_dir) && ocr_present(ocr_dir)
}

/// Remove the model's files, the vision encoder's and OCR's included. The
/// caller makes sure nothing has them loaded.
pub fn delete() -> Result<()> {
    for dir in [default_dir()?, vision_dir()?, ocr_dir()?] {
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| dir.display().to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// What the test server sends for a path: a Content-Length, and a body
    /// that may be shorter (a dropped connection) or longer.
    struct Reply {
        length: usize,
        body: Vec<u8>,
    }

    impl Reply {
        fn whole(body: &[u8]) -> Self {
            Reply {
                length: body.len(),
                body: body.to_vec(),
            }
        }
    }

    /// An HTTP server on localhost serving `replies` by path, 404 for any
    /// other. Returns its URL and the paths asked for.
    fn serve(replies: Vec<(&'static str, Reply)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap() <= 2 {
                        break;
                    }
                }
                log.lock().unwrap().push(path.clone());
                let _ = match replies.iter().find(|(p, _)| *p == path) {
                    Some((_, r)) => write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        r.length
                    )
                    .and_then(|()| stream.write_all(&r.body)),
                    None => write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                };
            }
        });
        (url, asked)
    }

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vinosearch-model-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// `fetch_files` for one file pinned to `want` and `size`, from `url`.
    fn fetch_one(url: &str, dir: &Path, want: &str, size: u64) -> Result<u64> {
        let mut done = 0;
        let mut last = 0;
        fetch_files(
            url,
            &[("onnx/m.onnx", want, size)],
            dir,
            &mut done,
            size,
            &mut |_, d, _| last = d,
        )?;
        assert_eq!(last, done);
        Ok(done)
    }

    /// Every file under `dir`, relative to it.
    fn files(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = walkdir::WalkDir::new(dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().strip_prefix(dir).unwrap().display().to_string())
            .collect();
        out.sort();
        out
    }

    const BODY: &[u8] = b"weights weights weights";

    #[test]
    fn a_good_file_lands_and_is_not_fetched_again() {
        let (url, asked) = serve(vec![("/onnx/m.onnx", Reply::whole(BODY))]);
        let dir = temp("good");
        let size = BODY.len() as u64;
        assert_eq!(fetch_one(&url, &dir, &sha(BODY), size).unwrap(), size);
        assert_eq!(fs::read(dir.join("onnx/m.onnx")).unwrap(), BODY);
        assert_eq!(files(&dir), ["onnx/m.onnx"]);
        // In place and matching: counted, not asked for.
        assert_eq!(fetch_one(&url, &dir, &sha(BODY), size).unwrap(), size);
        assert_eq!(*asked.lock().unwrap(), ["/onnx/m.onnx"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_file_in_place_is_replaced() {
        let (url, asked) = serve(vec![("/onnx/m.onnx", Reply::whole(BODY))]);
        let dir = temp("corrupt");
        fs::create_dir_all(dir.join("onnx")).unwrap();
        fs::write(dir.join("onnx/m.onnx"), b"bit rot").unwrap();
        // A stale part from an earlier try is overwritten, not appended to.
        fs::write(dir.join("onnx/m.part"), b"stale stale stale stale stale").unwrap();
        fetch_one(&url, &dir, &sha(BODY), BODY.len() as u64).unwrap();
        assert_eq!(fs::read(dir.join("onnx/m.onnx")).unwrap(), BODY);
        assert_eq!(files(&dir), ["onnx/m.onnx"]);
        assert_eq!(asked.lock().unwrap().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wrong_hash_leaves_nothing() {
        let (url, _) = serve(vec![("/onnx/m.onnx", Reply::whole(BODY))]);
        let dir = temp("hash");
        let err = fetch_one(&url, &dir, &sha(b"other"), BODY.len() as u64).unwrap_err();
        assert!(err.to_string().contains("sha256"), "{err:#}");
        assert!(files(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_over_its_pinned_size_is_abandoned() {
        let (url, _) = serve(vec![("/onnx/m.onnx", Reply::whole(BODY))]);
        let dir = temp("size");
        let err = fetch_one(&url, &dir, &sha(BODY), 4).unwrap_err();
        assert!(
            err.to_string().contains("more than the pinned 4"),
            "{err:#}"
        );
        assert!(files(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dropped_download_leaves_no_part_file() {
        let (url, _) = serve(vec![(
            "/onnx/m.onnx",
            Reply {
                length: 1000,
                body: BODY.to_vec(),
            },
        )]);
        let dir = temp("dropped");
        let err = fetch_one(&url, &dir, &sha(BODY), 1000).unwrap_err();
        assert!(err.to_string().starts_with("GET "), "{err:#}");
        assert!(files(&dir).is_empty(), "{:?}", files(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let (url, _) = serve(vec![]);
        let dir = temp("missing");
        let err = fetch_one(&url, &dir, &sha(BODY), 10).unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
        assert!(files(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pinned_files_have_hashes_and_sizes() {
        for (name, sha, size) in FILES.iter().chain(VISION_FILES).chain(OCR_FILES) {
            assert_eq!(sha.len(), 64, "{name}");
            assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()), "{name}");
            assert!(*size > 0, "{name}");
        }
        assert_eq!(
            hub(REPO, REVISION),
            format!("https://huggingface.co/{REPO}/resolve/{REVISION}")
        );
        assert!(total_bytes(true) > total_bytes(false));
    }
}
