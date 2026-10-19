"""Flickr30k 1k test retrieval with the static vision encoder on the NPU (#2).

    hf download nlphuji/flickr_1k_test_image_text_retrieval --type dataset --local-dir <dataset dir>
    ENV="uv run --python 3.13 --no-project --with sentence-transformers[image] --with transformers==5.20.1 \
         --with torch --with torchvision --with pillow --with openvino==2026.4.1 --with numpy python -I"
    $ENV bench/image_retrieval.py captions <st dir glob> <dataset dir> <out dir>
    $ENV bench/image_retrieval.py images <st dir glob> <dataset dir> <out dir> <onnx dir> <tokens>
    $ENV bench/image_retrieval.py score <out dir>

<st dir glob> is google/embeddinggemma-2 for sentence-transformers. Captions
are embedded there with encode_query. Images go through its preprocess, which
builds the exact input_ids, then vision_encoder_static[_<tokens>].onnx
(static_vision.py) on the NPU, with the text model on the CPU alongside it.
`images` is resumable. `score` prints recall@1/5/10 both ways. Keep <out dir>
on disk, not tmpfs: it holds OpenVINO's cache, 2 GB for three token counts.
"""
import csv, glob, json, os, sys, time
import numpy as np

def rows(ds):
    with open(os.path.join(ds, "test_1k_flickr.csv")) as f:
        return [(r["filename"], json.loads(r["raw"])) for r in csv.DictReader(f)]

def captions(st_dir, ds, out):
    from sentence_transformers import SentenceTransformer
    st = SentenceTransformer(glob.glob(st_dir)[0], device="cpu")
    caps, owner = [], []
    for i, (_, raw) in enumerate(rows(ds)):
        caps += raw; owner += [i] * len(raw)
    t = time.time()
    q = st.encode_query(caps, batch_size=64, normalize_embeddings=True)
    print(f"{len(caps)} captions in {time.time()-t:.0f}s", flush=True)
    np.savez(os.path.join(out, "captions.npz"), q=q.astype(np.float32), owner=np.array(owner))

def images(st_dir, ds, out, onnx_dir, k):
    import openvino as ov
    from PIL import Image
    from sentence_transformers import SentenceTransformer
    st = SentenceTransformer(glob.glob(st_dir)[0], device="cpu")  # its preprocess builds the exact input_ids
    st[0].processor.image_processor.max_soft_tokens = k
    core = ov.Core(); core.set_property({"CACHE_DIR": os.path.join(out, "ovcache")})
    vis = core.compile_model(os.path.join(onnx_dir, "vision_encoder_static.onnx" if k == 280 else f"vision_encoder_static_{k}.onnx"), "NPU")
    text = core.compile_model(os.path.join(onnx_dir, "model_rope.onnx"), "CPU")
    req = vis.create_infer_request()
    empty = np.zeros((0, 512), np.float32)
    path = os.path.join(out, f"images_{k}.npy")
    R = rows(ds)
    E = np.load(path) if os.path.exists(path) else np.zeros((len(R), 768), np.float32)
    todo = [i for i in range(len(R)) if not E[i].any()]
    def prep(i):
        f = st.preprocess([Image.open(os.path.join(ds, "images_flickr_1k_test", R[i][0])).convert("RGB")])
        pos = f["image_position_ids"].numpy()
        g = pos[0][(pos[0] >= 0).all(-1)].max(0) + 1
        return f, (g[0] // 3) * (g[1] // 3)
    t0 = time.time(); busy = 0.0
    pending = None
    for j, i in enumerate(todo + [None]):
        nxt = None
        if i is not None:
            f, cells = prep(i)
            req.start_async({"pixel_values": f["pixel_values"].numpy(), "pixel_position_ids": f["image_position_ids"].numpy()})
            nxt = (i, f, cells)
        if pending is not None:
            pi, pf, pfeats = pending
            e = text({"input_ids": pf["input_ids"].numpy(), "attention_mask": pf["attention_mask"].numpy(),
                      "image_features": pfeats, "video_features": empty, "audio_features": empty})[text.outputs[1]][0]
            E[pi] = e / np.linalg.norm(e)
        if nxt is not None:
            req.wait()
            i, f, cells = nxt
            pending = (i, f, req.get_output_tensor(0).data[:cells].copy())
        else:
            pending = None
        if j % 50 == 0 or i is None:
            np.save(path, E)
            print(f"k={k} {j}/{len(todo)} {time.time()-t0:.0f}s", flush=True)

def recall(sims, gold_sets, ks=(1, 5, 10)):
    order = np.argsort(-sims, 1)
    return {f"R@{k}": round(100 * float(np.mean([bool(set(order[r, :k]) & gold_sets[r]) for r in range(len(order))])), 1) for k in ks}

def score(out):
    c = np.load(os.path.join(out, "captions.npz")); Q, owner = c["q"], c["owner"]
    for k in (280, 140, 70):
        p = os.path.join(out, f"images_{k}.npy")
        if not os.path.exists(p): continue
        E = np.load(p)
        if not E.any(1).all(): print(k, "incomplete", int(E.any(1).sum())); continue
        S = Q @ E.T
        t2i = recall(S, [{o} for o in owner])
        i2t = recall(S.T, [set(np.where(owner == i)[0]) for i in range(len(E))])
        print(json.dumps({"tokens": k, "text_to_image": t2i, "image_to_text": i2t}))

if __name__ == "__main__":
    cmd, *a = sys.argv[1:]
    {"captions": captions, "images": lambda *x: images(*x[:4], int(x[4])), "score": score}[cmd](*a)
