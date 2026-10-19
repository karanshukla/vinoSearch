<p align="center"><img src="data/dev.vinosearch.svg" width="96" height="96" alt="vinoSearch icon: a magnifier on the dark vino tile"></p>

# vinoSearch

Semantic file search for Linux, run on Intel NPUs through OpenVINO. The goal is a background daemon that indexes your files and answers queries from KRunner and GNOME Shell search, with every model on the NPU.

Early days: a working proof of concept for text files, searched from the command line, KRunner or GNOME Shell, with a settings window for the rest.

## Usage

```bash
git clone https://github.com/karanshukla/vinoSearch && cd vinoSearch
scripts/install.sh --index ~/Documents
```

`scripts/install.sh` does all of the below, skipping what's already done, so re-running it after a `git pull` upgrades in place. It needs Rust and a C compiler, and stops with one line at the first thing missing (including the NPU driver; `--device CPU` runs without an NPU). It indexes only the folders you name with `--index` (repeatable) or type when it asks. `scripts/install.sh --uninstall` removes the service, KRunner / GNOME files and binaries; add `--purge` to also delete the index, model, compiled cache and runtime.

Or step by step:

```bash
cargo install --path .                     # vinosearch into ~/.cargo/bin
cargo install --path crates/settings       # vinosearch-settings, the settings window (optional)
cargo install ovfetch                      # 0.5.0 or newer
scripts/install-runtime.sh                 # OpenVINO via ovfetch, see Runtime
vinosearch fetch-model                     # 308 MB, every file checked against a pinned SHA-256
vinosearch index ~/Documents ~/Development # once per root; unchanged files are skipped on re-runs
vinosearch watch                           # keep indexed roots current (the KRunner runner also does this)
vinosearch search "how do I rotate the signing key"
vinosearch krunner --install               # register with KRunner and GNOME Shell search, as a systemd user service started at login, and in the app menu
systemctl --user restart plasma-krunner.service
```

Text files only for now (Markdown, plain text, source code; not config formats like TOML, YAML or JSON, where credentials live), up to 1 MiB each, split into ~900-character chunks. Hidden directories, build output, vendored dependencies and generated reports (`target`, `node_modules`, `vendor`, `obj`, `coverage`, ...) are skipped, as are minified `*.min.js`/`*.min.css` and `*.bundle.js` files. Indexing runs at ~28.6 chunks/s on the NPU (see [Performance](#performance)).

Settings live in `~/.config/vinosearch/config.toml`; flags and environment variables override it:

```toml
exclude = ["~/Downloads"]   # never indexed or watched, even under an indexed root
device = "NPU"
idle_unload_minutes = 5
```

Without an `exclude` line, the default list applies: `~/snap`, `~/go/pkg`, `~/sdk`, `~/Android/Sdk`, `~/R`, `~/perl5` and `~/Games`, trees of installed packages that take long to index and are rarely searched. Conda installs (`miniconda3`, `anaconda3`, ...) are skipped wherever they are. Adding an exclusion drops anything already indexed under it. A running runner applies changes to any of the three within 30 seconds, unless a flag or environment variable sets that one (see [docs/dbus.md](docs/dbus.md#settings)). `vinosearch index` refuses an excluded path.

Indexed roots are watched with inotify, one non-recursive watch per directory indexing would visit, so `target/`, `node_modules/` and hidden trees cost nothing (a whole home directory here is ~22,600 watches against the 135,178 limit). Changes are indexed after 2 quiet seconds; new roots from `vinosearch index` are picked up within 30 seconds. inotify can still miss changes (the kernel drops events when its queue overflows, and directories past the watch limit go unwatched), so every root is also compared with the index by mtime and size once an hour, after the machine wakes from sleep, right after dropped events, and on demand from the settings window. That reads no file contents: 0.5 s for 45,418 paths under a home directory here.

The runner frees the model after 5 idle minutes (`--idle-unload-minutes`, or `VINOSEARCH_IDLE_UNLOAD_MINUTES`; 0 keeps it loaded): 553 MB loaded with a 23k-chunk index and no load spike (it compiles from a static-shape graph, so a cached model is loaded without reading the weights), 12 MB unloaded, ~1 s to reload. `vinosearch krunner --uninstall` stops the service and removes the KRunner, D-Bus and systemd files.

The runner runs as `vinosearch.service` (logs: `journalctl --user -u vinosearch`), started at login so the watcher is always running. Each time it starts it compares every indexed root against the index by mtime and size, without loading the model (under 0.1 s for 2,600 files here), and indexes whatever changed while nothing was watching. Without systemd, D-Bus starts it on the first query instead.

The runner also serves `dev.vinosearch.Control1` at `/control` for the settings app: `Status` (model loaded, indexing, device, file/chunk counts, roots), `Reindex(path)`, `Forget(path)` and `Unload`. Status answers while the worker is busy (4 ms mid-index).

The index is plaintext SQLite in `~/.local/share/vinosearch/index.sqlite`, readable only by you (0600, directory 0700). Embeddings can be partly inverted back to their text, so treat it like the files it indexes; encryption at rest is [#4](https://github.com/karanshukla/vinoSearch/issues/4).

## Settings

`vinosearch-settings` is the settings window: egui, in the same style as vinoWhisper's settings (Inter, flat surfaces, the vino green), with the icon above. Install it with `cargo install --path crates/settings`, then run `vinosearch krunner --install` once more so the **vinoSearch** entry in the app menu opens it. Or run `vinosearch-settings` directly; `--page index|folders|model|hardware|integration` opens a given page.

| Page | Shows | Lets you |
|---|---|---|
| Index | Whether it is indexing and how many files it has done, files, chunks and index size on disk, when a change was last indexed and when the folders were last checked for missed changes, the largest folders inside the indexed ones and their share of the index | Check now, exclude one of the largest folders in one click |
| Folders | The included (indexed) folders and the excluded ones | Add a folder (the desktop's own folder chooser, or a typed path), re-index or remove one, add or remove exclusions |
| Model | Whether the model is downloaded and its size, download progress, whether it is loaded and on which device, the compiled-model cache size | Download, verify or delete the model, unload it now, clear the cache, pick NPU, GPU or CPU, set the idle unload time |
| Hardware | What `vinosearch doctor` reports: NPU, driver, NPU compiler, OpenVINO, devices, and a probe run on the device picked on the Model page | Check again |
| Integration | Whether KRunner and GNOME search are on | Turn them on or off (`vinosearch krunner --install` / `--uninstall`), with the command's output |

How it behaves:

- **Live.** It asks the runner for `Status` over `dev.vinosearch.Control1` once a second, from a background thread, so the counts move while indexing runs and a slow D-Bus start never freezes the window. If the runner isn't installed, the pages say so.
- **Settings go to `config.toml`.** Exclusions, device and idle unload are written straight to the file, keeping your comments and layout, and the runner applies them within 30 seconds. A file that doesn't parse is left alone and the window says why.
- **Destructive actions ask twice.** Removing a folder drops its part of the index, and deleting the model stops search until it is downloaded again, so both need a second click.
- **Fits the desktop.** It follows the light or dark preference from the desktop portal, and on Plasma its header takes the colour scheme's header colours, so it runs into the title bar like Dolphin's toolbar. Below 620 px wide the sidebar folds into tabs.
- **Light.** It never loads OpenVINO or the model: it only depends on `vinosearch-shared`, eframe and zbus.

**Largest folders** lists up to 8 folders inside the indexed ones, biggest first, none inside another. A folder gives way to a subfolder holding three quarters of it, so a copied source tree shows as the part that is its bulk. It refreshes once the index changes and indexing settles.

It is Wayland only for now, as vinoWhisper's window is. Clicking vinoSearch's icon in GNOME Shell's search results opens it.

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
