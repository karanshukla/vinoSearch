---
title: Home
layout: home
nav_order: 1
permalink: /
---

# vinoSearch

**Search your files by what's in them, from KRunner or GNOME Shell.** Every model runs on your
Intel NPU, and nothing leaves your machine.

<img src="media/krunner.svg" width="800" alt="KRunner opens with Alt+Space. Typing &quot;how do I rotate the signing key&quot; lists signing-keys.md, release-checklist.md, gpg.md and deploy.sh under vinoSearch Text, most of which never use the word rotate. Then &quot;dog watching the sunset&quot; lists IMG_2041.jpg and three other camera-named photos under vinoSearch Images, and Enter opens a photo of a dog silhouetted against a sunset.">

Filename search finds `signing-keys.md` when you remember it's called that. vinoSearch finds it when
you remember what it says. It also finds `IMG_2041.jpg` from "dog watching the sunset", because it
reads the pixels, not the name.

- **Text and images**, searched from one box. Markdown and plain text, plus photos, documents and source code when you turn them on. Code is searched apart, with `code:` in front of the query, so it never crowds out your notes.
- **On the NPU.** Indexing runs at ~28.6 chunks/s and 3% of one core, so it doesn't fight you for the CPU. A warm query takes 26 ms (p50).
- **Where you already search.** KRunner and GNOME Shell's overview. A CLI too.
- **Stays current.** inotify picks up changes within seconds, and an hourly check catches anything it missed.
- **Private.** The only network calls download the model and the runtime, and each model file is checked against a pinned SHA-256.

> It's early days: a working proof of concept, used daily on one laptop. Expect rough edges.

## Start here

You need Linux with an Intel NPU (Core Ultra or newer) and the
[NPU driver](https://github.com/intel/linux-npu-driver); no NPU, `--device CPU` works, just slower.
Then:

```bash
curl -L https://github.com/karanshukla/vinoSearch/releases/latest/download/vinosearch-x86_64-linux.tar.gz | tar xz
vinosearch/scripts/install.sh --index ~/Documents
```

[Installing](install.md) covers what that does, other distros, building from source and removal.
[Using it](using.md) covers KRunner, GNOME, other Wayland desktops and the CLI.
