"""Speed and energy of the full static image pipeline on the NPU (#2).

    uv run --python 3.13 --no-project --with openvino==2026.4.1 --with numpy python -I \
        bench/image_energy.py <inputs.npz> <cache dir> <vision onnx dir> <text onnx dir> [tokens...]

The vision encoder (static_vision.py) is followed by model_image_<tokens>.onnx
(static_image_text.py), both on the NPU, at batch 1, 3 rounds over the images
in <inputs.npz>. That file holds per-image processor outputs (<k>_pv<i>,
<k>_pos<i>, <k>_ids<i>, <k>_mask<i>) and sentence-transformers' embedding
(<k>_ref<i>). Energy is from RAPL: package (intel-rapl:0) and system psys
(intel-rapl:1), which need read access (`sudo chmod o+r .../energy_uj`, until
reboot). It reports J/image, both total and above the idle draw measured
before the run. Close anything busy first: a moving idle makes "above idle" meaningless.
"""
import os, sys, time, json
import numpy as np
import openvino as ov
npz, cache, vdir, tdir = sys.argv[1:5]
D = np.load(npz); n = int(D["n"])
RAPL = {"pkg": "/sys/class/powercap/intel-rapl:0/energy_uj", "psys": "/sys/class/powercap/intel-rapl:1/energy_uj"}
WRAP = 262143328850
NPU_BUSY = "/sys/class/accel/accel0/device/npu_busy_time_us"
ROUNDS = 3
read = lambda p: int(open(p).read())
def snap():
    t = os.times()
    return {"wall": time.perf_counter(), "cpu": t.user + t.system, "npu": read(NPU_BUSY), **{k: read(p) for k, p in RAPL.items()}}
def delta(a, b):
    d = {k: b[k] - a[k] for k in a}
    for k in RAPL:
        if d[k] < 0: d[k] += WRAP
    return d
def idle(s=15):
    a = snap(); time.sleep(s); d = delta(a, snap()); return {k: d[k] / 1e6 / d["wall"] for k in RAPL}
core = ov.Core(); core.set_property({"CACHE_DIR": cache})
base = idle()
print(f"idle pkg {base['pkg']:.2f} W psys {base['psys']:.2f} W", flush=True)
for k in [int(x) for x in sys.argv[5:]] or (280, 140, 70):
    L = -(-(k + 4) // 16) * 16
    vis = core.compile_model(vdir + ("/vision_encoder_static.onnx" if k == 280 else f"/vision_encoder_static_{k}.onnx"), "NPU")
    for dev in ("NPU",):
        txt = core.compile_model(tdir + f"/model_image_{k}.onnx", dev)
        feeds = []
        for i in range(n):
            ids, mask = D[f"{k}_ids{i}"], D[f"{k}_mask{i}"]; pad = L - ids.shape[1]
            feeds.append(({"pixel_values": D[f"{k}_pv{i}"], "pixel_position_ids": D[f"{k}_pos{i}"]},
                          np.pad(ids, ((0, 0), (0, pad))), np.pad(mask, ((0, 0), (0, pad)))))
        def one(i):
            v, ids, mask = feeds[i]
            f = vis(v)[vis.outputs[0]]
            return txt({"input_ids": ids, "attention_mask": mask, "image_features": f})[txt.outputs[-1]][0]
        one(0); one(1)
        cos = []
        a = snap()
        for r in range(ROUNDS):
            for i in range(n):
                e = one(i)
                if r == 0: cos.append(float(e @ D[f"{k}_ref{i}"] / np.linalg.norm(e)))
        d = delta(a, snap()); runs = ROUNDS * n
        row = {"tokens": k, "text_on": dev, "ms_per_image": round(d["wall"] / runs * 1000),
               "npu_busy_ms": round(d["npu"] / 1000 / runs), "cpu_s": round(d["cpu"] / runs, 2),
               **{f"{p}_J": round(d[p] / 1e6 / runs, 2) for p in RAPL},
               **{f"{p}_above_idle_J": round((d[p] / 1e6 - base[p] * d["wall"]) / runs, 2) for p in RAPL},
               "cos_min": round(min(cos), 4)}
        print(json.dumps(row), flush=True)
        del txt; time.sleep(5)
    del vis
after = idle()
print(f"idle after pkg {after['pkg']:.2f} W psys {after['psys']:.2f} W", flush=True)
