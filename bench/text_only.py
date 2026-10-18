"""Turn the media inputs of an EmbeddingGemma 2 text model into empty constants.

    uv run --no-project --with onnx bench/text_only.py <dir with onnx/model.onnx>

The text model takes image_features, video_features and audio_features
[N, 512] next to the tokens; text-only use feeds them with N = 0. OpenVINO's C
API refuses a zero dimension in a shape, so a Rust caller cannot build those
inputs. Baking them in as empty initializers leaves input_ids and
attention_mask as the only inputs. Writes onnx/model_text.onnx, sharing the
original's external data.
"""

import sys
from pathlib import Path

import numpy as np
import onnx
from onnx import numpy_helper

MEDIA = ("image_features", "video_features", "audio_features")


def main():
    src = Path(sys.argv[1]) / "onnx" / "model.onnx"
    model = onnx.load(str(src), load_external_data=False)
    graph = model.graph
    keep = []
    for inp in graph.input:
        if inp.name in MEDIA:
            width = inp.type.tensor_type.shape.dim[1].dim_value
            graph.initializer.append(numpy_helper.from_array(np.zeros((0, width), np.float32), inp.name))
        else:
            keep.append(inp)
    if len(keep) == len(graph.input):
        raise SystemExit(f"{src}: none of {MEDIA} are inputs")
    del graph.input[:]
    graph.input.extend(keep)
    dst = src.parent / "model_text.onnx"
    onnx.save(model, str(dst))
    print(f"{dst}: inputs {[i.name for i in graph.input]}")


if __name__ == "__main__":
    main()
