---
title: Performance and models
nav_order: 6
---

# Performance and models

## Performance

Measured on `~/Development` on a Dell XPS 13 (Core 5 320, Wildcat Lake NPU, driver 1.35.0, OpenVINO 2026.4.1), running on battery on 2026-10-18. Roughly 80% of this directory consists of two copies of a kernel driver, the KWin source tree, and generated coverage reports.

| Metric | Result |
|---|---|
| Index size | 9,763 files, 113,341 chunks, 470 MB on disk |
| Indexing speed | 66 min, 28.6 chunks/s, 3% of one CPU core (137 s CPU time), peak RSS 1.07 GB |
| Indexing power | ~9.2 W total system power vs. 4.5 W idle (~5.2 Wh total, roughly 10% of the battery) |
| First model compile | 4.5 s with an empty cache; cached on subsequent runs |
| Cold query (D-Bus activation, model load, vectors, query) | 1.7 s |
| Warm query through KRunner | p50 26 ms, p95 46 ms |
| Runner memory | 645 MB with model loaded; 12 MB after unloading following 5 idle minutes |

Images take 1.3 s and ~4.4 J each on the NPU, using CPU time only for image decoding.

These numbers come from a single machine; your throughput and latency will vary with your NPU and file corpus. Run `vinosearch estimate <folder>` to estimate indexing time for a folder before starting.

## Text

Text embeddings use [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2), packaged as [arankays/embeddinggemma-2-text-int8-cw-onnx](https://huggingface.co/arankays/embeddinggemma-2-text-int8-cw-onnx) (INT8, 274 MB) with a fix for an OpenVINO conversion bug ([#1](https://github.com/karanshukla/vinoSearch/issues/1)). On BEIR SciFact it scores **0.846 nDCG@10**, compared to 0.582 for the upstream ONNX export and 0.684 for `bge-small`. See the [full 12-model comparison](how-it-works.md#text-model-benchmark).

## Images

Image search uses EmbeddingGemma 2's vision encoder rewritten with static tensor shapes so the NPU can compile it, published as [arankays/embeddinggemma-2-vision-static-onnx](https://huggingface.co/arankays/embeddinggemma-2-vision-static-onnx) (fp32, 671 MB, downloaded only when image indexing is enabled). Measured on Flickr30k's 1k test split (1,000 images, 5,000 captions) with both stages running on the NPU (`bench/image_retrieval.py`, `bench/image_energy.py`):

| Soft tokens | Text→image R@1 / R@5 / R@10 | ms/image | Package J/image |
|---|---|---|---|
| 280 | 86.3 / 97.5 / 98.7 | 2767 | 9.4 |
| **140 (used)** | **84.5 / 96.9 / 98.6** | **1311** | **4.4** |
| 70 | 81.1 / 95.4 / 97.9 | 653 | 2.2 |

## OCR

Text inside images and scanned PDFs is read by PaddleOCR's PP-OCRv6 small detector and recognizer (matching RapidOCR's published weights) at [arankays/pp-ocrv6-small-onnx](https://huggingface.co/arankays/pp-ocrv6-small-onnx) (31 MB), compiled with static shapes so both stages run on the NPU ([#106](https://github.com/karanshukla/vinoSearch/issues/106)). On 1920×1080 browser screenshots it detects 96–98% of visible words in ~0.5 s, compared to 86–94% in 1.4–2.1 s for RapidOCR's CPU pipeline (`bench/ocr_static.py`). It supports Chinese, Japanese, English, and Latin-script languages; it does not currently read Cyrillic, Arabic, or Korean.

The models are licensed under Apache 2.0: EmbeddingGemma by Google DeepMind and PP-OCRv6 by Baidu's PaddleOCR.
