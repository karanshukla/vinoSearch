---
title: How it works
nav_order: 7
---

# How vinoSearch works

Technical details covering what gets indexed, how the index stays current, how the background runner operates, encryption at rest, runtime setup, benchmarks, and NPU notes.

## What gets indexed

### Text

Markdown, plain text, and HTML files up to 1 MiB each (or 16 MiB for `.txt`, `.md`, `.rst`, `.org`, and `.tex`, which may hold book-length text) are split into ~900-character chunks.

HTML pages are stripped of tags, scripts, styles, and comments while preserving line numbers so matches still open at the right line (HTML pages indexed before this change are re-indexed once). Configuration formats such as TOML, YAML, and JSON are intentionally excluded because they frequently contain credentials. Hidden directories, build outputs, vendored dependencies, and generated reports (`target`, `node_modules`, `vendor`, `obj`, `coverage`, etc.) are skipped across all formats.

### Images

Image indexing is disabled by default. When enabled, images up to 64 MiB are embedded directly from their pixels using EmbeddingGemma 2's vision encoder (both stages running on the NPU: 1.3 s and ~4.4 J per image, with CPU used only for decoding).

- **Search and grouping:** Text queries match images directly. KRunner displays them under **vinoSearch Images** separately from **vinoSearch Text**, while GNOME Shell lists them after text results. Each result uses its MIME-type icon. Because image cosine scores run lower than text scores, images use their own `min_score` threshold.
- **Enabling and disabling:** Turning images on scans all roots for existing photos (run `vinosearch fetch-model` or click **Download** on the Model page first); turning them off removes them from the index.
- **HEIC and AVIF decoding:** Because the Rust `image` crate lacks HEIF/AVIF decoders, those formats are converted to JPEG in a private temporary directory via `libheif`'s `heif-convert` (`libheif-tools` on Fedora, `libheif-examples` on Debian/Ubuntu, `libheif` on Arch). If `heif-convert` is missing, those photos are recorded as skipped, the Index page reports the count, and installing the package triggers indexing on the next startup ([#79](https://github.com/karanshukla/vinoSearch/issues/79)). Images that fail to decode store a vector-less row so they are not retried until modified.
- **Text inside images:** Visible text in photos and screenshots is also extracted via [OCR](#ocr) so screenshots of error dialogs or photos of receipts match by their words (under Documents) as well as visually.
- **Near-duplicate grouping ([#78](https://github.com/karanshukla/vinoSearch/issues/78)):** Byte-identical copies, resized/recompressed exports, edits, crops, and burst shots collapse into one result under the best-scoring image with `(+N similar)`. Grouping runs at query time (`dupes::collapse`) without extra storage, adapted from [Orchard Photos](https://github.com/chaseleantj/orchard-photos) with thresholds tuned on EmbeddingGemma 2 vectors (`bench/near_duplicates.py`, `bench/results/near_duplicates.txt`):
  - An image joins a group if its cosine similarity to the fixed group leader is ≥ 0.92 (distinct Flickr30k photos peaked at 0.90), or ≥ 0.80 when its filename in the same folder differs only by a size or copy suffix (`-800x400`, ` (1)`, ` copy`).
  - Because screenshots of the same application can score very similarly regardless of content, same-resolution images without EXIF camera maker/model metadata must also match pixel-wise: at most 1% of a brightness/contrast-normalized 256×256 grayscale thumbnail may differ by more than 16/255 (Orchard's 64×64 / 3% check misses text changes). Resized copies and crops differ in dimensions and rely on vector similarity alone.
  - KRunner and GNOME Shell currently apply grouping; the standalone search window does not yet.
- **Reverse image similarity ([#77](https://github.com/karanshukla/vinoSearch/issues/77)):** `vinosearch similar <image>` ranks indexed photos by dot product against the target's image vector (no encoding needed if already indexed; ~1.3 s on the NPU otherwise). Because image-to-image cosine scores run much higher than text-to-image scores, the command prints raw scores and accepts `--min-score` rather than reusing `formats.images.min_score`.

### Documents

Document indexing is disabled by default because document directories commonly hold financial statements, tax forms, and bank exports whose extracted text stays readable in the index unless [Encryption at rest](#encryption-at-rest) is enabled. Turning documents off drops them from the index.

- **Supported formats:** PDF (per-page text), Word, PowerPoint, Excel, and OpenDocument (including macro/template variants and flat `.fodt`/`.fodp`), legacy Word/PowerPoint 97–2003 (`.doc`, `.ppt`), RTF, EPUB, SRT/WebVTT subtitles, and CSV/TSV. Extracted text runs through the same text embedding model and appears under **vinoSearch Documents** with its matching page, slide, row, chapter, or line. Spreadsheet and CSV chunks prepend the header row.
- **PDF boilerplate removal ([#158](https://github.com/karanshukla/vinoSearch/issues/158)):** Running headers, footers, and page numbers are stripped when a line in the first or last three lines of a page is purely a page number (`3`, `Page 3 of 40`, `iv`) or, ignoring digits, appears on at least 3 pages and more than half of the pages (or half of odd/even pages) in the current extraction run.
- **Scanned PDFs:** PDFs with no text layer are OCR'd page by page when poppler's `pdfinfo` and `pdftoppm` are installed (`poppler-utils` on Fedora/Debian/Ubuntu, `poppler` on Arch). Without poppler, they are recorded without text and automatically picked up on the next startup after poppler is installed.
- **Incremental indexing of long files ([#108](https://github.com/karanshukla/vinoSearch/issues/108)):** The first 30 pages (slides, chapters, or 30 groups of 3 chunks for unpaginated formats) are indexed immediately alongside regular files so one large book does not block a folder of notes. Remaining sections are indexed 30 parts at a time once other work finishes, capped at 5,000 parts and 16 MiB of text. Partially indexed files show progress on the Index page (for example, `"120 of 640 pages"`), resume after restarts, and restart from scratch if modified. Long plain-text files follow the same split.
- **Sandboxed extraction ([#84](https://github.com/karanshukla/vinoSearch/issues/84)):** Each document parses in a child process (`vinosearch extract`) limited to 1 GiB of address space, 30 CPU seconds, and 2 minutes of wall time per 30-part batch so malformed or hostile files cannot crash or hang the runner. Failed files are recorded without text and skipped until modified.

### Code

Source code indexing is disabled by default ([#110](https://github.com/karanshukla/vinoSearch/issues/110)). It covers Rust, Python, JavaScript, TypeScript, Go, C, C++, Java, Kotlin, shell, SQL, CSS, and roughly 50 other extensions (`code::EXTENSIONS`) up to 1 MiB each. Turning code off drops it from the index.

- **Dedicated `code:` queries:** Because source code dominates a developer's home directory, normal queries never return code. Prefix a query with `code:` to search source code exclusively (displayed under **vinoSearch Code** and opened at the matching line). Queries use EmbeddingGemma's code-retrieval prompt (`task: code retrieval`), which raises scores by ~0.04 and motivates code's higher default `min_score` (`0.72`).
- **Exclusions and secret filtering:** Inside git repositories, ignored files (`.gitignore` at every level, `.ignore`, `.git/info/exclude`, and global excludes via the `ignore` crate) and directories named `third_party`, `third-party`, `3rdparty`, or `vendored` are skipped. This `.gitignore` filtering applies only to code so ignored personal notes remain searchable and a git-tracked home directory with `*` in `.gitignore` does not block non-code files. Generated files (`*.pb.go`, `*_pb2.py`, `*.d.ts`, `*.g.dart`, `*.designer.cs`, `*.generated.*`, minified/bundled JS, files with `do not edit` or `@generated` headers), secret filenames (`secrets.py`, `credentials.js`), private keys (`-----BEGIN ... PRIVATE KEY-----`), and unmistakable API tokens (AWS key IDs, GitHub, GitLab, Slack, Stripe, OpenAI, and Google API keys) are refused and stored with empty chunks so they are not re-read until changed.
- **Definition-aware chunking (`code::chunk`):** Rather than splitting every ~900 characters, units start at top-level indented lines after blank lines or definition keywords (`fn`, `def`, `class`, `func`, `function`, etc., including leading comments, attributes, and decorators). Adjacent short units pack into chunks up to 900 characters; long definitions (`class`, `impl`, large functions) split at nested units one level deeper while repeating the parent signature line. Plain length splitting is used only when no structural boundary exists.
  - On 7.7 MiB across 795 files of Rust, Python, TypeScript, Go, C, and shell, 98.4% of functions that fit within a chunk landed intact in one chunk (compared to 69.7% with the text chunker and 99.1% with tree-sitter, which would have added 10.7 MB to the binary and 45 s of C compilation).
  - On 12 queries against `ovfetch`'s source, the top result opened inside the target function 7/12 times (MRR 0.75) vs. 4/12 (MRR 0.47) with the text chunker.
  - Existing pre-feature indexes migrate once on open: existing code chunks are relabeled without re-embedding, generated/secret files are dropped, and old chunk boundaries remain until files change.

### vinoWhisper sessions

Session indexing is disabled by default ([#143](https://github.com/karanshukla/vinoSearch/issues/143)) because transcripts capture whatever was spoken or played and remain readable unless the index is encrypted.

- **Bundle format:** A `.vwsession` file saved by vinoWhisper ([vinoWhisper#123](https://github.com/karanshukla/vinoWhisper/issues/123)) is a ZIP archive starting with an uncompressed `mimetype` entry (`application/vnd.vinowhisper.session+zip`), containing `transcript.txt` (paragraphs beginning with `[MM:SS]`), an optional `summary.md`, and `attestation.json`.
- **Indexing and display:** Transcripts and summaries are indexed as chunks of the same file. Transcript chunks track line numbers and group short paragraphs up to ~15 s of speech (always starting at a `[MM:SS]` timestamp); summary chunks use line numbers starting at 2^32 (`session::SUMMARY_LINES`) to distinguish them. Matches appear under **vinoWhisper sessions** with a subtitle showing the part, timestamp, date, and duration (for example, `"Transcript at 01:12 · 2026-10-27 14:05, 43 min"`, or `"Summary, AI-generated"` when marked as model-written in the attestation). KRunner offers **Open summary** or **Open transcript** actions.
- **Opening:** Sessions open in vinoWhisper's GUI via `vinowhisper-gui --page sessions --open <bundle>#transcript@MM:SS` (or `#summary`). If vinoWhisper is not installed, the matched part is written to a `0600` file under `$XDG_RUNTIME_DIR/vinosearch/sessions` (overwriting the previous file) and opened as text at the matching line.
- **Discovery and safety:** Because `$XDG_DATA_HOME/vinowhisper/transcripts` sits under a hidden directory, enabling sessions automatically adds it as a root (taking only `.vwsession` files, ignoring active `.txt` logs and `.tmp` writes), while exported `.vwsession` files anywhere under other roots are also indexed. Adding a summary rewrites the archive and triggers normal re-indexing. Bundles whose `attestation.json` specifies a `format` other than `vinowhisper-session/1` are skipped, recorded without text, and counted on the Search page. Sessions are read directly inside the runner (plain text in ZIP) with a 1 MiB cap each for transcript and summary (~20 hours of speech) to bound decompression.

### Default exclusions

When `exclude` is omitted from `config.toml`, vinoSearch automatically skips package and game trees that are slow to index and rarely searched: `~/snap`, `~/go/pkg`, `~/sdk`, `~/Android/Sdk`, `~/R`, `~/perl5`, and `~/Games`, along with conda environments (`miniconda3`, `anaconda3`, etc.) anywhere in the tree. Adding an exclusion removes existing indexed entries under it, and `vinosearch index` rejects excluded paths.

A running runner picks up changes to `exclude`, `[formats]`, and `device` within 30 seconds unless overridden by a CLI flag or environment variable (see [dbus.md](dbus.md#settings)).

## Exact words

Vector embeddings do not preserve exact identifiers (for example, `INV-20431` looks similar to any other invoice code). To support exact matching, every chunk's text and filename are also indexed in a contentless SQLite FTS5 table kept in sync via triggers on `chunks` ([#104](https://github.com/karanshukla/vinoSearch/issues/104)). Because text is not duplicated, FTS5 adds only 10–13% to database size; pre-FTS5 indexes backfill once on open (16 s for 113k chunks in a 2-vCPU VM).

Every query combines vector and FTS5 lookups:
- A chunk containing all query terms receives a `+0.05` cosine boost, enough to break ties and pull borderline hits above `min_score` without promoting unrelated chunks.
- Single identifier-like queries (containing a digit, internal punctuation, or all capitals such as `INV-20431`, `E0502`, or `TODO`) and fully quoted queries place exact FTS5 matches first in BM25 order while keeping scores on the cosine scale so `min_score` and category grouping still apply.
- Terms appearing in more than 2,000 chunks are skipped by FTS5 because common words are not selective and ranking them costs more than vector search (250 ms for `"the"` on 113k chunks). With this cap, FTS5 adds 0–5 ms per query on 113k chunks.
- All query words are quoted before passing to FTS5 so `AND`, `NEAR`, `*`, and `:` behave as literal terms.

FTS5 uses the `unicode61` tokenizer (splitting on whitespace and punctuation). Because Chinese, Japanese, and Thai lack spaces, contiguous runs index as a single token and require matching the full run for exact lookup.

## OCR

Images and scanned PDFs are read with PaddleOCR's PP-OCRv6 small text detector and recognizer from [arankays/pp-ocrv6-small-onnx](https://huggingface.co/arankays/pp-ocrv6-small-onnx) (identical to RapidOCR 3.10.0's ONNX files, 31 MB, downloaded with the base text model) ([#106](https://github.com/karanshukla/vinoSearch/issues/106)).

Because the NPU cannot compile dynamic tensor shapes, `src/ml/ocr.rs` compiles five static shapes: detector at `1088×1920` and `1920×1088`, and recognizer at `48×320`, `48×640`, and `48×1280`. Each image scales (preserving aspect ratio, up to 4× for small inputs) into the orientation-matched detector shape with black padding. Detected regions are rotated to their principal axis when elongated, expanded by DB's unclip distance, bilinearly sampled, and padded to the narrowest fitting recognizer width. Lines scoring below 0.5 confidence are dropped. Extracted text is stored as document chunks attached to the image path alongside its pixel vector so matches from either appear once per file.

Rotating boxes to match text slope is important for skewed scans: on a page tilted 0.8°, axis-aligned boxes recovered 0.79 of characters compared to 1.00 with rotated boxes. Because word recall masks character errors when words repeat elsewhere on a page, `bench/ocr_static.py` measures both word recall and per-line character accuracy:

| Image | Words found (RapidOCR CPU / vinoSearch NPU) | Characters (RapidOCR / vinoSearch) | Latency ms (RapidOCR / vinoSearch) |
|---|---|---|---|
| Synthetic desktop, 1920×1080 | 0.994 / 1.000 | 0.969 / 0.994 | 945 / 371 |
| Synthetic desktop, 2880×1620 | 0.988 / 1.000 | 0.955 / 0.994 | 861 / 416 |
| Synthetic A4 scan, 300 dpi, 0.8° off | 1.000 / 0.996 | 0.965 / 0.999 | 1998 / 787 |
| GitHub issue, 1920×1080 | 0.944 / 0.965 | 0.714 / 0.745 | 1392 / 457 |
| GitHub releases, 1920×1080 | 0.855 / 0.959 | 0.682 / 0.735 | 1366 / 481 |
| Wikipedia article, 1920×1080 | 0.912 / 0.978 | 0.614 / 0.621 | 2095 / 606 |

Browser screenshot ground truth comes from visible DOM text (which includes icon text and differs in line wrapping), lowering character scores for both pipelines. On the NPU, detection takes 64–130 ms and recognition 3–12 ms per line; initial compilation of all five graphs takes ~20 s, then 0.2 s from cache. Fresh NPU compiles are parity-checked against CPU output (cosine 0.9997; falls back to CPU below 0.995).

Scanned PDFs are rendered page by page with `pdftoppm` scaled to 1920 px on the longest side (164 dpi for A4), piped over stdin from the already-hashed bytes, subject to the extractor's 1 GiB memory limit, 30 CPU seconds per run, and 5-minute total render cap. Up to 100 pages are OCR'd (~0.6 s per page), stopping on the first page error while keeping earlier pages. PDFs containing any embedded text layer are parsed as text rather than OCR'd.

Because image model tags include the OCR model revision, upgrading to a build with OCR (or a newer OCR model) automatically re-indexes images once. Previously empty PDFs are also retried when poppler or OCR becomes available. Run `vinosearch read-text <image or PDF>` to inspect extracted OCR text directly.

## Keeping the index current

Indexed roots are watched with `inotify`, using one non-recursive watch per visited directory so skipped trees (`target/`, `node_modules/`, hidden folders) consume zero watches (a full home directory uses ~22,600 watches against a 135,178 kernel limit). File edits are indexed after 2 quiet seconds, and new roots added via `vinosearch index` are picked up within 30 seconds.

Because `inotify` can drop events on queue overflow or hit watch limits, every root is also reconciled against the index by mtime and size hourly, after system resume from sleep, immediately after dropped events, and on demand from Settings. This scan reads metadata only (0.5 s for 45,418 paths on a typical home directory).

## The runner

The background service is `vinosearch krunner`. It starts at login via `~/.config/autostart/dev.vinosearch.Runner.desktop` (keeping the file watcher active) or via D-Bus activation on the first query, avoiding systemd-specific units so it works consistently across distros. Compositors that do not run XDG autostart entries (such as sway or Hyprland) activate it on first query or via `exec vinosearch krunner`.

- **Single-instance locking and supervision:** A lock in `$XDG_RUNTIME_DIR` ensures only one instance runs; a competing startup (for example, autostart racing an early query) waits for the D-Bus name and exits. `krunner` supervises a `krunner --worker` child and restarts crashes with exponential backoff (1 s up to 60 s), except after clean exits, stop signals, or crashes before acquiring the bus name. Stopping `krunner` stops the worker.
- **Logging:** Output goes to the systemd journal when launched by systemd (`journalctl --user -u '*dev.vinosearch*'`), to `~/.local/state/vinosearch/service.log` otherwise (rotated to `service.log.old` past 1 MiB), or directly to stdout when invoked from a terminal.
- **Startup reconciliation and model versioning:** On startup, the runner checks all roots by mtime and size without loading ML models (<0.1 s for 2,600 files) to catch offline edits. Each indexed format records the model revision that produced its vectors (plus vision encoder revision and soft-token count for images); if a newer build ships a different model, affected vectors are dropped and re-indexed automatically, with a notice on the Index page ([#80](https://github.com/karanshukla/vinoSearch/issues/80)).
- **Reconcile loop ([#137](https://github.com/karanshukla/vinoSearch/issues/137)):** A single worker thread owns OpenVINO handles (which are not thread-safe) and the SQLite connection. Settings (`exclude`, formats, encryption, device) are read as live state on every iteration rather than queued messages, and pending work is a deduplicated set of paths prioritized as: watcher events (files you just edited), explicit `Reindex` requests, periodic scan diffs, and remaining parts of long documents. Changing a setting or removing a folder interrupts the active file after the current chunk and re-evaluates it under the new settings. Search queries run between chunks, and paused/power-held work simply stays queued.
- **Idle memory unloading:** After 5 idle minutes (`--idle-unload-minutes` or `VINOSEARCH_IDLE_UNLOAD_MINUTES`; `0` disables), the runner unloads the model, dropping memory from ~553 MB (with a 23k-chunk index and no load spike thanks to static-shape cached blobs) down to 12 MB, and reloads in ~1 s on the next query.
- **Control interface and desktop integration ([#45](https://github.com/karanshukla/vinoSearch/issues/45)):** Serves `dev.vinosearch.Control1` at `/control` (`Status` responds in ~4 ms even mid-index; see [dbus.md](dbus.md)). `vinosearch krunner --install` writes D-Bus activation, autostart, and icon files, restarts any existing runner (`Control1.Quit`), cleans up legacy systemd units, and installs the search provider matching `XDG_CURRENT_DESKTOP` (KRunner on Plasma, GNOME Shell on GNOME, neither on sway/Hyprland/COSMIC/XFCE, or both when run headless over SSH/TTY). Use `vinosearch integrate kde|gnome [--off]` to toggle individual desktop integrations, or `vinosearch krunner --uninstall` to remove the service and all integration files.

## Encryption at rest

Disabled by default; enable via the settings window's Security page or `[encryption] enabled = true` in `config.toml` ([#4](https://github.com/karanshukla/vinoSearch/issues/4)).

- **Database encryption:** Encrypts the entire database and WAL using SQLCipher 4 (AES-256 per page, HMAC-SHA512) linked against system `libcrypto`, with temporary tables kept in memory. Full-database encryption is required because FTS5 shadow tables (#104) store every indexed word and cannot be covered by column-level encryption. Plain `sqlite3` reports `file is not a database` and `strings` yields no plaintext.
- **Key generation and `systemd-creds` sealing:** Generates 32 raw random bytes (skipping PBKDF2 passphrase derivation, which would add ~0.1 s per open without strengthening a random key). On systemd 256+, `systemd-creds --user encrypt` seals the key into `index.key` next to the database via `io.systemd.Credentials` (scoped to your UID, named `vinosearch-index`, requiring no root or polkit prompt). Each process unseals it once and zeroizes it on drop.
- **PCR policy:** No explicit PCRs are bound. The user credential service selects the TPM + host key automatically (or systemd's signed PCR 11 policy on UKI systems, which survives kernel updates), ignoring `--tpm2-pcrs` from unprivileged callers. Binding firmware PCRs like PCR 7 would force multi-hour re-indexing after every BIOS or Secure Boot update.
- **Host-key fallback (no TPM):** vinoSearch inspects the credential's first 16 bytes to verify which key sealed it. If sealed only with systemd's host key (`/var/lib/systemd/credential.secret`, readable by `root`), it is refused unless `allow_host_key = true` (**Host-key fallback**). Host-key sealing protects backups and synced home folders, though not an extracted full disk.
- **Desktop keyring fallback (no `systemd-creds`):** Where `systemd-creds --user` is unavailable (non-systemd or systemd < 256), the key is stored via the freedesktop Secret Service (`seal = "keyring"`). Existing TPM/host credentials never silently fall back to the keyring. The entry is stored in the default collection labeled `"vinoSearch index key"` with attributes `xdg:schema = dev.vinosearch.IndexKey` and `id = <32 random hex digits>`, while `index.key` stores `secret-service 1 <id>` (distinguishable from base64 systemd credentials). Rotating the key or disabling encryption deletes the old keyring item. Communication uses direct `zbus` calls over a `plain` session (no extra crypto dependencies): although the key crosses the session bus unencrypted, only your own processes can attach to the session bus and could already query the unlocked keyring directly. If a keyring rejects `plain` sessions, the index stays preserved in `seal_waiting`. Flatpak accesses the same Secret Service via `--talk-name=org.freedesktop.secrets` for format compatibility with native installs.
- **Flatpak TPM sealing:** Inside Flatpak, vinoSearch invokes the host's `systemd-creds` via `flatpak-spawn -v --clear-env --directory=/ --host /usr/bin/systemd-creds --user --name=vinosearch-index encrypt - -` (or `decrypt`), locating the binary under `/run/host/usr` and piping the key directly to the host process. Host TPM and systemd 256+ checks (`systemd-analyze has-tpm2`, `systemd-notify --booted`) run the same way, producing credentials interchangeable with native installs (~5 ms overhead on top of the TPM's ~200 ms). Because `flatpak-spawn` exits `1` for both host-spawn errors and tool errors, vinoSearch passes `-v` and checks the `child exit code` line: unreachable session helper, revoked permission, spawn failure, or 30 s timeout all enter `seal_waiting` rather than dropping the index or switching to the keyring.
- **Startup with a locked keyring:** The key is fetched once at runner startup. If the keyring is locked, the runner stays up with the index closed (`seal_waiting`), prompts to unlock once (1-minute timeout), retries in the background every 30 s, and re-prompts on `ApplyEncryption` (the **Unlock** button). Dismissed prompts, locked keyrings, missing/slow Secret Service (10 s timeout), and missing keyring items all preserve the index. Turning encryption off after a failed prompt rebuilds the index in plaintext (preserving configured roots), after which re-enabling encryption seals a new key.
- **Lost keys:** If a `systemd-creds` key fails to unseal (TPM cleared/replaced), goes missing, or fails to open the database, the runner deletes the index on next startup and re-indexes roots saved in plaintext `index.roots` (reporting `seal_broken`). If sealing a replacement key also fails, the rebuilt index uses an ephemeral in-memory key and is never written in plaintext.
- **In-place conversion and free space:** Enabling or disabling encryption closes open connections, exports the database with `sqlcipher_export`, writes `index.key`, removes the old WAL, and atomically swaps the file without re-indexing. A mid-conversion crash leaves a mismatched key that triggers a clean rebuild rather than plaintext exposure. Concurrent `vinosearch index` runs hold a shared lock that conversion waits on (`"another vinoSearch process has the index open"`). Note that old unencrypted blocks remain in free disk space until overwritten or SSD-trimmed; enable encryption before initial indexing (or use full-disk encryption) to avoid plaintext remnants and swap exposure.
- **In-memory state:** The vector cache is not `mlock`'d or zeroized because any process able to read runner memory already runs as your user and could unseal the key. The key itself is zeroized on drop, though transient copies in `zbus` message buffers during keyring calls are not wiped.

## The settings window

`vinosearch-settings` is built with `egui` and styled with [vinoUserInterface](https://github.com/karanshukla/vinoUserInterface), sharing its theme and widgets with vinoWhisper and vinoSpeak (Inter font, flat hairline surfaces, and vino green accents). Pass `--page index|folders|search|model|hardware|integration` to open directly to a specific page.

| Page | Displays | Actions |
|---|---|---|
| Index | Indexing status and file count progress, indexed files/chunks and database size on disk, last indexed and last checked timestamps, and the largest subfolders with their share of the index | Trigger **Check now**, or exclude any large subfolder in one click |
| Folders | Included (indexed) roots and excluded directories | Add folders via the desktop folder chooser or a typed path, re-index or remove roots, and manage exclusions |
| Search | Enabled file formats (text always on; toggles for images, documents, code, sessions, and media, plus image extensions and optional media duration limit), per-format index shares, and minimum similarity thresholds | Toggle formats, edit image extensions, skip long media past a custom minute limit, and adjust match-threshold sliders per format |
| Model | Download state and disk size, active download progress, loaded status and device, and compiled-model cache size | Download, verify, or delete models, unload now, clear the cache, choose NPU/GPU/CPU, and set the idle unload timeout |
| Hardware | Output of `vinosearch doctor`: NPU, driver, NPU compiler, OpenVINO version, available devices, a live probe on the selected device, and CPU parity scores per model stage | Re-run hardware checks |
| Integration | Toggle status for KRunner (including Plasma's launcher), GNOME Shell search, the standalone search window app-menu entry, and the background service (noting when a desktop is not active) | Toggle each integration (`vinosearch integrate kde\|gnome\|menu [--off]`) or install/remove the service (`vinosearch krunner --install` / `--uninstall`), displaying command output |

Key design properties:
- **Live updates:** Polls `Status` once per second from a background thread so counts update smoothly while indexing and a slow D-Bus startup never blocks the UI. If the runner is not installed, pages report that clearly.
- **Direct `config.toml` edits:** Changes to exclusions, device, and idle timeout are written directly to `config.toml` while preserving comments and formatting. If the file fails to parse, it is left untouched and an error is shown.
- **Confirmation for destructive actions:** Removing a folder (which drops its indexed entries) and deleting models (which halts search until re-downloaded) both require a second confirmation click.
- **Desktop theme integration:** Follows the desktop portal's light/dark preference, matches KDE Plasma's header color scheme so the top bar blends with the title bar like Dolphin's toolbar, and collapses the sidebar into tabs below 620 px width.
- **Lightweight dependencies:** Never loads OpenVINO or ML models; depends only on `vinosearch-shared`, `eframe`, and `zbus`.

### Each setting

Detailed notes for every option exposed in the settings window:

| Setting | Details |
|---|---|
| Checked | File changes are indexed within seconds via `inotify`. Every folder is also reconciled against the index hourly, after wake from sleep, and after dropped kernel events (metadata-only scan, ~1 s). |
| Included folders | Hidden folders, build outputs, dependencies (`target`, `node_modules`, `vendor`, etc.), and config formats (TOML, YAML, JSON) are always skipped. |
| Excluded folders | Adding an exclusion removes indexed entries under that path within 30 seconds. The default list skips installed package and game trees. |
| Largest folders | Vendored source trees and generated reports often make up most of an index; excluding them speeds up indexing. |
| Model download | Downloaded from Hugging Face with every file verified against a pinned SHA-256 hash. **Verify** downloads only missing or corrupt files. Models load on the first query or indexing job. |
| Compiled cache | OpenVINO's compiled binary blobs for your device. Clearing it forces recompilation on the next load (~4 s on the NPU). |
| Device | On the NPU, indexing runs about 5x faster than on the CPU at roughly one-tenth of the energy per chunk. Changes apply within 30 seconds. |
| Unload after | Loaded models use ~553 MB of RAM and reload in ~1 s after unloading. |
| Indexing share | Maximum fraction of device time background indexing may use (halved on battery). Lowering it leaves more NPU headroom for face unlock and dictation. |
| Images | Searchable both visually and by visible text (for example, `"a cat"`, `"a receipt"`, or an invoice number). Takes ~1.3 s per image on the NPU plus 0.1–0.5 s for OCR. Requires the 671 MB vision encoder; turning images off removes them from the index. Press `Enter` to apply extension changes. |
| Documents | See [Documents](#documents) above. |
| Sessions | Indexes vinoWhisper saved transcripts and summaries; see [vinoWhisper sessions](#vinowhisper-sessions) above. Turning off drops them from the index. |
| Code | Matched only by queries starting with `code:`. Skips `.gitignore`d, vendored, generated, and secret-bearing files; see [Code](#code) above. Turning off drops them from the index. |
| Match threshold | Filters out results scoring below the threshold on the next query. For text, unrelated queries typically peak around 0.60 while true matches score 0.70+; images score lower (true matches ~0.61–0.69 vs. ~0.57–0.60 for non-matches). |
| Encryption | See [Encryption at rest](#encryption-at-rest). Applies immediately. Refused without a TPM unless **Host-key fallback** is enabled. Without `systemd-creds`, uses the desktop keyring (showing a Waiting row and **Unlock** button while locked). **New key** seals a fresh key and re-encrypts the index (useful after moving to a machine with a TPM). |
| Hardware | Runs a small probe model on the selected device in a separate process, matching `vinosearch doctor`. |
| Index on battery | Enabled by default. When disabled, indexing pauses on battery (checked every 10 s, showing `"Waiting for AC power"`). Indexing also pauses whenever `power-profiles-daemon` or `tuned-ppd` reports the `power-saver` profile (`"Paused (power saver)"`), regardless of this setting. Searches still work and pending changes resume automatically once power returns. Audio and video indexing always waits for AC power and starts within 10 s of plugging in. |
| Search places | Independent toggles for KRunner (shared with Plasma's application launcher), GNOME Shell, and a standalone search-window app-menu entry (off by default; `vinosearch-settings --search`). KRunner updates immediately; GNOME Shell takes effect on the next login. |
| Search service | Starts at login, watches folders, and serves queries. Enabling it also activates search for the current desktop; disabling it removes all search integrations while leaving the index and models intact. |

**Estimating folder cost ([#112](https://github.com/karanshukla/vinoSearch/issues/112)):** Before adding a folder, the UI estimates file count, indexing duration, disk size, and the five slowest subfolders (each with an **Exclude** button). `vinosearch estimate DIR` prints the same breakdown, and `vinosearch index` summarizes it on one line (`"~4,200 files, ~35 min, ~180 MB"`) and prompts interactively unless `--yes` is passed. Estimation uses the metadata-only walk (applying exclusions, format filters, and size limits, skipping unchanged indexed files). Duration uses the NPU rates (28.6 chunks/s, 1.3 s/image) and size uses 470 MB per 113k chunks, estimating 1 chunk per 800 bytes for text (8% conservative over 30k real chunks) and conservative text ratios for documents (25% of PDF size, 50% for Office and EPUB files). Byte-identical duplicates are counted in full even though only one copy is embedded.

**Largest folders:** Displays up to 8 non-overlapping subfolders within indexed roots, largest first. A folder yields to a subfolder containing at least 75% of its chunks so copied source trees highlight their actual bulk. Refreshes automatically once indexing settles.

The settings window currently requires Wayland (matching vinoWhisper). Clicking vinoSearch's icon next to GNOME Shell search results opens the search window.

## Query latency

Warm queries initially ran at p50 85 ms on the benchmark index. Latency dropped to 26 ms through five optimizations (in order of impact): removing a per-query `O(n)` `COUNT(*)`, fixing a dot-product loop that failed to auto-vectorize, replacing a full sort with partial selection for the top 40 hits, storing in-memory vectors as `int8` instead of `f32` (top-result scores differ by at most 0.001), and shrinking the static query shape from 64 tokens to 32. Measure end-to-end latency through the installed runner with `bench/query_latency.sh`.

## Runtime

OpenVINO is installed per user into `~/.local/lib/vinosearch/openvino` via [ovfetch](https://github.com/karanshukla/ovfetch) (0.6.0 or newer):

```bash
scripts/install-runtime.sh   # downloads Intel's openvino wheel (~139 MB, ~17 s, no compilation)
vinosearch doctor            # checks NPU, driver, and runtime, then compiles and runs a probe model
```

- **Device parity checks ([#76](https://github.com/karanshukla/vinoSearch/issues/76)):** Hardware accelerators can miscompile a graph without raising an error (for example, the INT8 vision encoder produced 0.67 cosine similarity against CPU on the NPU). Whenever a model stage (query, document, or image) compiles freshly on a non-CPU device or under a new OpenVINO version, vinoSearch embeds a fixed input and compares it against CPU reference vectors stored in `src/ml/parity/`. Any stage scoring below 0.995 falls back to the CPU; healthy NPU stages score 0.9995 (document) to 0.99998 (image). Parity results are cached beside the compiled blobs and displayed by `vinosearch doctor`. When updating a model revision, regenerate references with `cargo test --release write_parity_references -- --ignored`.
- **Dynamic loading and version alignment:** Because vinoSearch invokes OpenVINO directly (without ONNX Runtime), it runs `ovfetch install --openvino-only` to fetch the newest OpenVINO release supported by your NPU driver (2026.4.1 here), matching vinoAuthFace and vinoWhisper so processes share a compatible driver/runtime pair (an NPU fault resets the device for all active processes). Pass `doctor --openvino-dir <prefix>` to test another prefix. The runtime loads dynamically via `openvino`'s `runtime-linking` feature without build-time linking or rpaths. Because `ovfetch`'s runtime includes only the ONNX frontend, all models must be ONNX.
- **User-space NPU compiler:** Some distributions package the NPU driver without its compiler (such as Fedora's `intel-npu-driver` 1.32.0 RPM), causing the NPU to enumerate but fail every compile with `ZE_RESULT_ERROR_UNSUPPORTED_FEATURE`. When `ovfetch detect` spots this, `scripts/install-runtime.sh` runs `ovfetch npu install` (0.6.0+), installing Intel's compiler or driver libraries into `~/.local/lib/ovfetch-npu` (verified against ovfetch's ledger, no root required). Because those libraries sit outside the standard linker path, vinoSearch preloads each entry in `ovfetch-preload.txt` in order with `RTLD_GLOBAL` (`runtime::preload_npu`) after loading OpenVINO and before opening the NPU. `vinosearch doctor` prints the active prefix when used.

## Text model benchmark

Measured with `bench/` on BEIR SciFact (nDCG@10, documents truncated to 256 tokens), Core 5 320 NPU, OpenVINO 2026.4.1, 2026-10-18:

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

Across all models, the NPU indexes 4.5× to 11× faster than the CPU at roughly half the power. Raw results are stored in `bench/results/`.

## Ranking evals

While `bench/` evaluates raw embedding models, `vinosearch eval` tests end-to-end retrieval including chunking, `min_score` thresholds, per-file deduplication, and exact-word boosting ([#107](https://github.com/karanshukla/vinoSearch/issues/107)). `scripts/make-testbed.sh` generates a 46-file corpus (notes, code, French and Hindi text, PDFs, `.docx`, `.xlsx`, `.pptx`, RTF, Word 97 `.doc`, CSVs, and two drawn images) with near-miss distractors. `vinosearch eval` builds a temporary index, runs `evals/queries.toml` through KRunner's ranking path, and reports recall@1, recall@5, and MRR overall and per category (or `--json`):

```bash
scripts/make-testbed.sh /tmp/testbed
vinosearch --device CPU eval /tmp/testbed
```

The command exits non-zero if any overall metric drops below the thresholds in `evals/queries.toml` (overridable via `--min-recall-at-1`, `--min-recall-at-5`, and `--min-mrr`). CI runs this suite on CPU; image queries require `--images` and the vision encoder and are run manually.

CPU baseline (2026-10-25, 55 queries, 52 chunks):

| Category | Queries | recall@1 | recall@5 | MRR |
|---|---|---|---|---|
| keyword | 6 | 1.000 | 1.000 | 1.000 |
| paraphrase | 18 | 0.778 | 0.889 | 0.833 |
| section | 5 | 1.000 | 1.000 | 1.000 |
| identifier | 6 | 1.000 | 1.000 | 1.000 |
| cross-language | 9 | 0.889 | 1.000 | 0.944 |
| document | 11 | 1.000 | 1.000 | 1.000 |
| overall | 55 | 0.909 | 0.964 | 0.936 |

All misses are paraphrase queries: two fall below the 0.65 threshold, two rank a sibling retry function higher, and one English query about notice periods ranks the English moving checklist above the French lease. CPU evaluation gates PRs because chunking, thresholds, deduplication, and exact-word logic are device-independent. NPU vectors agree with CPU vectors at 0.9995–0.99998 cosine (`parity.rs`), shifting scores by ~0.001 typically (0.03 worst case); since 6 of 55 queries sit within 0.01 of a rival or threshold, CI thresholds leave a margin of two queries.

## NPU gotchas

Observed on OpenVINO 2026.4.1 and NPU driver 1.35.0:

- Static batch sizes greater than 1 mix rows across batch items. Running batch size 1 with multiple concurrent in-flight requests avoids the bug and runs ~6× faster.
- `com.microsoft.RotaryEmbedding` with offset-form `position_ids` converts incorrectly in OpenVINO ([#1](https://github.com/karanshukla/vinoSearch/issues/1)); `bench/patch_rope.py` rewrites the graph.
- OpenVINO rejects ONNX external weights accessed through Hugging Face cache symlinks; download models into a plain directory.
- The published vision encoder graph uses dynamic output shapes (`NonZero` into `GatherND`) that the NPU compiler rejects. [arankays/embeddinggemma-2-vision-static-onnx](https://huggingface.co/arankays/embeddinggemma-2-vision-static-onnx) provides a static-shape rewrite. Because its INT8 build produces inaccurate NPU outputs (0.67 cosine against CPU), it ships in fp32.

## Demos

The SVG animations in the README (`docs/media/krunner.svg` and `docs/media/settings.svg`) are generated programmatically using fictional filenames, snippets, and counts. The sample dog photo is by [Rafaëlla Waasdorp](https://unsplash.com/@krafaella_) on [Unsplash](https://unsplash.com/license).
