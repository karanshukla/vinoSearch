"""Turn the shipped text model back into one that takes an image, with static shapes.

    uv run --python 3.13 --no-project --with onnx --with numpy bench/static_image_text.py <dir with onnx/model_text.onnx> [soft tokens]

text_only.py baked image_features into model_text.onnx as an empty constant.
This makes it an input again, fixed at [tokens, 512] so the static vision
encoder's output (static_vision.py) feeds it unsliced: the model merges the
first N rows, N being the image tokens in input_ids, and ignores the rest.
input_ids and attention_mask are fixed at the image's longest prompt, tokens + 4
(<bos> <boi> ... <eoi> <eos>), rounded up to 16; shorter prompts are padded at
the end with attention_mask 0. Padding with extra image tokens instead is not
neutral (cosine 0.9993 to unpadded): they are attended even when masked.
Writes onnx/model_image_<tokens>.onnx, sharing model.onnx_data.
"""

import sys
from pathlib import Path

import onnx
from onnx import TensorProto, helper

WIDTH = 512


def main():
    src = Path(sys.argv[1]) / "onnx" / "model_text.onnx"
    tokens = int(sys.argv[2]) if len(sys.argv) > 2 else 280
    length = -(-(tokens + 4) // 16) * 16
    model = onnx.load(str(src), load_external_data=False)
    graph = model.graph
    baked = next(i for i in graph.initializer if i.name == "image_features")
    graph.initializer.remove(baked)
    for inp in graph.input:
        dims = inp.type.tensor_type.shape.dim
        for dim, value in zip(dims, (1, length)):
            dim.ClearField("dim_param")
            dim.dim_value = value
    graph.input.append(helper.make_tensor_value_info("image_features", TensorProto.FLOAT, [tokens, WIDTH]))
    dst = src.parent / f"model_image_{tokens}.onnx"
    onnx.save(model, str(dst))
    print(f"{dst}: {[(i.name, [d.dim_value for d in i.type.tensor_type.shape.dim]) for i in graph.input]}")


if __name__ == "__main__":
    main()
