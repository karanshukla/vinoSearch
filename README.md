# vinoSearch

Semantic file search for Linux, run on Intel NPUs through OpenVINO. The goal is a background daemon that indexes your files and answers queries from KRunner and GNOME Shell search, with every model on the NPU.

Early days: a working proof of concept for text files, searched from the command line or KRunner.

## Usage

```bash
git clone https://github.com/karanshukla/vinoSearch && cd vinoSearch
cargo install --path .                     # vinosearch into ~/.cargo/bin
cargo install ovfetch                      # 0.5.0 or newer
scripts/install-runtime.sh                 # OpenVINO via ovfetch, see Runtime
vinosearch fetch-model                     # 274 MB, every file checked against a pinned SHA-256
vinosearch index ~/Documents ~/Development # once per root; unchanged files are skipped on re-runs
vinosearch watch                           # keep indexed roots current (the KRunner runner also does this)
vinosearch search "how do I rotate the signing key"
vinosearch krunner --install               # register with KRunner, and as a systemd user service started at login
systemctl --user restart plasma-krunner.service
```

Text files only for now (Markdown, plain text, source code; not config formats like TOML, YAML or JSON, where credentials live), up to 1 MiB each, split into ~900-character chunks. Hidden directories, build output, vendored dependencies and generated reports (`target`, `node_modules`, `vendor`, `obj`, `coverage`, ...) are skipped, as are minified `*.min.js`/`*.min.css` and `*.bundle.js` files. Indexing runs at ~28.6 chunks/s on the NPU (see [Performance](#performance)).

Settings live in `~/.config/vinosearch/config.toml`; flags and environment variables override it:

```toml
exclude = ["~/Downloads"]   # never indexed or watched, even under an indexed root
device = "NPU"
idle_unload_minutes = 5
```

Adding an exclusion drops anything already indexed under it, and a running runner picks the change up within 30 seconds. `vinosearch index` refuses an excluded path.

Indexed roots are watched with inotify, one non-recursive watch per directory indexing would visit, so `target/`, `node_modules/` and hidden trees cost nothing (a whole home directory here is ~22,600 watches against the 135,178 limit). Changes are indexed after 2 quiet seconds; new roots from `vinosearch index` are picked up within 30 seconds.

The runner frees the model after 5 idle minutes (`--idle-unload-minutes`, or `VINOSEARCH_IDLE_UNLOAD_MINUTES`; 0 keeps it loaded): 553 MB loaded with a 23k-chunk index and no load spike (it compiles from a static-shape graph, so a cached model is loaded without reading the weights), 12 MB unloaded, ~1 s to reload. `vinosearch krunner --uninstall` stops the service and removes the KRunner, D-Bus and systemd files.

The runner runs as `vinosearch.service` (logs: `journalctl --user -u vinosearch`), started at login so the watcher is always running. Each time it starts it compares every indexed root against the index by mtime and size, without loading the model (under 0.1 s for 2,600 files here), and indexes whatever changed while nothing was watching. Without systemd, D-Bus starts it on the first query instead.

The runner also serves `dev.vinosearch.Control1` at `/control` for the settings app: `Status` (model loaded, indexing, device, file/chunk counts, roots), `Reindex(path)`, `Forget(path)` and `Unload`. Status answers while the worker is busy (4 ms mid-index).

The index is plaintext SQLite in `~/.local/share/vinosearch/index.sqlite`, readable only by you (0600, directory 0700). Embeddings can be partly inverted back to their text, so treat it like the files it indexes; encryption at rest is [#4](https://github.com/karanshukla/vinoSearch/issues/4).

## Performance

`~/Development` on a Dell XPS 13 (Core 5 320, Wildcat Lake NPU, driver 1.35.0, OpenVINO 2026.4.1), on battery, 2026-10-18. A deliberately messy tree: two copies of a kernel driver, the KWin source and generated coverage reports are 80% of it.

| | |
|---|---|
| Index | 9,763 files, 113,341 chunks, 470 MB on disk |
| Indexing | 66 min, 28.6 chunks/s, 3% of one core (137 s CPU), peak RSS 1.07 GB |
| Indexing power | ~9.2 W system vs 4.5 W idle: ~5.2 Wh for the whole index, about 10% of the battery |
| First model compile | 4.5 s with an empty cache; cached after |
| Cold query (D-Bus activation, model, vectors, query) | 1.7 s |
| Warm query through KRunner's D-Bus call | p50 26 ms, p95 46 ms (query embedding ~12 ms, search ~11 ms) |
| Runner memory, model loaded | 645 MB |

Warm queries started at p50 85 ms on this index. Fixed, in order of what they saved: an O(n) `COUNT(*)` on every query, a dot product that did not vectorize, a full sort for the top 40, f32 vectors (now int8 in memory: the top result scores at most 0.001 below f32's), and a 64-token query shape (now 32). `bench/query_latency.sh` measures through the installed runner.

## Runtime

OpenVINO comes from [ovfetch](https://github.com/karanshukla/ovfetch) (0.5.0 or newer), installed per user into `~/.local/lib/vinosearch/openvino`:

```bash
scripts/install-runtime.sh   # Intel's openvino wheel alone: 139 MB, ~17 s, no compile
vinosearch doctor            # NPU, driver, runtime, then compiles and runs a probe model
```

vinoSearch calls OpenVINO directly, without ONNX Runtime, so it uses `ovfetch install --openvino-only`. That gets the newest OpenVINO the NPU driver supports (2026.4.1 here), the same release vinoAuthFace and vinoWhisper run, which matters: an NPU fault resets the device for every process using it. `doctor --openvino-dir <prefix>` points at another ovfetch install. The runtime is loaded at startup (`openvino` crate, `runtime-linking`), so nothing links against the prefix and nothing needs an rpath.

ovfetch's runtime ships OpenVINO's ONNX frontend only, so models must be ONNX.

## Model

[EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2), text model, as [arankays/embeddinggemma-2-text-int8-cw-onnx](https://huggingface.co/arankays/embeddinggemma-2-text-int8-cw-onnx): INT8, 274 MB, with a fix for an OpenVINO conversion bug ([#1](https://github.com/karanshukla/vinoSearch/issues/1)).

From `bench/`, on BEIR SciFact (nDCG@10, documents truncated to 256 tokens), Core 5 320 NPU, OpenVINO 2026.4.1, 2026-10-18:

| Model | nDCG@10 | NPU query | NPU docs/s |
|---|---|---|---|
| **EmbeddingGemma 2, rotary fix, INT8** | **0.846** | 7.0 ms | 29.5 |
| EmbeddingGemma 2, rotary fix, fp32 | 0.846 | 9.8 ms | 27.6 |
| EmbeddingGemma 2, rotary fix, 80% INT4 | 0.817 | 5.7 ms | 29.2 |
| EmbeddingGemma 300m (v1) | 0.766 | 7.3 ms | 46 |
| bge-small-en-v1.5 | 0.684 | 1.9 ms | 158 |
| nomic-embed-text-v1.5 | 0.680 | 5.9 ms | 55 |
| arctic-embed-s | 0.665 | 2.0 ms | 155 |
| bge-micro-v2 | 0.652 | 0.8 ms | 476 |
| multilingual-e5-small | 0.644 | 2.0 ms | 152 |
| all-MiniLM-L6-v2 | 0.640 | 1.1 ms | 275 |
| arctic-embed-xs | 0.612 | 1.1 ms | 284 |
| EmbeddingGemma 2, as published by onnx-community | 0.582 | 10.1 ms | 27 |

The NPU indexes 4.5 to 11x faster than the CPU on every model, at roughly half the power. Raw numbers are in `bench/results/`.

NPU gotchas found along the way (OpenVINO 2026.4.1, NPU driver 1.35.0):

- A static batch above 1 mixes rows across the batch. Batch 1 with requests in flight is correct and about 6x faster.
- `com.microsoft.RotaryEmbedding` with offset-form `position_ids` converts wrong ([#1](https://github.com/karanshukla/vinoSearch/issues/1)); `bench/patch_rope.py` rewrites it.
- ONNX external data reached through the Hugging Face cache's symlinks is refused; download into a plain directory.

## License

MIT. The model is Apache 2.0, by Google DeepMind.
