"""Rewrite com.microsoft RotaryEmbedding nodes as plain ONNX ops.

    uv run --no-project --with onnx --with huggingface_hub bench/patch_rope.py gemma2-270m

OpenVINO 2026.4.1's ONNX frontend gets this contrib op wrong on
onnx-community/embeddinggemma-2-ONNX: every op before the first one matches
ONNX Runtime to 1e-7, its output is off by 190%, and the final embedding lands
at cosine ~0.81 to the reference. The exporter's form is narrow, so only that is
handled: input [B, S, hidden], position_ids the constant offset [0], caches
[S, head/2] for every position, rotate-half (interleaved=0), no num_heads or
rotary_embedding_dim. Anything else is refused rather than guessed at.

Writes onnx/model_rope.onnx next to the original, sharing its external data.
"""

import sys

import numpy as np
import onnx
from onnx import helper, numpy_helper

from embed_bench import MODELS, fetch


def const(graph, name, values):
    graph.initializer.append(numpy_helper.from_array(np.array(values, dtype=np.int64), name))
    return name


def rewrite(model):
    graph = model.graph
    # Weights live in external data that is not loaded; the offset is a small
    # inline initializer or a Constant node.
    offsets = {i.name: numpy_helper.to_array(i) for i in graph.initializer if not i.external_data}
    for n in graph.node:
        if n.op_type == "Constant":
            offsets[n.output[0]] = numpy_helper.to_array(n.attribute[0].t)
    head_dims = {}
    for n in graph.node:
        if n.op_type != "RotaryEmbedding":
            continue
        attrs = {a.name: helper.get_attribute_value(a) for a in n.attribute}
        if attrs != {"interleaved": 0}:
            raise SystemExit(f"{n.name}: unsupported attributes {attrs}")
        pos = offsets.get(n.input[1])
        if pos is None or pos.tolist() != [0]:
            raise SystemExit(f"{n.name}: position_ids {n.input[1]} is not the constant offset [0]")
        head_dims.setdefault(n.input[2], None)

    ax02 = const(graph, "rope/axes_0_2", [0, 2])
    neg1 = const(graph, "rope/axis_-1", [-1])
    zero = const(graph, "rope/zero", [0])
    big = const(graph, "rope/big", [np.iinfo(np.int64).max])
    new_nodes, count = [], 0
    for n in graph.node:
        if n.op_type != "RotaryEmbedding":
            new_nodes.append(n)
            continue
        count += 1
        x, _, cos, sin = n.input
        out = n.output[0]
        p = f"{n.name or out}/rope"
        # Half the head is the cache's last dim; the head is twice that.
        half = f"{p}/half"
        new_nodes += [
            helper.make_node("Shape", [cos], [f"{p}/cos_shape"]),
            helper.make_node("Slice", [f"{p}/cos_shape", neg1, big], [half]),
            helper.make_node("Add", [half, half], [f"{p}/head"]),
            helper.make_node("Concat", [const(graph, f"{p}/lead", [0, 0, -1]), f"{p}/head"], [f"{p}/shape4"], axis=0),
            helper.make_node("Reshape", [x, f"{p}/shape4"], [f"{p}/x4"]),
            helper.make_node("Slice", [f"{p}/x4", zero, half, neg1], [f"{p}/x1"]),
            helper.make_node("Slice", [f"{p}/x4", half, big, neg1], [f"{p}/x2"]),
            helper.make_node("Unsqueeze", [cos, ax02], [f"{p}/c"]),
            helper.make_node("Unsqueeze", [sin, ax02], [f"{p}/s"]),
            helper.make_node("Mul", [f"{p}/x1", f"{p}/c"], [f"{p}/x1c"]),
            helper.make_node("Mul", [f"{p}/x2", f"{p}/s"], [f"{p}/x2s"]),
            helper.make_node("Sub", [f"{p}/x1c", f"{p}/x2s"], [f"{p}/o1"]),
            helper.make_node("Mul", [f"{p}/x2", f"{p}/c"], [f"{p}/x2c"]),
            helper.make_node("Mul", [f"{p}/x1", f"{p}/s"], [f"{p}/x1s"]),
            helper.make_node("Add", [f"{p}/x2c", f"{p}/x1s"], [f"{p}/o2"]),
            helper.make_node("Concat", [f"{p}/o1", f"{p}/o2"], [f"{p}/o4"], axis=-1),
            helper.make_node("Shape", [x], [f"{p}/x_shape"]),
            helper.make_node("Reshape", [f"{p}/o4", f"{p}/x_shape"], [out]),
        ]
    del graph.node[:]
    graph.node.extend(new_nodes)
    return count


def main():
    key = sys.argv[1]
    spec = MODELS[key]
    src = fetch(spec["repo"], "onnx/model.onnx")
    fetch(spec["repo"], spec["data"])
    model = onnx.load(str(src), load_external_data=False)
    count = rewrite(model)
    dst = src.parent / "model_rope.onnx"
    onnx.save(model, str(dst))
    print(f"rewrote {count} RotaryEmbedding nodes -> {dst}")


if __name__ == "__main__":
    main()
