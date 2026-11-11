"""Check the static audio encoder (static_audio.py, #190) against upstream's, and what text queries find with it.

    uv run --python 3.13 --no-project --with onnx --with numpy --with onnxruntime --with openvino==2026.4.1 \
        --with transformers --with tokenizers bench/audio_check.py <audio dir> <text dir> <clips.f32> [device]

<audio dir> holds onnx/audio_encoder.onnx (upstream, the reference) and the
static graphs static_audio.py wrote; <text dir> holds onnx/model_text.onnx,
onnx/model_audio_250.onnx and tokenizer.json. <clips.f32> is 16 kHz mono
float32; CLIPS below names spans of it, which are the spike's clip
(av_spike/make_clip.sh writes it as audio.f32). Prints, for each graph and runtime,
the cosine of its audio_features to the reference's (mean and worst row),
then each clip's sentence embedding against the reference's, then text
queries against the int8 graph's clips. device is an OpenVINO device (CPU).
"""

import sys
from pathlib import Path

import numpy as np
import onnxruntime as ort
import openvino as ov
from tokenizers import Tokenizer
from transformers import AutoFeatureExtractor

from static_audio import frames, tokens

SR, SECONDS = 16000, 10
BOS, EOS, BOA, EOA, AUDIO = 2, 1, 256000, 258883, 258881
L = 256
# (name, start s, end s) in the clips file; the last is short, so padded.
CLIPS = [
    ("talk", 0, 10),
    ("dog", 16.1, 26.1),
    ("bells", 44.5, 54.5),
    ("rain", 54.5, 64.5),
    ("siren", 81, 91),
    ("siren 6 s", 85, 91),
]
QUERIES = ["someone talking", "a dog barking", "church bells ringing", "rain falling", "an ambulance siren"]


def cos(a, b):
    a, b = np.asarray(a, np.float64), np.asarray(b, np.float64)
    return (a * b).sum(-1) / np.linalg.norm(a, axis=-1) / np.linalg.norm(b, axis=-1)


def main():
    audio_dir, text_dir, clips_path = (Path(p) for p in sys.argv[1:4])
    device = sys.argv[4] if len(sys.argv) > 4 else "CPU"
    onnx_dir = audio_dir / "onnx"
    wave = np.fromfile(clips_path, np.float32)
    fe = AutoFeatureExtractor.from_pretrained(audio_dir)
    n_static = frames(SECONDS)

    feats = []
    for name, s, e in CLIPS:
        f = fe([wave[int(s * SR):int(e * SR)]], sampling_rate=SR, return_tensors="np")
        x, m = f["input_features"].astype(np.float32), f["input_features_mask"].astype(bool)
        n = int(m.sum())
        px = np.zeros((1, n_static, x.shape[2]), np.float32)
        pm = np.zeros((1, n_static), bool)
        px[:, :x.shape[1]], pm[:, :m.shape[1]] = x[:, :n_static], m[:, :n_static]
        feats.append((name, x, m, px, pm, tokens(n)))

    ref = ort.InferenceSession(str(onnx_dir / "audio_encoder.onnx"), providers=["CPUExecutionProvider"])
    reference = {name: ref.run(None, {"input_features": x, "input_features_mask": m})[0] for name, x, m, *_ in feats}
    del ref

    core = ov.Core()
    graphs = {}
    for graph in ("audio_encoder_static_10s.onnx", "audio_encoder_static_10s_int8.onnx"):
        path = onnx_dir / graph
        if not path.exists():
            continue
        # One at a time: two fp32 copies don't fit in 4 GB.
        for runtime in ("ORT", f"OV {device}"):
            if runtime == "ORT":
                r = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])
            else:
                r = core.compile_model(core.read_model(path), device)
            out, worst, mean = {}, 1.0, []
            for name, _, _, px, pm, t in feats:
                inputs = {"input_features": px, "input_features_mask": pm}
                rows = (r.run(None, inputs)[0] if runtime == "ORT" else r(inputs)["audio_features"])[:t]
                c = cos(rows, reference[name])
                if c.min() < 0.99:
                    i = int(c.argmin())
                    norms = np.linalg.norm(reference[name], axis=-1)
                    low = int((c < 0.99).sum())
                    print(f"    {name}: {low}/{len(c)} rows under 0.99, worst row {i} ({c[i]:.3f}), "
                          f"its norm {norms[i] / np.median(norms):.2f}x the median")
                worst, out[name] = min(worst, c.min()), rows
                mean.append(c.mean())
            graphs[(graph, runtime)] = out
            print(f"{graph:38} {runtime:7} features cosine mean {np.mean(mean):.5f}, worst row {worst:.5f}")
            del r

    text = core.compile_model(core.read_model(text_dir / "onnx" / "model_audio_250.onnx"), device)
    query_model = core.compile_model(core.read_model(text_dir / "onnx" / "model_text.onnx"), device)
    tok = Tokenizer.from_file(str(text_dir / "tokenizer.json"))
    tok.enable_truncation(L)
    tok.enable_padding(length=L)

    def embed_audio(rows):
        ids = [BOS, BOA] + [AUDIO] * len(rows) + [EOA, EOS]
        mask = [1] * len(ids) + [0] * (L - len(ids))
        ids += [0] * (L - len(ids))
        feats = np.zeros((tokens(n_static), 512), np.float32)
        feats[:len(rows)] = rows
        out = text({"input_ids": np.array([ids], np.int64), "attention_mask": np.array([mask], np.int64),
                    "audio_features": feats})["sentence_embedding"][0]
        return out / np.linalg.norm(out)

    ref_emb = {name: embed_audio(rows) for name, rows in reference.items()}
    for key, out in graphs.items():
        c = [float(cos(embed_audio(out[name]), ref_emb[name])) for name in out]
        print(f"{key[0]:38} {key[1]:7} embedding cosine min {min(c):.5f}")

    int8 = graphs.get(("audio_encoder_static_10s_int8.onnx", f"OV {device}"))
    if int8 is None:
        return
    qs = []
    for q in QUERIES:
        enc = tok.encode(f"task: search result | query: {q}")
        e = query_model({"input_ids": np.array([enc.ids], np.int64),
                         "attention_mask": np.array([enc.attention_mask], np.int64)})["sentence_embedding"][0]
        qs.append(e / np.linalg.norm(e))
    names = list(int8)
    print("\nint8 clips against queries (best per query starred):")
    print(" " * 22 + "".join(f"{n:>11}" for n in names))
    for q, qe in zip(QUERIES, qs):
        scores = [float(qe @ embed_audio(int8[n])) for n in names]
        best = int(np.argmax(scores))
        print(f"{q:22}" + "".join(f"{s:>10.3f}{'*' if i == best else ' '}" for i, s in enumerate(scores)))


if __name__ == "__main__":
    main()
