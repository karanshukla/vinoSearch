<p align="center"><img src="data/dev.vinosearch.svg" width="96" height="96" alt="vinoSearch icon: a magnifier on the dark vino tile"></p>

<h1 align="center">vinoSearch</h1>

<p align="center">Search your local files by content using natural language and keywords, from KRunner or GNOME Shell.</p>

<p align="center"><img src="docs/media/krunner.svg" width="800" alt="KRunner opens with Alt+Space. Typing &quot;how do I rotate the signing key&quot; lists signing-keys.md, release-checklist.md, gpg.md and deploy.sh under vinoSearch Text, most of which never use the word rotate. Then &quot;dog watching the sunset&quot; lists IMG_2041.jpg and three other camera-named photos under vinoSearch Images, and Enter opens a photo of a dog silhouetted against a sunset."></p>

vinoSearch finds files by their contents rather than just their filenames. You can locate `signing-keys.md` by asking how to rotate a signing key, or find `IMG_2041.jpg` by searching for "dog watching the sunset" based on the image contents.

- **Text, documents, code, and images in one search box.** Indexes Markdown and plain text by default, with optional support for photos, documents, audio/video, vinoWhisper sessions, and source code. Prefix a query with `code:` to search source code separately.
- **Intel NPU acceleration.** Indexing runs on the NPU at ~28.6 chunks/s while using only ~3% of one CPU core. Warm queries complete in 26 ms (p50).
- **Desktop and CLI integration.** Works directly inside KRunner and GNOME Shell's overview search, as well as from the command line and dmenu-style launchers.
- **Automatic index updates.** Watches indexed folders with `inotify` to pick up changes within seconds, backed by an hourly scan to catch any missed events.
- **Local and private.** Everything runs locally; the only network requests download the OpenVINO runtime and models, with every model file verified against a pinned SHA-256 hash.

## Requirements

- Linux with an Intel NPU (Core Ultra or newer) and the [Intel Linux NPU driver](https://github.com/intel/linux-npu-driver). If no NPU is available, vinoSearch falls back to the CPU at lower speed.
- KDE Plasma or GNOME on Wayland. Sway, Hyprland, COSMIC, and other Wayland desktops are supported through `vinosearch menu` with a dmenu-style launcher.
- `x86_64` with glibc 2.39 or newer for prebuilt releases (Fedora 40+, Ubuntu 24.04+, Debian 13, Arch, openSUSE Tumbleweed, Fedora Atomic, openSUSE Aeon). On older distributions, build [from source](docs/install.md#from-source).

## Install

```bash
curl -fsSL https://github.com/karanshukla/vinoSearch/releases/latest/download/install.sh | bash -s -- --index ~/Documents
```

This script installs the prebuilt binaries into `~/.local/bin`, downloads OpenVINO and the 340 MB base models, indexes the folders you specify (`--index` is repeatable, or the installer prompts for folders), and registers vinoSearch with your desktop's search. No `sudo` is required because everything installs under your home directory. If a prerequisite such as the NPU driver is missing, the installer stops and prints what is needed.

| Task | Action |
|---|---|
| Upgrade | Click **Check for updates** on the settings window's About page, or run `vinosearch-upgrade` |
| Uninstall | Run the same `curl` command with `--uninstall` instead of `--index ~/Documents` (keeps your index and settings; add `--purge` to delete the index and runtime as well) |
| Install via Flatpak | [Download the Flatpak bundle](docs/install.md#flatpak) from the latest release |
| Verify installation | Run `vinosearch doctor` |

See [Installing](docs/install.md) for building from source, distribution package commands, Fedora Atomic and openSUSE Aeon, Flatpak permissions, and uninstall details.

## Usage

- **KDE Plasma:** Press `Alt+Space` to search in KRunner.
- **GNOME:** Search from the GNOME Shell overview (active starting on the next login).
- **Sway, Hyprland, COSMIC, and other Wayland desktops:** Bind `vinosearch menu` to a shortcut; it prompts via `fuzzel`, `wofi`, `rofi`, `tofi`, or `bemenu`.
- **Terminal:** Run `vinosearch search "how do I rotate the signing key"`, `vinosearch search "code: retry a failed download"`, or `vinosearch similar ~/Pictures/IMG_2041.jpg`.

Queries require at least 3 characters. Pressing `Enter` opens the file directly at the match: a text file at the matching line, a PDF at the matching page, or an audio/video file at the matching timestamp. Choosing **Show all results** opens a full search window with up to 100 files per category, image thumbnails, and filters.

See [Using it](docs/using.md) for supported editors, viewers, and media players, the search window, key bindings, and CLI subcommands.

## Settings

<p align="center"><img src="docs/media/settings.svg" width="720" alt="The vinoSearch settings window on its Index page, indexing photos: the file count rises, the Pictures folder's share of the index grows, then it reads Up to date."></p>

Open **vinoSearch** from your application menu or run `vinosearch-settings`. From there you can view index statistics, manage indexed and excluded folders, enable optional file types (images, documents, code, vinoWhisper sessions, audio and video), adjust score thresholds, select your editor, media player, and inference device, and toggle KRunner or GNOME Shell search integration. Changes take effect within 30 seconds without restarting.

All settings are stored in `~/.config/vinosearch/config.toml`. See [Settings](docs/configuration.md) for the complete file format.

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

These numbers come from a single machine; your throughput and latency will vary with your NPU and file corpus.

See [Performance and models](docs/performance.md) for model details and benchmarks, including EmbeddingGemma 2 for text (0.846 nDCG@10 on BEIR SciFact) and images (84.5 R@1 on Flickr30k), and PP-OCRv6 for text inside images.

## Privacy

The index is stored in an SQLite database at `~/.local/share/vinosearch/index.sqlite` with permissions restricted to your user (`0600` file inside a `0700` directory). Because embeddings can be partially inverted back into source text, treat the index with the same sensitivity as the files it covers.

Optional encryption at rest is available using SQLCipher with the key sealed to your machine's TPM (disabled by default). See [Privacy](docs/privacy.md) for what encryption covers and its limitations.

## Documentation

- [vinosearch.vinopilot.ca](https://vinosearch.vinopilot.ca): hosted documentation site.
- [How it works](docs/how-it-works.md): indexing pipeline, file watching, runner lifecycle, encryption, runtime, and NPU notes.
- [D-Bus interface](docs/dbus.md): KRunner, GNOME Shell, and the control interface used by the settings window.
- [Releasing](docs/releasing.md): release workflow and versioning.
- [Comment style](docs/comment-style.md): contributor guide for test-pinned comments (``(pinned by `tests::name`)``, checked by `tests/doc_links.rs`).

## License

MIT. The models are licensed under Apache 2.0: EmbeddingGemma by Google DeepMind and PP-OCRv6 by Baidu's PaddleOCR.
