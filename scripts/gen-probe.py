"""Writes src/probe.onnx: Relu over a static [1, 64] f32 input.

    uv run --with onnx scripts/gen-probe.py
"""

from pathlib import Path

import onnx
from onnx import TensorProto, helper

LEN = 64

graph = helper.make_graph(
    [helper.make_node("Relu", ["input"], ["output"])],
    "vinosearch_probe",
    [helper.make_tensor_value_info("input", TensorProto.FLOAT, [1, LEN])],
    [helper.make_tensor_value_info("output", TensorProto.FLOAT, [1, LEN])],
)
model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 13)], producer_name="vinosearch")
model.ir_version = 8
onnx.checker.check_model(model)
out = Path(__file__).resolve().parent.parent / "src" / "probe.onnx"
onnx.save(model, out)
print(f"{out} ({out.stat().st_size} bytes)")
