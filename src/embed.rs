//! Text and image embeddings on the NPU.
//!
//! Every shape is static, as the NPU needs. Batch is 1: as of OpenVINO 2026.4.1
//! a static batch above 1 mixes rows across the batch on the NPU. Throughput
//! comes from keeping several requests in flight instead.

use anyhow::{Context, Result, anyhow, ensure};
use openvino::{
    CompiledModel, Core, DeviceType, ElementType, InferRequest, PartialShape, RwPropertyKey, Shape,
    Tensor,
};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokenizers::{PostProcessor, Tokenizer, TruncationDirection};

pub const DIM: usize = 768;
/// Static on the NPU, so every query pays for the full length: 64 took ~20 ms
/// a query, 32 takes ~12. 32 holds the task prefix plus ~22 tokens (about 15
/// words), longer than a launcher query. A longer one goes through the
/// document shape instead (~35-50 ms; identical embeddings, cosine 0.999998).
const QUERY_LEN: usize = 32;
const DOC_LEN: usize = 256;
/// Document requests queued on the device at once. 1 indexes at 27.3
/// chunks/s against 4's 28.0, with the NPU ~87% busy either way, and leaves
/// another program's request (a face unlock, dictation) behind at most one
/// document instead of four.
const IN_FLIGHT: usize = 1;
/// `wait` is `wait_for`, which refuses a negative (unbounded) timeout.
const WAIT_MS: i64 = 120_000;

const QUERY_PREFIX: &str = "task: search result | query: ";
/// An image's prompt, `<bos> <|image> <|image|> x tokens <image|> <eos>`,
/// padded to this: SOFT_TOKENS + 4 rounded up to 16, model_image_140's shape.
const IMAGE_LEN: usize = 144;

pub fn document_text(title: &str, text: &str) -> String {
    format!("title: {title} | text: {text}")
}

struct Stage {
    len: usize,
    _compiled: CompiledModel,
    requests: Vec<InferRequest>,
    /// Compiled now rather than loaded from the cache.
    fresh: bool,
    cache: PathBuf,
}

/// The vision encoder and the text model taking its output.
struct ImageStage {
    _vision: CompiledModel,
    vision: InferRequest,
    _text: CompiledModel,
    text: InferRequest,
    /// `<bos>`, `<|image>`, `<|image|>`, `<image|>`, `<eos>`.
    ids: [i64; 5],
    fresh: bool,
    cache: PathBuf,
}

/// Query stage up front; the document and image stages compile on first
/// use, so a process that only answers queries never holds a second copy of
/// the model.
pub struct Embedder {
    core: Core,
    model_dir: PathBuf,
    vision_dir: PathBuf,
    ocr_dir: PathBuf,
    device: String,
    cache: PathBuf,
    tok: Tokenizer,
    query: Stage,
    docs: Option<Stage>,
    images: Option<ImageStage>,
    ocr: Option<crate::ocr::Ocr>,
    docs_share: f64,
}

/// Whether `text` fits in `len` tokens with the special tokens added.
fn fits(tok: &Tokenizer, text: &str, len: usize) -> Result<bool> {
    let n = tok.encode(text, false).map_err(|e| anyhow!("{e}"))?.len();
    let added = tok
        .get_post_processor()
        .map_or(0, |p| p.added_tokens(false));
    Ok(n + added <= len)
}

/// Token ids and attention mask, exactly `len` long: truncated before the
/// special tokens are added so they survive (as `TokenizerImpl::post_process`
/// does with truncation set), then padded with mask 0.
fn encode_fixed(tok: &Tokenizer, text: &str, len: usize) -> Result<(Vec<i64>, Vec<i64>)> {
    let mut enc = tok.encode(text, false).map_err(|e| anyhow!("{e}"))?;
    let added = tok
        .get_post_processor()
        .map_or(0, |p| p.added_tokens(false));
    ensure!(
        len > added,
        "length {len} leaves no room past {added} special tokens"
    );
    enc.truncate(len - added, 0, TruncationDirection::Right);
    let enc = tok
        .post_process(enc, None, true)
        .map_err(|e| anyhow!("{e}"))?;
    let pad = tok.get_padding().map_or(0, |p| p.pad_id);
    let mut ids: Vec<i64> = enc.get_ids().iter().map(|&i| i64::from(i)).collect();
    let mut mask: Vec<i64> = enc
        .get_attention_mask()
        .iter()
        .map(|&m| i64::from(m))
        .collect();
    ids.resize(len, i64::from(pad));
    mask.resize(len, 0);
    Ok((ids, mask))
}

/// Compiled blobs for one stage live in their own directory, named for what
/// decides their contents, so blobs for an old model revision or shape are
/// recognisably stale. OpenVINO names blobs by an opaque hash, and nothing
/// evicts them: one day of changing shapes left 1.4 GB, five ~280 MB blobs,
/// for two in use.
fn cache_key(device: &str, len: usize) -> String {
    stage_key(crate::model::REVISION, device, &len.to_string())
}

pub(crate) fn stage_key(revision: &str, device: &str, stage: &str) -> String {
    let dev: String = device
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{}-{dev}-{stage}", &revision[..12])
}

/// The stages this build compiles, as (model revision, stage).
fn stages() -> Vec<(&'static str, String)> {
    let tokens = crate::vision::SOFT_TOKENS;
    let mut out = vec![
        (crate::model::REVISION, QUERY_LEN.to_string()),
        (crate::model::REVISION, DOC_LEN.to_string()),
        (crate::model::REVISION, format!("image{tokens}")),
        (crate::model::VISION_REVISION, format!("vision{tokens}")),
    ];
    out.extend(
        crate::ocr::stages()
            .into_iter()
            .map(|s| (crate::model::OCR_REVISION, s)),
    );
    out
}

/// Drop everything under `root` but this build's stage directories (for
/// any device, so a one-off run on another device does not cost the NPU a
/// recompile).
fn prune_cache(root: &Path) -> Result<()> {
    let stages = stages();
    for e in std::fs::read_dir(root)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let current = e.path().is_dir()
            && stages.iter().any(|(rev, stage)| {
                name.strip_prefix(&rev[..12]).is_some_and(|rest| {
                    rest.starts_with('-') && rest.ends_with(&format!("-{stage}"))
                })
            });
        if !current {
            let removed = if e.path().is_dir() {
                std::fs::remove_dir_all(e.path())
            } else {
                std::fs::remove_file(e.path())
            };
            if let Err(err) = removed {
                eprintln!("pruning {}: {err}", e.path().display());
            }
        }
    }
    Ok(())
}

fn blobs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "blob"))
        .collect()
}

/// Compile with this stage's cache directory, then drop the blobs a cache
/// miss left stale. True when it was a miss.
pub(crate) fn compile_cached(
    core: &mut Core,
    device: &str,
    cache: &Path,
    compile: impl FnOnce(&mut Core) -> Result<CompiledModel>,
) -> Result<(CompiledModel, bool)> {
    std::fs::create_dir_all(cache)?;
    core.set_property(
        &DeviceType::from(device),
        &RwPropertyKey::CacheDir,
        cache.to_str().context("cache path is not UTF-8")?,
    )
    .context("setting CACHE_DIR")?;
    let before = blobs(cache);
    let compiled = compile(core)?;
    // A new blob means a cache miss (a new OpenVINO, say), so the older
    // ones in this directory are for a build that no longer loads them.
    let fresh = blobs(cache).iter().any(|b| !before.contains(b));
    if fresh {
        for old in before {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok((compiled, fresh))
}

/// Straight from the path: on a cache hit OpenVINO loads the blob without
/// reading the model, 340 MB peak instead of 630 MB for the text model.
fn compile_file(core: &mut Core, path: &Path, device: &str) -> Result<CompiledModel> {
    core.compile_model_from_file(
        path.to_str().context("model path is not UTF-8")?,
        DeviceType::from(device),
    )
    .with_context(|| format!("compiling {}", path.display()))
}

impl Stage {
    fn new(
        core: &mut Core,
        model_dir: &Path,
        device: &str,
        cache_root: &Path,
        len: usize,
        in_flight: usize,
    ) -> Result<Self> {
        let cache = cache_root.join(cache_key(device, len));
        let (mut compiled, fresh) =
            compile_cached(
                core,
                device,
                &cache,
                |core| match crate::model::static_path(model_dir, len).filter(|p| p.is_file()) {
                    Some(path) => compile_file(core, &path, device),
                    // A model fetched before the static graphs were published.
                    None => {
                        let path = crate::model::onnx_path(model_dir);
                        let mut model = core
                            .read_model_from_file(
                                path.to_str().context("model path is not UTF-8")?,
                                "",
                            )
                            .context("reading model")?;
                        let tokens = PartialShape::new_static(2, &[1, len as i64])?;
                        model
                            .reshape(&[("input_ids", &tokens), ("attention_mask", &tokens)])
                            .context("reshaping to static shapes")?;
                        Ok(core
                            .compile_model(&model, DeviceType::from(device))
                            .context("compiling")?)
                    }
                },
            )?;
        let requests = (0..in_flight)
            .map(|_| compiled.create_infer_request())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self {
            len,
            _compiled: compiled,
            requests,
            fresh,
            cache,
        })
    }

    fn feed(tok: &Tokenizer, len: usize, req: &mut InferRequest, text: &str) -> Result<()> {
        let (ids, mask) = encode_fixed(tok, text, len)?;
        let shape = Shape::new(&[1, len as i64])?;
        let mut t_ids = Tensor::new(ElementType::I64, &shape)?;
        t_ids.get_data_mut::<i64>()?.copy_from_slice(&ids);
        let mut t_mask = Tensor::new(ElementType::I64, &shape)?;
        t_mask.get_data_mut::<i64>()?.copy_from_slice(&mask);
        req.set_tensor("input_ids", &t_ids)?;
        req.set_tensor("attention_mask", &t_mask)?;
        Ok(())
    }

    /// One request's `sentence_embedding`, L2-normalised.
    fn read(req: &InferRequest) -> Result<Vec<f32>> {
        let out = req.get_tensor("sentence_embedding")?;
        let v = out.get_data::<f32>()?;
        ensure!(
            v.len() == DIM,
            "sentence_embedding has {} values, expected {DIM}",
            v.len()
        );
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        ensure!(
            norm.is_finite() && norm > 0.0,
            "degenerate embedding (norm {norm})"
        );
        Ok(v.iter().map(|x| x / norm).collect())
    }

    /// `share` of the device's time, in (0, 1]: after each request, wait
    /// `busy * (1 - share) / share`, leaving the device to other programs.
    fn run(&mut self, tok: &Tokenizer, texts: &[String], share: f64) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for window in texts.chunks(self.requests.len()) {
            let t0 = Instant::now();
            for (req, text) in self.requests.iter_mut().zip(window) {
                Self::feed(tok, self.len, req, text)?;
                req.infer_async()?;
            }
            for req in self.requests.iter_mut().take(window.len()) {
                req.wait(WAIT_MS)?;
                out.push(Self::read(req)?);
            }
            yield_device(t0, share);
        }
        Ok(out)
    }
}

/// After work that started at `t0`, leave the device to other programs for
/// long enough that this work takes `share` of its time.
pub(crate) fn yield_device(t0: Instant, share: f64) {
    if share < 1.0 {
        std::thread::sleep(t0.elapsed().mul_f64((1.0 - share) / share));
    }
}

fn i64_tensor(dims: &[i64], data: &[i64]) -> Result<Tensor> {
    let mut t = Tensor::new(ElementType::I64, &Shape::new(dims)?)?;
    t.get_data_mut::<i64>()?.copy_from_slice(data);
    Ok(t)
}

impl ImageStage {
    fn new(
        core: &mut Core,
        model_dir: &Path,
        vision_dir: &Path,
        device: &str,
        cache_root: &Path,
        tok: &Tokenizer,
    ) -> Result<Self> {
        ensure!(
            crate::model::vision_present(model_dir, vision_dir),
            "the vision encoder is not in {}; run: vinosearch fetch-model",
            vision_dir.display()
        );
        let tokens = crate::vision::SOFT_TOKENS;
        let cache = cache_root.join(stage_key(
            crate::model::VISION_REVISION,
            device,
            &format!("vision{tokens}"),
        ));
        let path = crate::model::vision_path(vision_dir);
        let (mut vision, vision_fresh) = compile_cached(core, device, &cache, |core| {
            compile_file(core, &path, device)
        })?;
        let cache = cache_root.join(stage_key(
            crate::model::REVISION,
            device,
            &format!("image{tokens}"),
        ));
        let path = crate::model::image_text_path(model_dir);
        let (mut text, text_fresh) = compile_cached(core, device, &cache, |core| {
            compile_file(core, &path, device)
        })?;
        let id = |t: &str| {
            tok.token_to_id(t)
                .map(i64::from)
                .with_context(|| format!("{t} is not in the tokenizer"))
        };
        Ok(Self {
            vision: vision.create_infer_request()?,
            _vision: vision,
            text: text.create_infer_request()?,
            _text: text,
            ids: [
                id("<bos>")?,
                id("<|image>")?,
                id("<|image|>")?,
                id("<image|>")?,
                id("<eos>")?,
            ],
            fresh: vision_fresh || text_fresh,
            cache,
        })
    }

    fn run(&mut self, px: &crate::vision::Pixels) -> Result<Vec<f32>> {
        use crate::vision::{PATCH_DIM, PATCHES, SOFT_TOKENS};
        ensure!(
            (1..=SOFT_TOKENS).contains(&px.tokens),
            "{} soft tokens, expected 1 to {SOFT_TOKENS}",
            px.tokens
        );
        let mut values = Tensor::new(
            ElementType::F32,
            &Shape::new(&[1, PATCHES as i64, PATCH_DIM as i64])?,
        )?;
        values.get_data_mut::<f32>()?.copy_from_slice(&px.values);
        self.vision.set_tensor("pixel_values", &values)?;
        self.vision.set_tensor(
            "pixel_position_ids",
            &i64_tensor(&[1, PATCHES as i64, 2], &px.positions)?,
        )?;
        self.vision.infer()?;
        // Every soft token's row; the text model merges the first `tokens`
        // (one per image token in the prompt) and ignores the rest.
        let features = self.vision.get_tensor("image_features")?;
        let [bos, boi, image, eoi, eos] = self.ids;
        let mut ids = vec![bos, boi];
        ids.extend(std::iter::repeat_n(image, px.tokens));
        ids.extend([eoi, eos]);
        let mut mask = vec![1; ids.len()];
        // Padded at the end with id 0 and mask 0. More image tokens instead
        // would not be neutral: they are attended even when masked.
        ids.resize(IMAGE_LEN, 0);
        mask.resize(IMAGE_LEN, 0);
        let dims = [1, IMAGE_LEN as i64];
        self.text
            .set_tensor("input_ids", &i64_tensor(&dims, &ids)?)?;
        self.text
            .set_tensor("attention_mask", &i64_tensor(&dims, &mask)?)?;
        self.text.set_tensor("image_features", &features)?;
        self.text.infer()?;
        Stage::read(&self.text)
    }
}

/// A compiled stage, as `checked` sees it.
trait Compiled {
    fn fresh(&self) -> bool;
    fn cache(&self) -> &Path;
}

impl Compiled for Stage {
    fn fresh(&self) -> bool {
        self.fresh
    }
    fn cache(&self) -> &Path {
        &self.cache
    }
}

impl Compiled for ImageStage {
    fn fresh(&self) -> bool {
        self.fresh
    }
    fn cache(&self) -> &Path {
        &self.cache
    }
}

/// `build` the stage on `device`, then hold it to the CPU's answer for the same input
/// (#76): checked when compiled fresh or never checked under this OpenVINO, otherwise
/// the recorded cosine stands. Below `parity::MIN_COSINE`, it's built on the CPU instead.
fn checked<S: Compiled>(
    name: &str,
    device: &str,
    mut build: impl FnMut(&str) -> Result<S>,
    probe: impl FnOnce(&mut S) -> Result<Vec<f32>>,
) -> Result<S> {
    let mut stage = build(device)?;
    let Some(reference) = crate::parity::reference(name).filter(|_| device != "CPU") else {
        return Ok(stage);
    };
    let build_id = openvino::version().build_number;
    let cosine = match crate::parity::recorded(stage.cache()) {
        Some((checked, cosine)) if checked == build_id && !stage.fresh() => cosine,
        _ => {
            let cosine = crate::parity::cosine(&probe(&mut stage)?, &reference);
            crate::parity::record(stage.cache(), &build_id, cosine)?;
            eprintln!("{name} stage on {device}: cosine {cosine:.5} to the CPU reference");
            cosine
        }
    };
    if cosine >= crate::parity::MIN_COSINE {
        return Ok(stage);
    }
    eprintln!(
        "{name} stage on {device} is off the CPU reference (cosine {cosine:.4}); running it on CPU"
    );
    drop(stage);
    build("CPU")
}

/// What each stage's last check on `device` found, as doctor reports it: None for not
/// checked yet.
pub fn parity(cache_root: &Path, device: &str) -> Vec<(&'static str, Option<f32>)> {
    let tokens = crate::vision::SOFT_TOKENS;
    let dirs = [
        cache_key(device, QUERY_LEN),
        cache_key(device, DOC_LEN),
        stage_key(crate::model::REVISION, device, &format!("image{tokens}")),
    ];
    crate::parity::STAGES
        .iter()
        .zip(dirs)
        .map(|((name, _), dir)| {
            let cosine = crate::parity::recorded(&cache_root.join(dir)).map(|(_, c)| c);
            (*name, cosine)
        })
        .collect()
}

fn query_probe(stage: &mut Stage, tok: &Tokenizer) -> Result<Vec<f32>> {
    let text = format!("{QUERY_PREFIX}{}", crate::parity::QUERY);
    Ok(stage.run(tok, &[text], 1.0)?.remove(0))
}

impl Embedder {
    pub fn new(
        mut core: Core,
        model_dir: &Path,
        vision_dir: &Path,
        ocr_dir: &Path,
        device: &str,
        cache_dir: &Path,
    ) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        prune_cache(cache_dir)?;
        let tok_path = crate::model::tokenizer_path(model_dir);
        let tok =
            Tokenizer::from_file(&tok_path).map_err(|e| anyhow!("{}: {e}", tok_path.display()))?;
        let query = checked(
            "query",
            device,
            |device| Stage::new(&mut core, model_dir, device, cache_dir, QUERY_LEN, 1),
            |stage| query_probe(stage, &tok),
        )?;
        Ok(Self {
            core,
            model_dir: model_dir.to_owned(),
            vision_dir: vision_dir.to_owned(),
            ocr_dir: ocr_dir.to_owned(),
            device: device.to_owned(),
            cache: cache_dir.to_owned(),
            tok,
            query,
            docs: None,
            images: None,
            ocr: None,
            docs_share: 1.0,
        })
    }

    pub fn query(&mut self, text: &str) -> Result<Vec<f32>> {
        let text = format!("{QUERY_PREFIX}{text}");
        let mut v = if fits(&self.tok, &text, QUERY_LEN)? {
            self.query.run(&self.tok, &[text], 1.0)?
        } else {
            // Too long for the query shape: the document shape takes 256
            // tokens, and is compiled already whenever the runner has indexed.
            self.prepare_documents()?;
            self.docs.as_mut().unwrap().run(&self.tok, &[text], 1.0)?
        };
        Ok(v.remove(0))
    }

    /// Compile the document stage now, if it is not already.
    pub fn prepare_documents(&mut self) -> Result<()> {
        if self.docs.is_none() {
            let (core, tok) = (&mut self.core, &self.tok);
            self.docs = Some(checked(
                "document",
                &self.device,
                |device| {
                    Stage::new(
                        core,
                        &self.model_dir,
                        device,
                        &self.cache,
                        DOC_LEN,
                        IN_FLIGHT,
                    )
                },
                |stage| {
                    let text =
                        document_text(crate::parity::DOCUMENT_TITLE, crate::parity::DOCUMENT);
                    Ok(stage.run(tok, &[text], 1.0)?.remove(0))
                },
            )?);
        }
        Ok(())
    }

    /// Cap document and image embedding at `share` of the device's time, in
    /// (0, 1]. Queries always run at full speed.
    pub fn set_document_share(&mut self, share: f64) {
        self.docs_share = share.clamp(0.01, 1.0);
    }

    /// Free the document and image stages; the next document (or long
    /// query) or image compiles them again, from the cache.
    pub fn release_documents(&mut self) -> bool {
        let images = self.images.take().is_some();
        let ocr = self.ocr.take().is_some();
        self.docs.take().is_some() || images || ocr
    }

    /// The document or image stage is compiled.
    pub fn has_documents(&self) -> bool {
        self.docs.is_some() || self.images.is_some() || self.ocr.is_some()
    }

    /// The OCR model's files are fetched.
    pub fn can_read_text(&self) -> bool {
        crate::model::ocr_present(&self.ocr_dir)
    }

    /// The lines of text in `img` (#106), at the document share of the device.
    pub fn read_text(&mut self, img: &image::RgbImage) -> Result<Vec<String>> {
        if self.ocr.is_none() {
            self.ocr = Some(crate::ocr::Ocr::new(
                &mut self.core,
                &self.ocr_dir,
                &self.device,
                &self.cache,
            )?);
        }
        self.ocr.as_mut().unwrap().read(img, self.docs_share)
    }

    /// Embed an image, prepared by [`crate::vision::prepare`].
    pub fn image(&mut self, px: &crate::vision::Pixels) -> Result<Vec<f32>> {
        if self.images.is_none() {
            let (core, tok) = (&mut self.core, &self.tok);
            self.images = Some(checked(
                "image",
                &self.device,
                |device| {
                    ImageStage::new(
                        core,
                        &self.model_dir,
                        &self.vision_dir,
                        device,
                        &self.cache,
                        tok,
                    )
                },
                |stage| stage.run(&crate::vision::prepare(&crate::parity::image())?),
            )?);
        }
        let t0 = Instant::now();
        let v = self.images.as_mut().unwrap().run(px)?;
        yield_device(t0, self.docs_share);
        Ok(v)
    }

    /// Embed documents already formatted with [`document_text`].
    pub fn documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.prepare_documents()?;
        let share = self.docs_share;
        self.docs.as_mut().unwrap().run(&self.tok, texts, share)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `src/parity/*.f32` from the CPU. Run after a model revision changes:
    /// `cargo test --release write_parity_references -- --ignored`.
    #[test]
    #[ignore = "needs the OpenVINO runtime and the model"]
    fn write_parity_references() {
        let prefix = crate::runtime::default_prefix(None).unwrap();
        crate::runtime::load(&prefix).unwrap();
        // Not the temp dir, which is RAM here; the CPU blobs are hundreds of MB.
        let home = std::env::var_os("HOME").unwrap();
        let cache = Path::new(&home).join(".cache/vinosearch/parity-references");
        let mut e = Embedder::new(
            Core::new().unwrap(),
            &crate::model::default_dir().unwrap(),
            &crate::model::vision_dir().unwrap(),
            &crate::model::ocr_dir().unwrap(),
            "CPU",
            &cache,
        )
        .unwrap();
        let vectors = [
            e.query(crate::parity::QUERY).unwrap(),
            e.documents(&[document_text(
                crate::parity::DOCUMENT_TITLE,
                crate::parity::DOCUMENT,
            )])
            .unwrap()
            .remove(0),
            e.image(&crate::vision::prepare(&crate::parity::image()).unwrap())
                .unwrap(),
        ];
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/parity");
        for ((name, _), v) in crate::parity::STAGES.iter().zip(vectors) {
            std::fs::write(dir.join(format!("{name}.f32")), crate::parity::encode(&v)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn prune_keeps_only_this_revisions_stage_directories() {
        let root = std::env::temp_dir().join(format!("vinosearch-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let keep = [
            cache_key("NPU", QUERY_LEN),
            cache_key("NPU", DOC_LEN),
            cache_key("CPU", DOC_LEN),
            stage_key(crate::model::REVISION, "NPU", "image140"),
            stage_key(crate::model::VISION_REVISION, "NPU", "vision140"),
        ];
        let drop = [
            cache_key("NPU", 64),
            format!("0123456789ab-NPU-{QUERY_LEN}"),
            stage_key(crate::model::VISION_REVISION, "NPU", "vision280"),
        ];
        for d in keep.iter().chain(&drop) {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("123.blob"), b"legacy").unwrap();
        prune_cache(&root).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut want = keep.to_vec();
        want.sort();
        assert_eq!(left, want);
        std::fs::remove_dir_all(&root).unwrap();
    }
    use tokenizers::models::wordlevel::WordLevel;
    use tokenizers::pre_tokenizers::whitespace::Whitespace;
    use tokenizers::processors::template::TemplateProcessing;

    fn tiny() -> Tokenizer {
        let vocab = [
            ("<pad>", 0),
            ("<bos>", 2),
            ("<eos>", 1),
            ("a", 3),
            ("b", 4),
            ("<unk>", 5),
        ]
        .into_iter()
        .map(|(t, i)| (t.to_string(), i))
        .collect();
        let model = WordLevel::builder()
            .vocab(vocab)
            .unk_token("<unk>".into())
            .build()
            .unwrap();
        let mut tok = Tokenizer::new(model);
        tok.with_pre_tokenizer(Some(Whitespace {}));
        tok.with_post_processor(Some(
            TemplateProcessing::builder()
                .try_single("<bos> $A <eos>")
                .unwrap()
                .special_tokens(vec![("<bos>", 2), ("<eos>", 1)])
                .build()
                .unwrap(),
        ));
        tok
    }

    #[test]
    fn truncation_keeps_the_special_tokens() {
        let (ids, mask) = encode_fixed(&tiny(), "a b a b a b a b", 5).unwrap();
        assert_eq!(ids, [2, 3, 4, 3, 1]);
        assert_eq!(mask, [1; 5]);
    }

    #[test]
    fn fit_counts_the_special_tokens() {
        // "a b a" is 3 tokens, 5 with <bos> and <eos>.
        assert!(fits(&tiny(), "a b a", 5).unwrap());
        assert!(!fits(&tiny(), "a b a", 4).unwrap());
    }

    #[test]
    fn short_text_pads_with_mask_zero() {
        let (ids, mask) = encode_fixed(&tiny(), "a b", 6).unwrap();
        assert_eq!(ids, [2, 3, 4, 1, 0, 0]);
        assert_eq!(mask, [1, 1, 1, 1, 0, 0]);
    }
}
