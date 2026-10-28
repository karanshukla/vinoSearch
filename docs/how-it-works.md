---
title: How it works
nav_order: 7
---

# How vinoSearch works

The details behind the [README](https://github.com/karanshukla/vinoSearch#readme): what gets indexed, how the index stays current, what the runner does, and what we learned about the NPU along the way.

## What gets indexed

**Text.** Markdown, plain text and HTML, up to 1 MiB each (16 MiB for `.txt`, `.md`, `.rst`, `.org` and `.tex`, which may be books), split into ~900-character chunks. HTML pages are read without their tags, scripts, styles and comments, with every line kept where it was so a result still opens at the right line; pages indexed before that are indexed again once. Config formats like TOML, YAML and JSON are left out on purpose, since that's where credentials live. Hidden directories, build output, vendored dependencies and generated reports (`target`, `node_modules`, `vendor`, `obj`, `coverage`, ...) are skipped for every kind.

**Images** (off by default). Embedded from their pixels with EmbeddingGemma 2's vision encoder, both stages on the NPU: 1.3 s an image, about 4.4 J on the package, up to 64 MiB a file, with no CPU time beyond decoding. Text queries search them directly. KRunner shows them in a **vinoSearch Images** group, apart from **vinoSearch Text**; GNOME lists them after the text results. Each result carries its file type's icon. Turning images on queues a check of every root for the images already there (run `vinosearch fetch-model` first, or Download on the Model page), and turning them off drops them from the index. Images score lower against a query than text does, hence their own `min_score`. HEIC (iPhone photos) and AVIF have no decoder in the `image` crate, so they go through libheif's `heif-convert` when it is installed (`libheif-tools` on Fedora, `libheif-examples` on Debian and Ubuntu, `libheif` on Arch), converted to JPEG in a private temporary directory; without it they are recorded as skipped, the Index page says how many need the decoder, and installing it gets them indexed at the next start ([#79](https://github.com/karanshukla/vinoSearch/issues/79)). An image that doesn't decode at all keeps a row with no vector, so it isn't decoded again until it changes. The text an image shows is read too (see [OCR](#ocr)), so a screenshot of an error dialog or a photo of a receipt turns up for its words, in the Documents group, as well as for what it looks like.

`vinosearch similar <image>` lists the indexed images closest to one ([#77](https://github.com/karanshukla/vinoSearch/issues/77)): a dot product over the image vectors, with no encode for an image that is indexed and one vision encode (~1.3 s on the NPU) for one that isn't. Image to image cosines run far higher than text to image ones, so the command prints scores and takes `--min-score` rather than sharing `formats.images.min_score`; a floor of its own waits on measuring them.

**Documents** (off by default). PDF (text per page), Word, PowerPoint, Excel and OpenDocument (their macro and template variants and flat `.fodt`/`.fodp` too), Word and PowerPoint 97-2003 (`.doc`, `.ppt`), RTF, EPUB, SRT and WebVTT subtitles, CSV and TSV, by their extracted text, through the same model as text. Results say which page, slide, row, chapter or line they're on, in a **vinoSearch Documents** group. Spreadsheet and CSV chunks each start with the header row. A PDF with no text layer (a scan) is read by OCR page by page, when poppler's `pdfinfo` and `pdftoppm` are installed (`poppler-utils` on Fedora, Debian and Ubuntu, `poppler` on Arch); without them it is recorded with no text, and installing them gets it read at the next start. A long document is indexed in parts ([#108](https://github.com/karanshukla/vinoSearch/issues/108)): its first 30 pages (slides, chapters, or for formats without those, 30 runs of 3 chunks, about a page each) go in with everything else, and the rest 30 at a time once nothing else is waiting, so one book doesn't hold up a folder of notes. Parts past 5,000 and text past 16 MiB are left out. Partly indexed documents are listed on the settings window's Index page ("120 of 640 pages"); they pick up where they left off after a restart and start over if the file changes. Long text files are split the same way. These are where statements, tax forms and bank exports live, so their text sits in the index readable unless it's encrypted (see [Encryption at rest](#encryption-at-rest)); that's why they're off by default. Turning documents off drops them from the index. Each document is parsed in a child process (`vinosearch extract`, the runner re-running itself) with 1 GiB of address space, 30 CPU seconds and 2 minutes of wall time for each run of 30 parts, so a crafted or broken file costs that process and not the runner; a document whose child fails or is stopped is recorded without text and not tried again until it changes ([#84](https://github.com/karanshukla/vinoSearch/issues/84)).

**Code** (off by default, [#110](https://github.com/karanshukla/vinoSearch/issues/110)). Rust, Python, JavaScript and TypeScript, Go, C and C++, Java, Kotlin, shell, SQL, CSS and about 50 other extensions (`code::EXTENSIONS`), up to 1 MiB each. On a developer's home folder code is most of the index, so it's a kind of its own that normal searches never show: a query starting `code:` searches code and nothing else, in a **vinoSearch Code** group, and opens the file at the chunk's line. Its query goes in with EmbeddingGemma's code retrieval prompt (`task: code retrieval`) instead of the search one, which scores everything about 0.04 higher, hence code's own `min_score` (0.72). Inside a git work tree, files git ignores are left out (`.gitignore` at every level, `.ignore`, `.git/info/exclude` and the global excludes file, read with the `ignore` crate), as is anything under `third_party`, `third-party`, `3rdparty` or `vendored`. Only code: a `.gitignore` often names personal notes and drafts kept beside a project, which are exactly what text search is for, and a home folder kept in git with `*` in its `.gitignore` would ignore everything. Generated files are left out by name (`*.pb.go`, `*_pb2.py`, `*.d.ts`, `*.g.dart`, `*.designer.cs`, `*.generated.*`, minified and bundled JavaScript) and by a "do not edit" or `@generated` header, as are minified files without `.min` in the name, files named for secrets (`secrets.py`, `credentials.js`) and any file holding a private key (`-----BEGIN ... PRIVATE KEY-----`) or a token in a format that can't be anything else (AWS key ids, GitHub, GitLab, Slack, Stripe, OpenAI and Google API keys). Those read and refused keep a row with no chunks, so they aren't read again until they change. Code is chunked on its definitions rather than every ~900 characters (`code::chunk`): a unit starts at a line at the outer indentation after a blank line, or at a line starting a definition (`fn`, `def`, `class`, `func`, `function`, ... after any modifiers), with the comments, attributes and decorators right above it. Neighbouring short units share a chunk up to 900 characters; a longer one (a class, an `impl`, a long function) is split at its own units one level in, the later parts starting with the definition's line so they say what they're part of; only a run with no break at all is cut by length. On 7.7 MiB of Rust, Python, TypeScript, Go, C and shell (795 files), 98.4% of functions that fit in a chunk landed whole in one, against 69.7% with the text chunker and 99.1% with tree-sitter grammars, which would add 10.7 MB to the binary and 45 s of C compiling to a release build. On 12 queries against ovfetch's source, the first result opened inside the function asked for 7 times in 12 (mean reciprocal rank 0.75), against 4 (0.47) with the text chunker, whose bigger chunks more often hold the function somewhere but start well above it. Indexes from before this move their code over once, at open: chunks of code files are relabelled, not embedded again, and files whose stored text holds a key or token or is generated are dropped. They keep their old chunking until they change. Turning code off, the default, drops it from the index, as turning images off does.

**vinoWhisper sessions** (off by default, [#143](https://github.com/karanshukla/vinoSearch/issues/143)). A `.vwsession` is one session vinoWhisper captioned and saved ([vinoWhisper#123](https://github.com/karanshukla/vinoWhisper/issues/123)): a zip whose first entry, stored, is `mimetype` (`application/vnd.vinowhisper.session+zip`), holding `transcript.txt` (paragraphs starting `[MM:SS]`), `summary.md` once vinoWhisper has written one, and `attestation.json`, which says how each was made. The transcript and summary are both indexed, as chunks of the one file: the transcript's numbered by its line, the summary's from 2^32 up (`session::SUMMARY_LINES`), so a hit says which part it came from. They're searched with everything else, in a **vinoWhisper sessions** group whose subtitle names the part, the `[MM:SS]` of the paragraph that matched and the session's date and length ("Transcript at 01:12 · 2026-10-27 14:05, 43 min"; a summary the attestation marks as model-written says "Summary, AI-generated"). KRunner offers **Open summary** or **Open transcript** for the other part. Opening one hands it to vinoWhisper's settings window, `vinowhisper-gui --page sessions --open <bundle>#transcript@MM:SS` (or `#summary`); without vinoWhisper, the part is copied to a 0600 file under `$XDG_RUNTIME_DIR/vinosearch/sessions` (each open replaces the last) and opened as text, at the line for a transcript. vinoWhisper keeps them in `$XDG_DATA_HOME/vinowhisper/transcripts`, a hidden folder the walk of a home folder skips, so while sessions are on that folder is a root of its own; only `.vwsession` files are taken there (not the live `.txt` of a session still running, nor the `.tmp` a bundle is written to before it's renamed into place), and one anywhere else under a root (an exported copy) is taken too. Adding the summary rewrites the bundle, which is a changed file like any other. A bundle whose `attestation.json` names a `format` other than `vinowhisper-session/1` is refused rather than guessed at, recorded with no text, and counted on the settings window's Search page. A session is plain text in a zip, read in the runner rather than the child, as nothing in it needs a parser that could crash; to keep what a crafted bundle can unzip there small, the transcript and summary are each held to 1 MiB (about 20 hours of speech). A transcript holds whatever was said or played, and the index keeps its text, readable unless it's encrypted; that's why sessions are off by default and the switch says so.

**Excluded by default.** Without an `exclude` line in the config, `~/snap`, `~/go/pkg`, `~/sdk`, `~/Android/Sdk`, `~/R`, `~/perl5` and `~/Games` are skipped: trees of installed packages that take long to index and are rarely searched. Conda installs (`miniconda3`, `anaconda3`, ...) are skipped wherever they are. Adding an exclusion drops anything already indexed under it, and `vinosearch index` refuses an excluded path.

A running runner applies changes to `exclude`, `[formats]` and the device within 30 seconds, unless a flag or environment variable sets that one (see [dbus.md](dbus.md#settings)).

## Exact words

Embeddings don't keep exact strings: the vector for `INV-20431` looks like any other invoice's. So each chunk's text and its file's name also go into an FTS5 table in `index.sqlite`, which triggers on `chunks` keep in step, including chunks handed to a copy ([#104](https://github.com/karanshukla/vinoSearch/issues/104)). It's contentless, so the text isn't stored twice: it adds 10 to 13% to the file, since the vectors are most of it. An index from before it is filled once when it's opened (16 s for 113k chunks in a 2-vCPU VM).

Each query runs both lookups. A chunk holding every word of the query gets 0.05 added to its cosine: enough to reorder near ties and lift a chunk just under `min_score`, not enough to carry an unrelated chunk past a related one. A query that is a single identifier-like word (with a digit, inner punctuation or all capitals: `INV-20431`, `E0502`, `TODO`) or wholly in quotes puts its exact matches first instead, in bm25 order. Scores stay on the cosine scale, so `min_score` and the Text/Documents/Images groups work as before. A word in more than 2,000 chunks is left out of the lookup: it's no exact string, and ranking it costs more than the vectors (250 ms for "the" on 113k chunks). With that, the word lookup adds 0 to 5 ms to a query on 113k chunks. Every word is quoted before it reaches FTS5, so `AND`, `NEAR`, `*` and `:` are plain words.

The tokenizer is `unicode61`, which splits on spaces and punctuation. Chinese, Japanese and Thai have no spaces, so a run of them is one token, and only a query matching the whole run finds it.

## OCR

Images and scanned PDF pages are read with PaddleOCR's PP-OCRv6 small, a text detector and a recogniser, from [arankays/pp-ocrv6-small-onnx](https://huggingface.co/arankays/pp-ocrv6-small-onnx) (RapidOCR 3.10.0's ONNX byte for byte, 31 MB, fetched with the text model) ([#106](https://github.com/karanshukla/vinoSearch/issues/106)). Both graphs take dynamic shapes, which the NPU can't compile, so `src/ocr.rs` compiles five static ones: the detector at 1088x1920 and 1920x1088, the recogniser at 48x320, 48x640 and 48x1280. An image is scaled, aspect kept, into the detector shape that matches its orientation (up to 4x for a small one) and padded with black. Each region of the detector's map is a line: turned to its principal axis when it's elongated, grown by DB's unclip distance, sampled out bilinearly, and padded to the narrowest recogniser width that holds it. Lines under 0.5 confidence are dropped. The text becomes chunks of the document kind on the image's own path, beside its pixel vector, so a hit can come from either and the result list shows the file once.

Turning boxes to the line's slope matters for scans: on a page scanned 0.8° off, upright boxes read 0.79 of the characters and turned ones 1.00. Word recall hid that (the same words turn up elsewhere on the page), so `bench/ocr_static.py` scores characters too, each true line against its best OCR line:

| Image | Words found, RapidOCR on CPU / vinoSearch on NPU | Characters | ms, RapidOCR / vinoSearch |
|---|---|---|---|
| Synthetic desktop, 1920x1080 | 0.994 / 1.000 | 0.969 / 0.994 | 945 / 371 |
| Synthetic desktop, 2880x1620 | 0.988 / 1.000 | 0.955 / 0.994 | 861 / 416 |
| Synthetic A4 scan, 300 dpi, 0.8° off | 1.000 / 0.996 | 0.965 / 0.999 | 1998 / 787 |
| GitHub issue, 1920x1080 | 0.944 / 0.965 | 0.714 / 0.745 | 1392 / 457 |
| GitHub releases, 1920x1080 | 0.855 / 0.959 | 0.682 / 0.735 | 1366 / 481 |
| Wikipedia article, 1920x1080 | 0.912 / 0.978 | 0.614 / 0.621 | 2095 / 606 |

The browser screenshots' truth is the page's visible DOM text, which includes text drawn as icons and splits lines differently than they're drawn, so their character scores are low for both. On the NPU the detector takes 64-130 ms and the recogniser 3-12 ms a line; the first compile of all five is about 20 s, then 0.2 s from the cache. As with the embedding stages, a fresh compile on the NPU is checked against the CPU on a fixed input (cosine 0.9997) and falls back to the CPU below 0.995.

A scanned PDF's pages are rendered one at a time by `pdftoppm` with their longer side at 1920 pixels (164 dpi for A4), from the bytes indexing hashed, on stdin, under the document parser's 1 GiB and 30 CPU seconds a run, and 5 minutes of rendering for the whole file; the first page that fails ends the scan with what was read. Up to 100 pages are read, about 0.6 s each. A PDF with a text layer on any page is taken as text, not OCR'd.

The image vectors' model tag includes the OCR model's revision, so the first start of a build with OCR, or with a new OCR model, indexes every image again once. PDFs recorded with no text are read again once too, and again whenever poppler or the OCR model arrives. `vinosearch read-text <image or PDF>` prints what indexing reads.

## Keeping the index current

Indexed roots are watched with inotify, one non-recursive watch per directory indexing would visit, so `target/`, `node_modules/` and hidden trees cost nothing (a whole home directory here is ~22,600 watches against the 135,178 limit). Changes are indexed after 2 quiet seconds; new roots from `vinosearch index` are picked up within 30 seconds.

inotify can still miss changes: the kernel drops events when its queue overflows, and directories past the watch limit go unwatched. So every root is also compared with the index by mtime and size once an hour, after the machine wakes from sleep, right after dropped events, and on demand from the settings window. That reads no file contents: 0.5 s for 45,418 paths under a home directory here.

## The runner

The runner runs as `vinosearch.service` (logs: `journalctl --user -u vinosearch`), started at login so the watcher is always running. Each time it starts it compares every indexed root against the index by mtime and size, without loading the model (under 0.1 s for 2,600 files here), and indexes whatever changed while nothing was watching. Without systemd, D-Bus starts it on the first query instead.

The index records which model made each kind's vectors (the text model's revision; for images the vision encoder's and the soft-token count too). A build shipping a different one drops that kind's vectors and files at startup, so the startup check indexes them again, and the Index page says why ([#80](https://github.com/karanshukla/vinoSearch/issues/80)). Vectors from another model would otherwise score against new queries without erroring, just wrongly.

One thread owns the model and the index, since OpenVINO's handles can't cross threads, and it runs as a reconcile loop ([#137](https://github.com/karanshukla/vinoSearch/issues/137)). The settings (exclusions, file types, encryption, device) are state it reads at every step, never messages queued behind indexing, and the work is a set of paths to check, each once however often it was asked for: the watcher's debounced batches, its check of every folder and a Reindex from the settings window can all name the same file. Each step applies whatever changed in the settings, then takes the most urgent path: one the watcher saw change (what you're working on), then a folder asked to be indexed, then what a walk found the index behind on, and last the remaining parts of long documents. A file being indexed when a setting changes, or a folder is removed, stops after its current chunk and is checked again under the new settings, so excluding a folder mid-index takes effect within a chunk. Queries are answered between chunks. Work held for a pause, for power or for a model that isn't downloaded simply stays in the set.

It frees the model after 5 idle minutes (`--idle-unload-minutes`, or `VINOSEARCH_IDLE_UNLOAD_MINUTES`; 0 keeps it loaded): 553 MB loaded with a 23k-chunk index and no load spike (it compiles from a static-shape graph, so a cached model is loaded without reading the weights), 12 MB unloaded, ~1 s to reload.

It also serves `dev.vinosearch.Control1` at `/control` for the settings app: `Status` (model loaded, indexing, device, file/chunk counts, roots), `Reindex(path)`, `Forget(path)` and `Unload`. Status answers while the worker is busy (4 ms mid-index). The full interface is in [dbus.md](dbus.md).

`vinosearch krunner --install` writes the service's files (D-Bus activation, the systemd unit, the icon) and the search place for the running desktop, read from `XDG_CURRENT_DESKTOP`: the KRunner plugin on Plasma, which the app launcher's search shares, or GNOME Shell's provider on GNOME. Any other desktop (sway, Hyprland, COSMIC, XFCE) gets neither, since neither KRunner nor GNOME Shell runs there; with no desktop named at all (a TTY, ssh) it writes both. `vinosearch integrate kde|gnome [--off]` turns one place on or off by itself, writing or removing only that place's files and restarting KRunner when it is running; the service stays while any place is on, and for the CLI and the settings app. `vinosearch krunner --uninstall` stops the service and removes everything ([#45](https://github.com/karanshukla/vinoSearch/issues/45)).

## Encryption at rest

Off by default; the Security page or `[encryption] enabled = true` turns it on ([#4](https://github.com/karanshukla/vinoSearch/issues/4)).

**What's encrypted.** The whole database, with SQLCipher 4 (AES-256 per page, HMAC-SHA512) built against the system's libcrypto. Encrypting columns instead would have left the word index (#104) readable: FTS5 keeps every word of every chunk in its own shadow tables, filled by triggers, which column encryption can't reach. The WAL is encrypted the same way, and temporary tables stay in memory. Plain `sqlite3` sees `file is not a database`; `strings` finds nothing.

**The key.** 32 random bytes, used raw (no passphrase derivation, which would add ~0.1 s to every open and nothing to a random key). `systemd-creds --user encrypt` seals them into `index.key` beside the database, scoped to your uid and named `vinosearch-index`. As a user that goes through systemd's `io.systemd.Credentials` service, so no root and no polkit prompt; it needs systemd 256 or newer. Each process unseals it once and holds it, wiped from memory when dropped.

**PCRs.** None. The user-scoped service picks the key itself: the TPM (with the host key) when the firmware reports one, no PCR policy, or systemd's signed PCR 11 policy on a system booted from a UKI with a PCR public key, which survives kernel updates. It ignores `--tpm2-pcrs` from an unprivileged caller, and talking to the TPM directly (tss-esapi) would need the `tss` group. That's also the default worth having: binding to PCR 7 or the firmware's PCRs would rebuild the index after every firmware or Secure Boot update, hours on the NPU for a big home folder, to guard against boot tampering by someone who'd then still need your login.

**No TPM.** `vinosearch` reads which key sealed the credential (its first 16 bytes) rather than trusting what it asked for. Host key alone: refused unless `allow_host_key = true`, the Security page's "Use the host key instead". That key is `/var/lib/systemd/credential.secret`, readable by root, so it keeps the index unreadable in a backup or a synced copy of your home, but not on a disk taken out with the rest of the system.

**A lost key.** If the key doesn't unseal (TPM cleared or replaced), is gone, or doesn't open the database, the runner deletes the index at its next start and indexes your folders again; their list is kept in `index.roots` beside the key for that, in plaintext (folder paths, as `exclude` already is). Status says why (`seal_broken`) and so does the Security page. If nothing can seal a new key either, the new index is encrypted with a key held in memory only and rebuilt at the following start. It is never reopened as plaintext.

**Turning it on or off.** The runner copies the index into a new file under the new key (or none) with `sqlcipher_export`, writes the key file, removes the old WAL and moves the copy into place, while its own connections are closed. A crash part way leaves a key that doesn't open the database, which is rebuilt, not read as plaintext. Nothing is indexed again. A `vinosearch index` run holds the index shared, and a conversion waits for it ("another vinoSearch process has the index open"). **The old plaintext stays in free disk space** until it's overwritten, or trimmed on an SSD; turning encryption on before indexing anything avoids that, and full-disk encryption covers it and swap.

**In memory.** The vector cache isn't locked or wiped: anything that can read the runner's memory runs as you and could unseal the key anyway. This is about the index at rest.

## The settings window

`vinosearch-settings` is egui, in the same style as vinoWhisper's settings (Inter, flat surfaces, the vino green). `--page index|folders|search|model|hardware|integration` opens a given page.

| Page | Shows | Lets you |
|---|---|---|
| Index | Whether it is indexing and how many files it has done, files, chunks and index size on disk, when a change was last indexed and when the folders were last checked for missed changes, the largest folders inside the indexed ones and their share of the index | Check now, exclude one of the largest folders in one click |
| Folders | The included (indexed) folders and the excluded ones | Add a folder (the desktop's own folder chooser, or a typed path), re-index or remove one, add or remove exclusions |
| Search | What is indexed (text always, images and documents by a switch, with image file extensions), what each format holds and its share of the index, and how close a result must be to show, per kind | Turn images and documents on or off, edit image extensions, slide each kind between fewer, closer and more, looser results |
| Model | Whether the model is downloaded and its size, download progress, whether it is loaded and on which device, the compiled-model cache size | Download, verify or delete the model, unload it now, clear the cache, pick NPU, GPU or CPU, set the idle unload time |
| Hardware | What `vinosearch doctor` reports: NPU, driver, NPU compiler, OpenVINO, devices, a probe run on the device picked on the Model page, and how close each model stage came to the CPU | Check again |
| Integration | Whether KRunner (with the Plasma launcher) and GNOME Shell search and the search window's app menu entry are each on, with a note for the desktop that isn't running, and whether the service is | Turn each place on or off (`vinosearch integrate kde\|gnome\|menu [--off]`), or the service with everything (`vinosearch krunner --install` / `--uninstall`), with the command's output |

- **Live.** It asks the runner for `Status` once a second, from a background thread, so the counts move while indexing runs and a slow D-Bus start never freezes the window. If the runner isn't installed, the pages say so.
- **Settings go to `config.toml`.** Exclusions, device and idle unload are written straight to the file, keeping your comments and layout. A file that doesn't parse is left alone and the window says why.
- **Destructive actions ask twice.** Removing a folder drops its part of the index, and deleting the model stops search until it is downloaded again, so both need a second click.
- **Fits the desktop.** It follows the light or dark preference from the desktop portal, and on Plasma its header takes the colour scheme's header colours, so it runs into the title bar like Dolphin's toolbar. Below 620 px wide the sidebar folds into tabs.
- **Light.** It never loads OpenVINO or the model: it only depends on `vinosearch-shared`, eframe and zbus.

### Each setting

The window keeps its help to a line; the rest is here.

| Setting | Details |
|---|---|
| Checked | Changes are indexed within seconds. Every folder is also compared with the index once an hour, after the machine wakes, and whenever the kernel drops change events. It reads no file contents and takes about a second. |
| Included folders | Hidden folders, build output and dependencies (`target`, `node_modules`, `vendor`, ...) are always skipped, as are config formats like TOML, YAML and JSON. |
| Excluded folders | Adding one drops what is indexed under it within 30 seconds. The default list is folders of installed packages and games. |
| Largest folders | Copied source trees and generated files are often most of the index; excluding one also makes indexing quicker. |
| Model download | From Hugging Face, every file checked against a pinned SHA-256. Verify fetches only what is missing or corrupt. The model loads with the first query or indexing job. |
| Compiled cache | OpenVINO's compiled copy of the model. Clearing it makes the next load recompile, about 4 s on the NPU. |
| Device | On the NPU, indexing runs about 5x faster than on the CPU at a tenth of the energy per chunk. Applies within 30 seconds. |
| Unload after | The model is 553 MB loaded and takes about 1 s to load again. |
| Indexing share | The most of the device's time background indexing may take, half that on battery. Lower leaves more room for face unlock and dictation. |
| Images | Found by what is in them and the text they show, from a text search: "a cat", "a receipt", an invoice number. About 1.3 s each on the NPU, plus 0.1-0.5 s for the text. The vision model is a 671 MB download. Turning them off drops them from the index. Extensions apply on Enter. |
| Documents | See [Documents](#what-gets-indexed) above. |
| Sessions | vinoWhisper's saved transcripts and summaries; see [vinoWhisper sessions](#what-gets-indexed) above. Turning it off drops them from the index. |
| Code | Found only by searches starting `code:`, and those find nothing else. Files git ignores, vendored and generated code and anything holding a key are left out; see [Code](#what-gets-indexed) above. Turning it off drops it from the index. |
| How close results must be | Results scoring below the line are left out; applies from the next search. For text, an unrelated query tops out around 0.60 and real matches score 0.70 and up. Images score lower: right matches about 0.61 to 0.69, the rest 0.57 to 0.60. |
| Encryption | See [Encryption at rest](#encryption-at-rest). Applies at once. Refused without a TPM unless "Use the host key instead" is on. New key seals a fresh key and re-encrypts the index under it, after moving to a machine with a TPM, say. |
| Hardware | Runs a tiny model on the device picked on the Model page, in a separate process, as `vinosearch doctor` does. |
| Index on battery | On by default. Off holds indexing while the machine runs on battery (checked every 10 s) and the Index page says "Waiting for a charger". Indexing also pauses while power-profiles-daemon or tuned-ppd has the power-saver profile on ("Paused: power saver"), whatever this is set to. Either way searches keep working, changes found meanwhile wait in the queue, and indexing picks up where it stopped. |
| Search places | One switch each for KRunner (and the Plasma app launcher, which shares KRunner's plugins), GNOME Shell, and the search window as an app of its own in the app menu (off by default; `vinosearch-settings --search`). Each writes or removes its own files; GNOME shows the change from the next login, KRunner at once. The one for a desktop that isn't running says so, but still works. |
| Search service | Starts at login, watches the folders and answers searches. Turning it on also adds the running desktop's search place; turning it off removes every place with it. The index and model stay. |

**Adding a folder** first shows what indexing it will cost: files, time and index size, and the five subfolders that would take longest, each with an Exclude button ([#112](https://github.com/karanshukla/vinoSearch/issues/112)). `vinosearch estimate DIR` prints the same, and `vinosearch index` prints it as one line ("~4,200 files, ~35 min, ~180 MB") and, at a terminal, asks before starting (`--yes` doesn't ask). The walk is the hourly check's, with its exclusions, skipped directories, formats and size caps, and reads no file contents. Files the index already has unchanged cost nothing. Times are at the NPU rates in the README (28.6 chunks/s, 1.3 s an image) and size at 470 MB per 113k chunks; a text file is taken as a chunk per 800 bytes, which ran 8% over on 30k real chunks, and a document's text as a share of its file guessed high (a quarter for a PDF, half for Office and EPUB files). Byte-identical copies are counted in full, though only one is embedded.

**Largest folders** lists up to 8 folders inside the indexed ones, biggest first, none inside another. A folder gives way to a subfolder holding three quarters of it, so a copied source tree shows as the part that is its bulk. It refreshes once the index changes and indexing settles.

It is Wayland only for now, as vinoWhisper's window is. Clicking vinoSearch's icon in GNOME Shell's search results opens it.

## Query latency

Warm queries started at p50 85 ms on the benchmark index. Fixed, in order of what they saved: an O(n) `COUNT(*)` on every query, a dot product that did not vectorize, a full sort for the top 40, f32 vectors (now int8 in memory: the top result scores at most 0.001 below f32's), and a 64-token query shape (now 32). `bench/query_latency.sh` measures through the installed runner.

## Runtime

OpenVINO comes from [ovfetch](https://github.com/karanshukla/ovfetch) (0.6.0 or newer), installed per user into `~/.local/lib/vinosearch/openvino`:

```bash
scripts/install-runtime.sh   # Intel's openvino wheel alone: 139 MB, ~17 s, no compile
vinosearch doctor            # NPU, driver, runtime, then compiles and runs a probe model
```

**Parity** ([#76](https://github.com/karanshukla/vinoSearch/issues/76)). A device can run a model wrong without an error: the INT8 vision encoder gave cosine 0.67 against the CPU on the NPU. So when a stage (query, document, image) compiles fresh on anything but the CPU, or hasn't been checked under this OpenVINO, it embeds a fixed input and compares the vector with the CPU's, stored in `src/parity/`. Under 0.995 that stage runs on the CPU instead. The NPU gives 0.9995 (document) to 0.99998 (image). The result sits next to the stage's compiled blobs, and `vinosearch doctor` prints it. After a model revision changes, regenerate the references with `cargo test --release write_parity_references -- --ignored`.

vinoSearch calls OpenVINO directly, without ONNX Runtime, so it uses `ovfetch install --openvino-only`. That gets the newest OpenVINO the NPU driver supports (2026.4.1 here), the same release vinoAuthFace and vinoWhisper run, which matters: an NPU fault resets the device for every process using it. `doctor --openvino-dir <prefix>` points at another ovfetch install. The runtime is loaded at startup (`openvino` crate, `runtime-linking`), so nothing links against the prefix and nothing needs an rpath.

ovfetch's runtime ships OpenVINO's ONNX frontend only, so models must be ONNX.

**NPU compiler.** Some distros package the NPU driver without its compiler (Fedora's `intel-npu-driver` 1.32.0 rpm): the NPU enumerates, and every compile fails with `ZE_RESULT_ERROR_UNSUPPORTED_FEATURE`. `ovfetch npu install` (0.6.0) fetches the compiler, or a whole driver, from Intel's linux-npu-driver release into `~/.local/lib/ovfetch-npu`, checked against ovfetch's ledger, no root. `scripts/install-runtime.sh` runs it when `ovfetch detect` reports a driver with no compiler. Those libraries are not on the linker path, so vinoSearch loads them itself (`runtime::preload_npu`): each line of the prefix's `ovfetch-preload.txt`, in order, with `RTLD_GLOBAL`, after OpenVINO and before anything opens the NPU. `vinosearch doctor` prints the prefix when one is in use.

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

## Ranking evals

`bench/` measures the models; the evals measure vinoSearch's ranking on top of them: chunking, the `min_score` floors, one result per file, exact words ([#107](https://github.com/karanshukla/vinoSearch/issues/107)). `scripts/make-testbed.sh` writes a testbed of 46 small files (notes, code, French and Hindi text, PDFs, `.docx`, `.xlsx`, `.pptx`, RTF, a Word 97 `.doc`, CSVs, two drawn pictures), with near misses next to the files queries expect. `vinosearch eval` indexes it into a throwaway index under the temp directory, runs each query in `evals/queries.toml` through the same search and per-file ranking KRunner gets, and prints recall@1, recall@5 and MRR overall and per kind, or `--json`:

    scripts/make-testbed.sh /tmp/testbed
    vinosearch --device CPU eval /tmp/testbed

It exits non-zero when an overall metric is below the threshold in `evals/queries.toml` (or `--min-recall-at-1`, `--min-recall-at-5`, `--min-mrr`). CI runs it on the CPU. Image queries need `--images` and the vision encoder, so they run by hand.

CPU baseline, 2026-10-25, 55 queries, 52 chunks:

| kind | queries | recall@1 | recall@5 | MRR |
|---|---|---|---|---|
| keyword | 6 | 1.000 | 1.000 | 1.000 |
| paraphrase | 18 | 0.778 | 0.889 | 0.833 |
| section | 5 | 1.000 | 1.000 | 1.000 |
| identifier | 6 | 1.000 | 1.000 | 1.000 |
| cross-language | 9 | 0.889 | 1.000 | 0.944 |
| document | 11 | 1.000 | 1.000 | 1.000 |
| overall | 55 | 0.909 | 0.964 | 0.936 |

The misses are paraphrases: two score under the 0.65 floor, two pick the other retry function, and an English query about notice periods ranks the English moving checklist over the French lease. The CPU gates PRs because what the evals are for (chunking, floors, dedup, exact words) doesn't depend on the device. The NPU's vectors are 0.9995 to 0.99998 cosine of the CPU's (`parity.rs`), which typically moves a score by about 0.001 (0.03 at the very worst); 6 of the 55 queries are within 0.01 of a rival file or the floor, so the NPU may differ from the CPU by a query or two. The thresholds leave room for two.

## NPU gotchas

Found along the way, on OpenVINO 2026.4.1 and NPU driver 1.35.0:

- A static batch above 1 mixes rows across the batch. Batch 1 with requests in flight is correct and about 6x faster.
- `com.microsoft.RotaryEmbedding` with offset-form `position_ids` converts wrong ([#1](https://github.com/karanshukla/vinoSearch/issues/1)); `bench/patch_rope.py` rewrites it.
- ONNX external data reached through the Hugging Face cache's symlinks is refused; download into a plain directory.
- The vision encoder's published graph has a data-dependent output size (`NonZero` into `GatherND`), which the NPU can't compile. [arankays/embeddinggemma-2-vision-static-onnx](https://huggingface.co/arankays/embeddinggemma-2-vision-static-onnx) is the static-shape rewrite. Its INT8 version runs on the NPU but is wrong (cosine 0.67 against the reference), so it ships fp32.

## Demos

The animations in the README (`docs/media/krunner.svg`, `settings.svg`) are drawn, not recorded. File names, snippets and counts in them are made up. The photo is by [Rafaëlla Waasdorp](https://unsplash.com/@krafaella_) on [Unsplash](https://unsplash.com/license).
