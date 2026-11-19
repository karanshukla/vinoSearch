---
title: Home
layout: home
nav_order: 1
permalink: /
---

# vinoSearch

**Search your local files by content from KRunner or GNOME Shell.** Every model runs locally on your Intel NPU so your files and queries stay on your machine.

<img src="media/krunner.svg" width="800" alt="KRunner opens with Alt+Space. Typing &quot;how do I rotate the signing key&quot; lists signing-keys.md, release-checklist.md, gpg.md and deploy.sh under vinoSearch Text, most of which never use the word rotate. Then &quot;dog watching the sunset&quot; lists IMG_2041.jpg and three other camera-named photos under vinoSearch Images, and Enter opens a photo of a dog silhouetted against a sunset.">

vinoSearch locates files by what is inside them rather than relying only on filenames. You can find `signing-keys.md` by asking how to rotate a signing key, or find `IMG_2041.jpg` by searching for "dog watching the sunset" based on the image contents.

- **Text, documents, code, and images in one search box.** Indexes Markdown and plain text by default, with optional support for photos, documents, and source code. Prefix queries with `code:` to search source code separately without cluttering document results.
- **Intel NPU acceleration.** Indexing runs at ~28.6 chunks/s while using ~3% of one CPU core, and warm queries complete in 26 ms (p50).
- **Desktop and CLI integration.** Integrates directly with KRunner and GNOME Shell's overview search, plus a full CLI and dmenu launcher support.
- **Automatic updates.** `inotify` detects file changes within seconds, and an hourly scan catches anything missed.
- **Local and private.** The only network requests download the runtime and models, and every model file is verified against a pinned SHA-256 checksum.

> Note: vinoSearch is under active early development and has primarily been tested on a single daily-driver laptop.

## Start here

You need Linux with an Intel NPU (Core Ultra or newer) and the [Intel Linux NPU driver](https://github.com/intel/linux-npu-driver). If no NPU is available, `install.sh` falls back to CPU execution at lower speed.

```bash
curl -L https://github.com/karanshukla/vinoSearch/releases/latest/download/vinosearch-x86_64-linux.tar.gz | tar xz
vinosearch/scripts/install.sh --index ~/Documents
```

See [Installing](install.md) for what the installer does, distribution notes, building from source, and uninstallation. See [Using it](using.md) for KRunner, GNOME Shell, other Wayland desktops, and CLI usage.
