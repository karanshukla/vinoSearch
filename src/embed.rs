//! Text embeddings on the NPU.
//!
//! Every shape is static, as the NPU needs. Batch is 1: as of OpenVINO 2026.4.1
//! a static batch above 1 mixes rows across the batch on the NPU. Throughput
//! comes from keeping several requests in flight instead.

use anyhow::{Context, Result, anyhow, ensure};
use openvino::{
    CompiledModel, Core, DeviceType, ElementType, InferRequest, PartialShape, RwPropertyKey, Shape,
    Tensor,
};
use std::path::Path;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

pub const DIM: usize = 768;
const QUERY_LEN: usize = 64;
const DOC_LEN: usize = 256;
const IN_FLIGHT: usize = 4;
/// `wait` is `wait_for`, which refuses a negative (unbounded) timeout.
const WAIT_MS: i64 = 120_000;

const QUERY_PREFIX: &str = "task: search result | query: ";

pub fn document_text(title: &str, text: &str) -> String {
    format!("title: {title} | text: {text}")
}

struct Stage {
    tok: Tokenizer,
    len: usize,
    _compiled: CompiledModel,
    requests: Vec<InferRequest>,
}

pub struct Embedder {
    query: Stage,
    docs: Stage,
}

fn tokenizer(path: &Path, len: usize) -> Result<Tokenizer> {
    let mut tok = Tokenizer::from_file(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    tok.with_truncation(Some(TruncationParams {
        max_length: len,
        ..Default::default()
    }))
    .map_err(|e| anyhow!("{e}"))?;
    tok.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::Fixed(len),
        ..Default::default()
    }));
    Ok(tok)
}

impl Stage {
    fn new(
        core: &mut Core,
        model_path: &Path,
        tok_path: &Path,
        device: &str,
        len: usize,
        in_flight: usize,
    ) -> Result<Self> {
        let path = model_path.to_str().context("model path is not UTF-8")?;
        let mut model = core
            .read_model_from_file(path, "")
            .context("reading model")?;
        let tokens = PartialShape::new_static(2, &[1, len as i64])?;
        model
            .reshape(&[("input_ids", &tokens), ("attention_mask", &tokens)])
            .context("reshaping to static shapes")?;
        let mut compiled = core
            .compile_model(&model, DeviceType::from(device))
            .context("compiling")?;
        let requests = (0..in_flight)
            .map(|_| compiled.create_infer_request())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self {
            tok: tokenizer(tok_path, len)?,
            len,
            _compiled: compiled,
            requests,
        })
    }

    fn feed(tok: &Tokenizer, len: usize, req: &mut InferRequest, text: &str) -> Result<()> {
        let enc = tok.encode(text, true).map_err(|e| anyhow!("{e}"))?;
        let shape = Shape::new(&[1, len as i64])?;
        let mut ids = Tensor::new(ElementType::I64, &shape)?;
        for (d, s) in ids.get_data_mut::<i64>()?.iter_mut().zip(enc.get_ids()) {
            *d = i64::from(*s);
        }
        let mut mask = Tensor::new(ElementType::I64, &shape)?;
        for (d, s) in mask
            .get_data_mut::<i64>()?
            .iter_mut()
            .zip(enc.get_attention_mask())
        {
            *d = i64::from(*s);
        }
        req.set_tensor("input_ids", &ids)?;
        req.set_tensor("attention_mask", &mask)?;
        Ok(())
    }

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

    fn run(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for window in texts.chunks(self.requests.len()) {
            for (req, text) in self.requests.iter_mut().zip(window) {
                Self::feed(&self.tok, self.len, req, text)?;
                req.infer_async()?;
            }
            for req in self.requests.iter_mut().take(window.len()) {
                req.wait(WAIT_MS)?;
                out.push(Self::read(req)?);
            }
        }
        Ok(out)
    }
}

impl Embedder {
    pub fn new(core: &mut Core, model_dir: &Path, device: &str, cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        let cache = cache_dir.to_str().context("cache path is not UTF-8")?;
        core.set_property(&DeviceType::from(device), &RwPropertyKey::CacheDir, cache)
            .context("setting CACHE_DIR")?;
        let onnx = crate::model::onnx_path(model_dir);
        let tok = crate::model::tokenizer_path(model_dir);
        Ok(Self {
            query: Stage::new(core, &onnx, &tok, device, QUERY_LEN, 1)?,
            docs: Stage::new(core, &onnx, &tok, device, DOC_LEN, IN_FLIGHT)?,
        })
    }

    pub fn query(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.query.run(&[format!("{QUERY_PREFIX}{text}")])?;
        Ok(v.remove(0))
    }

    /// Embed documents already formatted with [`document_text`].
    pub fn documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.docs.run(texts)
    }
}
