"""Bake vinoSearch's static input shapes into copies of model_text.onnx.

    uv run --python 3.13 --no-project --with onnx bench/static_shapes.py <dir with onnx/model_text.onnx>

The NPU needs static shapes, and the runner used to read the model, reshape
it and compile it on every load. That reads the 273 MB of weights even when
OpenVINO's cache already holds the compiled blob. A graph whose inputs are
already [1, len] can be compiled straight from its path, which lets the cache
skip reading the model: 340 MB peak and 0.04 s instead of 630 MB and 0.17 s
on a warm NPU cache (OpenVINO 2026.4.1). Writes onnx/model_text_q32.onnx and
onnx/model_text_d256.onnx, sharing model.onnx_data.
"""

import sys
from pathlib import Path

import onnx

SHAPES = {"q32": 32, "d256": 256}
INPUTS = ("input_ids", "attention_mask")


def main():
    src = Path(sys.argv[1]) / "onnx" / "model_text.onnx"
    for tag, length in SHAPES.items():
        model = onnx.load(str(src), load_external_data=False)
        names = [i.name for i in model.graph.input]
        if sorted(names) != sorted(INPUTS):
            raise SystemExit(f"{src}: inputs {names}, expected {INPUTS}")
        for inp in model.graph.input:
            dims = inp.type.tensor_type.shape.dim
            for dim, value in zip(dims, (1, length)):
                dim.ClearField("dim_param")
                dim.dim_value = value
        dst = src.parent / f"model_text_{tag}.onnx"
        onnx.save(model, str(dst))
        print(f"{dst}: {[(i.name, [d.dim_value for d in i.type.tensor_type.shape.dim]) for i in model.graph.input]}")


if __name__ == "__main__":
    main()
