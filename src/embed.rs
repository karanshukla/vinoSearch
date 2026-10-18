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
use std::path::{Path, PathBuf};
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

pub fn document_text(title: &str, text: &str) -> String {
    format!("title: {title} | text: {text}")
}

struct Stage {
    len: usize,
    _compiled: CompiledModel,
    requests: Vec<InferRequest>,
}

/// Query stage up front; the document stage compiles on first use, so a
/// process that only answers queries never holds a second copy of the model.
pub struct Embedder {
    core: Core,
    onnx: PathBuf,
    device: String,
    cache: PathBuf,
    tok: Tokenizer,
    query: Stage,
    docs: Option<Stage>,
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
    let dev: String = device
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{}-{dev}-{len}", &crate::model::REVISION[..12])
}

/// Drop everything under `root` but this revision's stage directories (for
/// any device, so a one-off run on another device does not cost the NPU a
/// recompile).
fn prune_cache(root: &Path) -> Result<()> {
    let rev = &crate::model::REVISION[..12];
    for e in std::fs::read_dir(root)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let current = e.path().is_dir()
            && name.strip_prefix(rev).is_some_and(|rest| {
                [QUERY_LEN, DOC_LEN]
                    .iter()
                    .any(|len| rest.starts_with('-') && rest.ends_with(&format!("-{len}")))
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

impl Stage {
    fn new(
        core: &mut Core,
        model_path: &Path,
        device: &str,
        cache_root: &Path,
        len: usize,
        in_flight: usize,
    ) -> Result<Self> {
        let cache = cache_root.join(cache_key(device, len));
        std::fs::create_dir_all(&cache)?;
        core.set_property(
            &DeviceType::from(device),
            &RwPropertyKey::CacheDir,
            cache.to_str().context("cache path is not UTF-8")?,
        )
        .context("setting CACHE_DIR")?;
        let before = blobs(&cache);
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
        // A new blob means a cache miss (a new OpenVINO, say), so the older
        // ones in this directory are for a build that no longer loads them.
        if blobs(&cache).iter().any(|b| !before.contains(b)) {
            for old in before {
                let _ = std::fs::remove_file(old);
            }
        }
        Ok(Self {
            len,
            _compiled: compiled,
            requests,
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

    fn run(&mut self, tok: &Tokenizer, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for window in texts.chunks(self.requests.len()) {
            for (req, text) in self.requests.iter_mut().zip(window) {
                Self::feed(tok, self.len, req, text)?;
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
    pub fn new(mut core: Core, model_dir: &Path, device: &str, cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        prune_cache(cache_dir)?;
        let onnx = crate::model::onnx_path(model_dir);
        let tok_path = crate::model::tokenizer_path(model_dir);
        let tok =
            Tokenizer::from_file(&tok_path).map_err(|e| anyhow!("{}: {e}", tok_path.display()))?;
        let query = Stage::new(&mut core, &onnx, device, cache_dir, QUERY_LEN, 1)?;
        Ok(Self {
            core,
            onnx,
            device: device.to_owned(),
            cache: cache_dir.to_owned(),
            tok,
            query,
            docs: None,
        })
    }

    pub fn query(&mut self, text: &str) -> Result<Vec<f32>> {
        let text = format!("{QUERY_PREFIX}{text}");
        let mut v = if fits(&self.tok, &text, QUERY_LEN)? {
            self.query.run(&self.tok, &[text])?
        } else {
            // Too long for the query shape: the document shape takes 256
            // tokens, and is compiled already whenever the runner has indexed.
            self.prepare_documents()?;
            self.docs.as_mut().unwrap().run(&self.tok, &[text])?
        };
        Ok(v.remove(0))
    }

    /// Compile the document stage now, if it is not already.
    pub fn prepare_documents(&mut self) -> Result<()> {
        if self.docs.is_none() {
            self.docs = Some(Stage::new(
                &mut self.core,
                &self.onnx,
                &self.device,
                &self.cache,
                DOC_LEN,
                IN_FLIGHT,
            )?);
        }
        Ok(())
    }

    /// Embed documents already formatted with [`document_text`].
    pub fn documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.prepare_documents()?;
        self.docs.as_mut().unwrap().run(&self.tok, texts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_only_this_revisions_stage_directories() {
        let root = std::env::temp_dir().join(format!("vinosearch-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let keep = [
            cache_key("NPU", QUERY_LEN),
            cache_key("NPU", DOC_LEN),
            cache_key("CPU", DOC_LEN),
        ];
        let drop = [
            cache_key("NPU", 64),
            format!("0123456789ab-NPU-{QUERY_LEN}"),
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
