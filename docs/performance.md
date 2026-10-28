---
title: Performance and models
nav_order: 6
---

# Performance and models

## Performance

My own `~/Development` on a Dell XPS 13 (Core 5 320, Wildcat Lake NPU, driver 1.35.0, OpenVINO
2026.4.1), on battery, 2026-10-18. It's a deliberately messy tree: two copies of a kernel driver, the
KWin source and generated coverage reports make up 80% of it.

| | |
|---|---|
| Index | 9,763 files, 113,341 chunks, 470 MB on disk |
| Indexing | 66 min, 28.6 chunks/s, 3% of one core (137 s CPU), peak RSS 1.07 GB |
| Indexing power | ~9.2 W system vs 4.5 W idle: ~5.2 Wh for the whole index, about 10% of the battery |
| First model compile | 4.5 s with an empty cache; cached after |
| Cold query (D-Bus activation, model, vectors, query) | 1.7 s |
| Warm query through KRunner | p50 26 ms, p95 46 ms |
| Runner memory, model loaded | 645 MB; 12 MB once it unloads after 5 idle minutes |

Images take 1.3 s and about 4.4 J each on the NPU, with no CPU time beyond decoding.

n=1 machine. Your NPU and your files will differ. `vinosearch estimate <folder>` says how long yours
would take before you commit to it.

## Text

[EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2) as
[arankays/embeddinggemma-2-text-int8-cw-onnx](https://huggingface.co/arankays/embeddinggemma-2-text-int8-cw-onnx):
INT8, 274 MB, with a fix for an OpenVINO conversion bug
([#1](https://github.com/karanshukla/vinoSearch/issues/1)). On BEIR SciFact it scores **0.846
nDCG@10**, against 0.582 for the published ONNX export and 0.684 for bge-small. The
[full table](how-it-works.md#text-model-benchmark) has 12 models.

## Images

Its vision encoder, rewritten with static shapes so the NPU can compile it, as
[arankays/embeddinggemma-2-vision-static-onnx](https://huggingface.co/arankays/embeddinggemma-2-vision-static-onnx):
fp32, 671 MB, downloaded only once images are on. On Flickr30k's 1k test split (1,000 images, 5,000
captions), both stages on the NPU (`bench/image_retrieval.py`, `bench/image_energy.py`):

| Soft tokens | Text→image R@1 / R@5 / R@10 | ms/image | Package J/image |
|---|---|---|---|
| 280 | 86.3 / 97.5 / 98.7 | 2767 | 9.4 |
| **140 (used)** | **84.5 / 96.9 / 98.6** | **1311** | **4.4** |
| 70 | 81.1 / 95.4 / 97.9 | 653 | 2.2 |

## OCR

Text in images and scanned PDFs is read by PaddleOCR's PP-OCRv6 small detector and recogniser, as
RapidOCR publishes them, at [arankays/pp-ocrv6-small-onnx](https://huggingface.co/arankays/pp-ocrv6-small-onnx)
(31 MB), compiled at static shapes so the NPU runs both
([#106](https://github.com/karanshukla/vinoSearch/issues/106)). On 1920x1080 browser screenshots it
finds 96-98% of the words shown in about 0.5 s, where RapidOCR's own pipeline on the CPU finds
86-94% in 1.4-2.1 s (`bench/ocr_static.py`). It reads Chinese, Japanese, English and Latin-script
languages; not Cyrillic, Arabic or Korean.

The models are Apache 2.0: EmbeddingGemma by Google DeepMind, PP-OCRv6 by Baidu's PaddleOCR.
