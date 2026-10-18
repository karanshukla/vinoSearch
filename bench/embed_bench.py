"""Speed and retrieval quality of candidate embedding models on the NPU and CPU.

    uv run --no-project --with openvino==2026.4.1 --with tokenizers --with huggingface_hub \
        --with numpy bench/embed_bench.py [--models bge-small,...] [--devices NPU,CPU]

OpenVINO is pinned to the release ovfetch installs, so the NPU sees what the
daemon will. Every shape is static, as the NPU needs: queries [1, QUERY_LEN],
documents [batch, DOC_LEN], where batch is 1 on the NPU: as of OpenVINO 2026.4.1
it mixes rows across a batch (cosine to CPU as low as 0.59 on bge-micro at
batch 8, exactly 1.0 at batch 1), and batch 1 with requests in flight is faster
anyway. Quality is nDCG@10 on BEIR SciFact (test
split), embedded on the first device given. Results land in bench/results/.
GPU works but OpenVINO's GPU plugin segfaulted intermittently in compile_model,
so it is not in the defaults.
"""

import argparse
import json
import math
import statistics
import subprocess
import sys
import time
from pathlib import Path

import numpy as np
import openvino as ov
from huggingface_hub import hf_hub_download
from tokenizers import Tokenizer

QUERY_LEN = 32
DOC_LEN = 256
DOC_BATCH = {"NPU": 1}
DEFAULT_BATCH = 8
IN_FLIGHT = 4
SPEED_DOCS = 256
QUERY_RUNS = 50
DRIFT_DOCS = 64

CACHE = Path.home() / ".cache/vinosearch/bench"
RESULTS = Path(__file__).resolve().parent / "results"

BGE_QUERY = "Represent this sentence for searching relevant passages: "
MODELS = {
    "bge-micro": dict(repo="TaylorAI/bge-micro-v2"),
    "minilm-l6": dict(repo="sentence-transformers/all-MiniLM-L6-v2"),
    "arctic-xs": dict(repo="Snowflake/snowflake-arctic-embed-xs", query=BGE_QUERY),
    "bge-small": dict(repo="BAAI/bge-small-en-v1.5", query=BGE_QUERY),
    "arctic-s": dict(repo="Snowflake/snowflake-arctic-embed-s", query=BGE_QUERY),
    "e5-small-ml": dict(repo="intfloat/multilingual-e5-small", query="query: ", doc="passage: "),
    "nomic-v1.5": dict(repo="nomic-ai/nomic-embed-text-v1.5", query="search_query: ", doc="search_document: "),
    # Its card says activations do not survive fp16, which the NPU computes in.
    "gemma-300m": dict(
        repo="onnx-community/embeddinggemma-300m-ONNX",
        query="task: search result | query: ",
        doc="title: none | text: ",
        data="onnx/model.onnx_data",
        pooling="mean",
    ),
    # Text model only; vision and audio encoders are separate files.
    "gemma2-270m": dict(
        repo="onnx-community/embeddinggemma-2-ONNX",
        query="task: search result | query: ",
        doc="title: none | text: ",
        data="onnx/model.onnx_data",
        pooling="mean",
    ),
}
# The same export with RotaryEmbedding rewritten by bench/patch_rope.py, which
# OpenVINO otherwise mis-converts.
MODELS["gemma2-270m-rope"] = {**MODELS["gemma2-270m"], "onnx": "onnx/model_rope.onnx"}
# NNCF-compressed models from bench/quantize.py (ONNX if made, else IR),
# tokenized like the model they came from.
for _base in ("gemma2-270m-rope",):
    for _variant in ("int8", "int4cw"):
        _dir = CACHE / "quant" / f"{_base}-{_variant}"
        _spec = {k: v for k, v in MODELS[_base].items() if k not in ("data", "onnx")}
        _spec["ir"] = _dir / "model.onnx" if (_dir / "model.onnx").exists() else _dir / "model.xml"
        MODELS[f"{_base}-{_variant}"] = _spec


def fetch(repo, name):
    # Real files, not the hub cache's symlinks into blobs/: OpenVINO refuses an
    # ONNX external-data path that resolves outside the model's directory.
    return Path(hf_hub_download(repo, name, local_dir=CACHE / "models" / repo.replace("/", "--")))


def pooling_mode(spec):
    if "pooling" in spec:
        return spec["pooling"]
    cfg = json.loads(fetch(spec["repo"], "1_Pooling/config.json").read_text())
    return "cls" if cfg.get("pooling_mode_cls_token") else "mean"


def load_scifact():
    root = CACHE / "scifact"
    def get(name):
        return Path(hf_hub_download("mteb/scifact", name, repo_type="dataset", cache_dir=CACHE / "hf"))
    corpus = [json.loads(l) for l in get("corpus.jsonl").read_text().splitlines() if l.strip()]
    queries = {q["_id"]: q["text"] for q in map(json.loads, get("queries.jsonl").read_text().splitlines()) if q}
    qrels = {}
    for line in get("qrels/test.tsv").read_text().splitlines()[1:]:
        qid, did, score = line.split("\t")
        if int(score) > 0:
            qrels.setdefault(qid, {})[did] = int(score)
    root.mkdir(parents=True, exist_ok=True)
    docs = [(d["_id"], (d.get("title", "") + " " + d["text"]).strip()) for d in corpus]
    return docs, {q: queries[q] for q in qrels}, qrels


class Embedder:
    def __init__(self, core, spec, device, batch, length, in_flight=1):
        self.repo = spec["repo"]
        self.batch, self.length = batch, length
        self.pooling = pooling_mode(spec)
        self.tok = Tokenizer.from_file(str(fetch(self.repo, "tokenizer.json")))
        self.tok.enable_truncation(length)
        self.tok.enable_padding(length=length)
        if "ir" in spec:
            path = spec["ir"]
        else:
            path = fetch(self.repo, "onnx/model.onnx")
            if "onnx" in spec:
                path = path.parent / Path(spec["onnx"]).name
            if "data" in spec:
                fetch(self.repo, spec["data"])
        model = core.read_model(path)
        self.inputs = [i.get_any_name() for i in model.inputs]
        # EmbeddingGemma 2 splices encoded media into the text sequence through
        # *_features inputs; text-only search feeds them empty.
        self.features = {
            i.get_any_name(): i.get_partial_shape()[1].get_length()
            for i in model.inputs
            if i.get_any_name().endswith("_features")
        }
        model.reshape({
            n: ov.PartialShape([0, self.features[n]] if n in self.features else [batch, length])
            for n in self.inputs
        })
        outputs = [o.get_any_name() for o in model.outputs]
        self.output = "sentence_embedding" if "sentence_embedding" in outputs else outputs[0]
        t0 = time.perf_counter()
        hint = "THROUGHPUT" if in_flight > 1 else "LATENCY"
        self.compiled = core.compile_model(model, device, {"PERFORMANCE_HINT": hint})
        self.compile_s = time.perf_counter() - t0
        self.queue = ov.AsyncInferQueue(self.compiled, in_flight)

    def embed(self, texts):
        chunks = {}
        output = self.output
        # Capturing self would cycle queue -> callback -> self and keep every
        # compiled model alive until the process exits.
        self.queue.set_callback(lambda req, i: chunks.__setitem__(i, req.get_tensor(output).data.copy()))
        masks = {}
        for i in range(0, len(texts), self.batch):
            chunk = texts[i : i + self.batch]
            enc = self.tok.encode_batch(chunk + [""] * (self.batch - len(chunk)))
            ids = np.array([e.ids for e in enc], dtype=np.int64)
            mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
            feed = {}
            for name in self.inputs:
                if name in self.features:
                    feed[name] = np.zeros((0, self.features[name]), dtype=np.float32)
                elif "mask" in name:
                    feed[name] = mask
                elif "type" in name:
                    feed[name] = np.zeros_like(ids)
                else:
                    feed[name] = ids
            masks[i] = (mask, len(chunk))
            self.queue.start_async(feed, i)
        self.queue.wait_all()
        out = []
        for i in sorted(chunks):
            res, (mask, n) = chunks[i], masks[i]
            if res.ndim == 3:
                if self.pooling == "cls":
                    res = res[:, 0]
                else:
                    m = mask[..., None].astype(np.float32)
                    res = (res * m).sum(1) / np.maximum(m.sum(1), 1)
            out.append(res[:n])
        e = np.concatenate(out).astype(np.float32)
        return e / np.maximum(np.linalg.norm(e, axis=1, keepdims=True), 1e-12)


def ndcg_at_10(scores, doc_ids, qids, qrels):
    total = 0.0
    for row, qid in zip(scores, qids):
        top = np.argsort(-row)[:10]
        rel = qrels[qid]
        dcg = sum(rel.get(doc_ids[j], 0) / math.log2(r + 2) for r, j in enumerate(top))
        ideal = sorted(rel.values(), reverse=True)[:10]
        idcg = sum(g / math.log2(r + 2) for r, g in enumerate(ideal))
        total += dcg / idcg
    return total / len(qids)


def battery_watts():
    for bat in Path("/sys/class/power_supply").glob("BAT*"):
        if (bat / "status").read_text().strip() == "Discharging" and (bat / "power_now").exists():
            return int((bat / "power_now").read_text()) / 1e6
    return None


def run_model(core, key, devices, docs, queries, qrels):
    spec = MODELS[key]
    q_pre, d_pre = spec.get("query", ""), spec.get("doc", "")
    doc_texts = [d_pre + t for _, t in docs]
    result = {"model": key, "repo": spec["repo"], "devices": {}}
    sample = doc_texts[:SPEED_DOCS]
    per_device_drift = {}
    for device in devices:
        r = {}
        try:
            qe = Embedder(core, spec, device, 1, QUERY_LEN)
            batch = DOC_BATCH.get(device, DEFAULT_BATCH)
            de = Embedder(core, spec, device, batch, DOC_LEN, IN_FLIGHT)
        except Exception as e:
            r["error"] = str(e).splitlines()[0][:300]
            result["devices"][device] = r
            print(f"  {device}: compile failed: {r['error']}")
            continue
        r["compile_s"] = round(qe.compile_s + de.compile_s, 2)
        q = q_pre + "how do I configure the npu driver"
        qe.embed([q])
        lat = []
        for _ in range(QUERY_RUNS):
            t0 = time.perf_counter()
            qe.embed([q])
            lat.append((time.perf_counter() - t0) * 1000)
        r["query_ms_p50"] = round(statistics.median(lat), 2)
        r["query_ms_p95"] = round(sorted(lat)[int(len(lat) * 0.95) - 1], 2)
        de.embed(sample[: batch * IN_FLIGHT])
        watts = []
        t0 = time.perf_counter()
        for i in range(0, len(sample), 32):
            de.embed(sample[i : i + 32])
            w = battery_watts()
            if w is not None:
                watts.append(w)
        r["docs_per_s"] = round(len(sample) / (time.perf_counter() - t0), 1)
        if watts:
            r["battery_w"] = round(statistics.mean(watts), 1)
        per_device_drift[device] = de.embed(doc_texts[:DRIFT_DOCS])
        if device == devices[0]:
            print(f"  {device}: embedding {len(doc_texts)} docs for nDCG@10")
            t0 = time.perf_counter()
            D = de.embed(doc_texts)
            r["corpus_s"] = round(time.perf_counter() - t0, 1)
            qids = list(queries)
            Q = np.concatenate([qe.embed([q_pre + queries[x]]) for x in qids])
            r["ndcg10"] = round(ndcg_at_10(Q @ D.T, [d for d, _ in docs], qids, qrels), 4)
        result["devices"][device] = r
        print(f"  {device}: {r}")
        del qe, de
    if "CPU" in per_device_drift:
        ref = per_device_drift["CPU"]
        for device, emb in per_device_drift.items():
            if device != "CPU":
                result["devices"][device]["cos_vs_cpu_min"] = round(float((emb * ref).sum(1).min()), 4)
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", default=",".join(k for k, v in MODELS.items() if "ir" not in v or v["ir"].exists()))
    ap.add_argument("--devices", default="NPU,CPU")
    ap.add_argument("--skip-done", action="store_true", help="skip models with a results file")
    ap.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    args = ap.parse_args()
    keys = args.models.split(",")
    if not args.child:
        # One process per model: compiled models are only reliably freed at
        # exit, and a plugin segfault then costs one model, not the run.
        for key in keys:
            if args.skip_done and (RESULTS / f"{key}.json").exists():
                print(f"== {key}: done, skipping")
                continue
            cmd = [sys.executable, "-u", __file__, "--child", "--models", key, "--devices", args.devices]
            code = subprocess.run(cmd).returncode
            if code:
                print(f"== {key}: exited {code}")
        return
    core = ov.Core()
    devices = [d for d in args.devices.split(",") if d in core.available_devices]
    print(f"OpenVINO {ov.get_version()}, devices {devices}")
    docs, queries, qrels = load_scifact()
    RESULTS.mkdir(exist_ok=True)
    for key in keys:
        print(f"== {key}")
        res = run_model(core, key, devices, docs, queries, qrels)
        (RESULTS / f"{key}.json").write_text(json.dumps(res, indent=2) + "\n")

if __name__ == "__main__":
    main()
