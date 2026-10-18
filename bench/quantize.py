"""Compress a benchmark model's weights with NNCF, data-free, to ONNX or OpenVINO IR.

    uv run --no-project --with openvino==2026.4.1 --with nncf==3.4.0 --with huggingface_hub \
        --with numpy bench/quantize.py gemma2-270m-rope [int8,int4cw] [ir|onnx]

Everything stays symmetric and per-channel (group_size=-1): grouped INT4
unrolls on the NPU compiler into a compile that does not finish (measured on
Gemma 4 E2B, see arankays/gemma-4-E2B-it-int4-cw-ov). Writes
~/.cache/vinosearch/bench/quant/<model>-<variant>/model.onnx (onnx) or model.xml (ir).
ONNX is what ships: ovfetch's runtime has the ONNX frontend only.
"""

import sys
from pathlib import Path

import onnx

import nncf
import openvino as ov

from embed_bench import CACHE, MODELS, fetch

VARIANTS = {
    "int8": dict(mode=nncf.CompressWeightsMode.INT8_SYM),
    # The Gemma 4 recipe: 80% INT4, the most sensitive layers INT8.
    "int4cw": dict(
        mode=nncf.CompressWeightsMode.INT4_SYM,
        group_size=-1,
        ratio=0.8,
        sensitivity_metric=nncf.SensitivityMetric.WEIGHT_QUANTIZATION_ERROR,
        backup_mode=nncf.BackupMode.INT8_SYM,
    ),
}


def main():
    key = sys.argv[1]
    variants = sys.argv[2].split(",") if len(sys.argv) > 2 else list(VARIANTS)
    fmt = sys.argv[3] if len(sys.argv) > 3 else "ir"
    spec = MODELS[key]
    onnx_path = fetch(spec["repo"], "onnx/model.onnx")
    if "data" in spec:
        fetch(spec["repo"], spec["data"])
    if "onnx" in spec:
        onnx_path = onnx_path.parent / Path(spec["onnx"]).name
    for variant in variants:
        out_dir = CACHE / "quant" / f"{key}-{variant}"
        out_dir.mkdir(parents=True, exist_ok=True)
        if fmt == "onnx":
            # ONNX in, ONNX out: what ovfetch's runtime (ONNX frontend only) can load.
            model = onnx.load(str(onnx_path))
            model = nncf.compress_weights(model, **VARIANTS[variant])
            out = out_dir / "model.onnx"
            onnx.save(model, str(out), save_as_external_data=True, location="model.onnx_data")
        else:
            model = nncf.compress_weights(ov.Core().read_model(onnx_path), **VARIANTS[variant])
            out = out_dir / "model.xml"
            ov.save_model(model, out, compress_to_fp16=False)
        size = sum(f.stat().st_size for f in out_dir.iterdir()) / 1e6
        print(f"{key}-{variant}: {out} ({size:.0f} MB)")

if __name__ == "__main__":
    main()
