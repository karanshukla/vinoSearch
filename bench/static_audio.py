"""Give the EmbeddingGemma 2 audio encoder static shapes, so it can compile on the NPU (#190).

    uv run --python 3.13 --no-project --with onnx --with numpy bench/static_audio.py <dir with onnx/audio_encoder.onnx> [seconds] [int8]

The feature extractor makes 100 frames a second (999 for 10 s), and the
encoder subsamples them twice by 2 to 25 soft tokens a second, then keeps the
tokens of real frames with NonZero + GatherND on the subsampled mask. As in
the vision encoder (#2), that selection is the only dynamic shape. This fixes
the input at `seconds` of frames (a shorter clip is padded at the end with
input_features_mask false) and drops the selection: audio_features is every
token, and the caller keeps the first ceil(ceil(n/2)/2) for n real frames.
audio_mask is the graph's own answer for that; don't rely on it on the NPU,
where vision's equivalent was wrong.

Writes onnx/audio_encoder_static_<seconds>s.onnx, sharing
audio_encoder.onnx_data; with int8, onnx/audio_encoder_static_<seconds>s_int8.onnx
and its own data, MatMul weights compressed INT8 symmetric per-channel,
data-free, in the layout NNCF 3.4.0's INT8_SYM wrote for the text model (an
int8 weight and a float scale per output channel into DequantizeLinear,
axis -1, no zero point). Done here a tensor at a time because NNCF holds
copies of the whole fp32 graph, over 3.4 GB, which a 4 GB machine can't.
"""

import sys
from pathlib import Path

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

MELS, OUT = 128, 512
FRAMES_PER_S = 100


def static(dims, values):
    for dim, value in zip(dims, values):
        dim.ClearField("dim_param")
        dim.dim_value = value


def frames(seconds):
    """Frames the feature extractor makes from `seconds` of 16 kHz audio."""
    return seconds * FRAMES_PER_S - 1


def tokens(n):
    """Soft tokens the encoder makes from n frames: two stride-2 subsamplings."""
    return -(-(-(-n // 2)) // 2)


def compress_int8(graph):
    """Each MatMul weight to int8 with a per-output-channel scale. Returns how many."""
    init = {i.name: i for i in graph.initializer}
    done = 0
    for node in list(graph.node):
        if node.op_type != "MatMul" or node.input[1] not in init:
            continue
        tensor = init.pop(node.input[1])
        w = numpy_helper.to_array(tensor)
        if w.ndim != 2 or w.dtype != np.float32:
            continue
        scale = np.abs(w).max(axis=0) / 127
        scale[scale == 0] = 1
        q = np.clip(np.rint(w / scale), -128, 127).astype(np.int8)
        base = node.input[1]
        graph.initializer.remove(tensor)
        graph.initializer.extend([
            numpy_helper.from_array(q, f"{base}_quantized"),
            numpy_helper.from_array(scale.astype(np.float32), f"{base}_scale"),
        ])
        graph.node.append(helper.make_node(
            "DequantizeLinear", [f"{base}_quantized", f"{base}_scale"], [f"{base}_dequantized"],
            name=f"{base}_DequantizeLinear", axis=-1,
        ))
        node.input[1] = f"{base}_dequantized"
        done += 1
    return done


def main():
    src = Path(sys.argv[1]) / "onnx" / "audio_encoder.onnx"
    seconds = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    int8 = len(sys.argv) > 3 and sys.argv[3] == "int8"
    n, cells = frames(seconds), tokens(frames(seconds))
    model = onnx.load(str(src), load_external_data=int8)
    graph = model.graph
    for inp, dims in zip(graph.input, ((1, n, MELS), (1, n))):
        static(inp.type.tensor_type.shape.dim, dims)

    gather = next(n for n in graph.node if n.op_type == "GatherND" and "audio_features" in n.output)
    hidden, indices = gather.input
    transpose = next(n for n in graph.node if indices in n.output)
    nonzero = next(n for n in graph.node if transpose.input[0] in n.output)
    for node in (gather, transpose, nonzero):
        graph.node.remove(node)
    graph.initializer.extend([
        numpy_helper.from_array(np.array([-1, OUT], np.int64), "static_rows_shape"),
        numpy_helper.from_array(np.array([-1], np.int64), "static_flat"),
    ])
    graph.node.extend([
        helper.make_node("Reshape", [hidden, "static_rows_shape"], ["audio_features"], name="static_rows"),
        helper.make_node("Reshape", [nonzero.input[0], "static_flat"], ["audio_mask"], name="static_mask"),
    ])
    static(graph.output[0].type.tensor_type.shape.dim, (cells, OUT))
    graph.output.append(helper.make_tensor_value_info("audio_mask", TensorProto.BOOL, [cells]))

    compressed = compress_int8(graph) if int8 else 0

    # The new nodes were appended; ONNX wants producers before consumers.
    pending, ready = list(graph.node), {""}
    ready |= {i.name for i in graph.input} | {i.name for i in graph.initializer}
    del graph.node[:]
    while pending:
        node = next(n for n in pending if all(i in ready for i in n.input))
        graph.node.append(node)
        ready.update(node.output)
        pending.remove(node)

    name = f"audio_encoder_static_{seconds}s"
    if int8:
        dst = src.parent / f"{name}_int8.onnx"
        onnx.save(model, str(dst), save_as_external_data=True, location=f"{name}_int8.onnx_data")
    else:
        dst = src.parent / f"{name}.onnx"
        onnx.save(model, str(dst))
    print(f"{dst}: {n} frames in, {cells} tokens out, {compressed} weights int8")


if __name__ == "__main__":
    main()
