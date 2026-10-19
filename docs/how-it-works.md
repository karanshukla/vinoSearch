# How vinoSearch works

The details behind the [README](../README.md): what gets indexed, how the index stays current, what the runner does, and what we learned about the NPU along the way.

## What gets indexed

**Text.** Markdown, plain text and source code, up to 1 MiB each, split into ~900-character chunks. Config formats like TOML, YAML and JSON are left out on purpose, since that's where credentials live. Hidden directories, build output, vendored dependencies and generated reports (`target`, `node_modules`, `vendor`, `obj`, `coverage`, ...) are skipped, as are minified `*.min.js`/`*.min.css` and `*.bundle.js` files.

**Images** (off by default). Embedded from their pixels with EmbeddingGemma 2's vision encoder, both stages on the NPU: 1.3 s an image, about 4.4 J on the package, up to 64 MiB a file, with no CPU time beyond decoding. Text queries search them directly. KRunner shows them in a **vinoSearch Images** group, apart from **vinoSearch Text**; GNOME lists them after the text results. Each result carries its file type's icon. Turning images on queues a check of every root for the images already there (run `vinosearch fetch-model` first, or Download on the Model page), and turning them off drops them from the index. Images score lower against a query than text does, hence their own `min_score`.

**Excluded by default.** Without an `exclude` line in the config, `~/snap`, `~/go/pkg`, `~/sdk`, `~/Android/Sdk`, `~/R`, `~/perl5` and `~/Games` are skipped: trees of installed packages that take long to index and are rarely searched. Conda installs (`miniconda3`, `anaconda3`, ...) are skipped wherever they are. Adding an exclusion drops anything already indexed under it, and `vinosearch index` refuses an excluded path.

A running runner applies changes to `exclude`, `[formats]` and the device within 30 seconds, unless a flag or environment variable sets that one (see [dbus.md](dbus.md#settings)).

## Keeping the index current

Indexed roots are watched with inotify, one non-recursive watch per directory indexing would visit, so `target/`, `node_modules/` and hidden trees cost nothing (a whole home directory here is ~22,600 watches against the 135,178 limit). Changes are indexed after 2 quiet seconds; new roots from `vinosearch index` are picked up within 30 seconds.

inotify can still miss changes: the kernel drops events when its queue overflows, and directories past the watch limit go unwatched. So every root is also compared with the index by mtime and size once an hour, after the machine wakes from sleep, right after dropped events, and on demand from the settings window. That reads no file contents: 0.5 s for 45,418 paths under a home directory here.

## The runner

The runner runs as `vinosearch.service` (logs: `journalctl --user -u vinosearch`), started at login so the watcher is always running. Each time it starts it compares every indexed root against the index by mtime and size, without loading the model (under 0.1 s for 2,600 files here), and indexes whatever changed while nothing was watching. Without systemd, D-Bus starts it on the first query instead.

It frees the model after 5 idle minutes (`--idle-unload-minutes`, or `VINOSEARCH_IDLE_UNLOAD_MINUTES`; 0 keeps it loaded): 553 MB loaded with a 23k-chunk index and no load spike (it compiles from a static-shape graph, so a cached model is loaded without reading the weights), 12 MB unloaded, ~1 s to reload.

It also serves `dev.vinosearch.Control1` at `/control` for the settings app: `Status` (model loaded, indexing, device, file/chunk counts, roots), `Reindex(path)`, `Forget(path)` and `Unload`. Status answers while the worker is busy (4 ms mid-index). The full interface is in [dbus.md](dbus.md).

`vinosearch krunner --uninstall` stops the service and removes the KRunner, D-Bus and systemd files.

## The settings window

`vinosearch-settings` is egui, in the same style as vinoWhisper's settings (Inter, flat surfaces, the vino green). `--page index|folders|search|model|hardware|integration` opens a given page.

| Page | Shows | Lets you |
|---|---|---|
| Index | Whether it is indexing and how many files it has done, files, chunks and index size on disk, when a change was last indexed and when the folders were last checked for missed changes, the largest folders inside the indexed ones and their share of the index | Check now, exclude one of the largest folders in one click |
| Folders | The included (indexed) folders and the excluded ones | Add a folder (the desktop's own folder chooser, or a typed path), re-index or remove one, add or remove exclusions |
| Search | What is indexed (text always, images by a switch, with their file extensions) and how close a result must be to show, per kind | Turn images on or off, edit their extensions, slide each kind between fewer, closer and more, looser results |
| Model | Whether the model is downloaded and its size, download progress, whether it is loaded and on which device, the compiled-model cache size | Download, verify or delete the model, unload it now, clear the cache, pick NPU, GPU or CPU, set the idle unload time |
| Hardware | What `vinosearch doctor` reports: NPU, driver, NPU compiler, OpenVINO, devices, and a probe run on the device picked on the Model page | Check again |
| Integration | Whether KRunner and GNOME search are on | Turn them on or off (`vinosearch krunner --install` / `--uninstall`), with the command's output |

- **Live.** It asks the runner for `Status` once a second, from a background thread, so the counts move while indexing runs and a slow D-Bus start never freezes the window. If the runner isn't installed, the pages say so.
- **Settings go to `config.toml`.** Exclusions, device and idle unload are written straight to the file, keeping your comments and layout. A file that doesn't parse is left alone and the window says why.
- **Destructive actions ask twice.** Removing a folder drops its part of the index, and deleting the model stops search until it is downloaded again, so both need a second click.
- **Fits the desktop.** It follows the light or dark preference from the desktop portal, and on Plasma its header takes the colour scheme's header colours, so it runs into the title bar like Dolphin's toolbar. Below 620 px wide the sidebar folds into tabs.
- **Light.** It never loads OpenVINO or the model: it only depends on `vinosearch-shared`, eframe and zbus.

**Largest folders** lists up to 8 folders inside the indexed ones, biggest first, none inside another. A folder gives way to a subfolder holding three quarters of it, so a copied source tree shows as the part that is its bulk. It refreshes once the index changes and indexing settles.

It is Wayland only for now, as vinoWhisper's window is. Clicking vinoSearch's icon in GNOME Shell's search results opens it.

## Query latency

Warm queries started at p50 85 ms on the benchmark index. Fixed, in order of what they saved: an O(n) `COUNT(*)` on every query, a dot product that did not vectorize, a full sort for the top 40, f32 vectors (now int8 in memory: the top result scores at most 0.001 below f32's), and a 64-token query shape (now 32). `bench/query_latency.sh` measures through the installed runner.

## Runtime

OpenVINO comes from [ovfetch](https://github.com/karanshukla/ovfetch) (0.5.0 or newer), installed per user into `~/.local/lib/vinosearch/openvino`:

```bash
scripts/install-runtime.sh   # Intel's openvino wheel alone: 139 MB, ~17 s, no compile
vinosearch doctor            # NPU, driver, runtime, then compiles and runs a probe model
```

vinoSearch calls OpenVINO directly, without ONNX Runtime, so it uses `ovfetch install --openvino-only`. That gets the newest OpenVINO the NPU driver supports (2026.4.1 here), the same release vinoAuthFace and vinoWhisper run, which matters: an NPU fault resets the device for every process using it. `doctor --openvino-dir <prefix>` points at another ovfetch install. The runtime is loaded at startup (`openvino` crate, `runtime-linking`), so nothing links against the prefix and nothing needs an rpath.

ovfetch's runtime ships OpenVINO's ONNX frontend only, so models must be ONNX.

## Text model benchmark

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

## NPU gotchas

Found along the way, on OpenVINO 2026.4.1 and NPU driver 1.35.0:

- A static batch above 1 mixes rows across the batch. Batch 1 with requests in flight is correct and about 6x faster.
- `com.microsoft.RotaryEmbedding` with offset-form `position_ids` converts wrong ([#1](https://github.com/karanshukla/vinoSearch/issues/1)); `bench/patch_rope.py` rewrites it.
- ONNX external data reached through the Hugging Face cache's symlinks is refused; download into a plain directory.
- The vision encoder's published graph has a data-dependent output size (`NonZero` into `GatherND`), which the NPU can't compile. [arankays/embeddinggemma-2-vision-static-onnx](https://huggingface.co/arankays/embeddinggemma-2-vision-static-onnx) is the static-shape rewrite. Its INT8 version runs on the NPU but is wrong (cosine 0.67 against the reference), so it ships fp32.

## Demos

The animations in the README (`docs/media/krunner.svg`, `settings.svg`) are drawn, not recorded. File names, snippets and counts in them are made up. The photo is by Rafaëlla Waasdorp, on [Unsplash](https://unsplash.com/license).
