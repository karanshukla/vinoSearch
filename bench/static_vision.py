"""Give the EmbeddingGemma 2 vision encoder static shapes, so it compiles on the NPU.

    uv run --python 3.13 --no-project --with onnx --with numpy bench/static_vision.py <dir with onnx/vision_encoder.onnx>

The processor pads every image to 2520 patches (280 soft tokens of 3x3), and
the encoder pools them into 280 cells, then keeps the cells holding a real
patch with NonZero + GatherND. That selection is the only dynamic shape, and
the NPU compiler refuses it (#2). This drops it: image_features is all 280
rows, and the caller keeps the first (h/3)*(w/3), h x w being the patch grid.
image_mask is the graph's own answer for that, right on CPU but not on the
NPU, so don't use it there. Writes onnx/vision_encoder_static.onnx, sharing
vision_encoder.onnx_data.
"""

import sys
from pathlib import Path

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

PATCHES, CELLS, WIDTH, OUT = 2520, 280, 768, 512


def static(dims, values):
    for dim, value in zip(dims, values):
        dim.ClearField("dim_param")
        dim.dim_value = value


def main():
    src = Path(sys.argv[1]) / "onnx" / "vision_encoder.onnx"
    model = onnx.load(str(src), load_external_data=False)
    graph = model.graph
    for inp, dims in zip(graph.input, ((1, PATCHES, WIDTH), (1, PATCHES, 2))):
        static(inp.type.tensor_type.shape.dim, dims)

    gather = next(n for n in graph.node if n.op_type == "GatherND")
    pooled, indices = gather.input
    transpose = next(n for n in graph.node if indices in n.output)
    nonzero = next(n for n in graph.node if transpose.input[0] in n.output)
    for n in (gather, transpose, nonzero):
        graph.node.remove(n)
    graph.initializer.extend([
        numpy_helper.from_array(np.array([-1, WIDTH], np.int64), "static_rows_shape"),
        numpy_helper.from_array(np.array([-1], np.int64), "static_flat"),
    ])
    graph.node.extend([
        helper.make_node("Reshape", [pooled, "static_rows_shape"], [gather.output[0]], name="static_rows"),
        helper.make_node("Reshape", [nonzero.input[0], "static_flat"], ["image_mask"], name="static_mask"),
    ])
    static(graph.output[0].type.tensor_type.shape.dim, (CELLS, OUT))
    graph.output.append(helper.make_tensor_value_info("image_mask", TensorProto.BOOL, [CELLS]))

    # The new Reshapes were appended; ONNX wants producers before consumers.
    pending, ready = list(graph.node), {""}
    ready |= {i.name for i in graph.input} | {i.name for i in graph.initializer}
    del graph.node[:]
    while pending:
        n = next(n for n in pending if all(i in ready for i in n.input))
        graph.node.append(n)
        ready.update(n.output)
        pending.remove(n)

    dst = src.parent / "vision_encoder_static.onnx"
    onnx.save(model, str(dst))
    print(f"{dst}: outputs {[o.name for o in graph.output]}")


if __name__ == "__main__":
    main()
