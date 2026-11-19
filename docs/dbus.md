---
title: D-Bus interface
nav_order: 8
---

# D-Bus interface

The runner owns `dev.vinosearch.Runner` on the session bus. It starts at login via XDG autostart, or on demand via D-Bus activation (`vinosearch krunner --install` writes both files), so calling any method below activates it automatically. The ML model is not loaded until a query or indexing job requires it.

Bus names and `Status` keys are defined as constants in `vinosearch_shared::dbus`; prefer those constants over raw string literals.

| Object | Interface | Purpose |
|---|---|---|
| `/runner` | `org.kde.krunner1` | KRunner (`Match`, `Actions`, `Run`, `Teardown`) and `vinosearch menu` (`Match`) |
| `/gnome` | `org.gnome.Shell.SearchProvider2` | GNOME Shell overview search |
| `/control` | `dev.vinosearch.Control1` | Settings window and CLI control |

## `org.kde.krunner1`

| Method | Description |
|---|---|
| `Actions() → a(sss)` | Returns available match actions: `open-folder` (Open containing folder) and `show-all` (Show all results). |
| `Match(s query)` | Searches the index; queries starting with `code:` search source code only ([#110](https://github.com/karanshukla/vinoSearch/issues/110)). Each match includes `subtext`, `urls`, `category`, and `actions` (`open-summary` or `open-transcript` on vinoWhisper sessions, in addition to `open-folder` and `show-all`), plus `line` (`u`) for a text file's matching line or `page` (`u`) for a PDF's matching page (used by `vinosearch menu`). |
| `Run(s matchId, s actionId)` | Rejects IDs that do not correspond to an indexed file. The default action opens the file at the match (see below). On a session transcript whose recording is indexed, it plays the recording from the matched `[MM:SS]` while `open-transcript` opens the transcript text instead ([#188](https://github.com/karanshukla/vinoSearch/issues/188)). `open-folder` opens the parent directory via `xdg-open`. `show-all` launches `vinosearch-settings --search` with the last `Match` query when installed alongside `vinosearch`, regardless of `matchId`. |

### Opening at the match

- **Text files:** Open at the matching line in the configured `editor`, or in the system default app (`xdg-mime query default`) when it is Visual Studio Code, Kate, KWrite, GNOME Text Editor, gedit, Zed, or a JetBrains IDE.
- **PDFs:** Open at the matching page when the default viewer is Okular, Evince, Papers, or Zathura.
- **Audio and video:** Play from the matching second in the configured `player`, or in the default app when it is mpv (`--start=`), VLC (`--start-time=`), or Celluloid (`--mpv-start=`) ([#188](https://github.com/karanshukla/vinoSearch/issues/188)). Haruna, Totem, and Showtime do not support a start-time CLI flag (verified from source on 2026-11-11: Haruna only accepts `--ytdl-format-selection`, Showtime only `--new-window`, and Totem's hidden `--seek` flag is unused), so they receive the file path alone.
- **vinoWhisper sessions:** If a session was transcribed from a recording that is also indexed, it plays that recording from the matched `[MM:SS]`; the recording is matched via the SHA-256 hash stored in the session's attestation.
- **Other files:** Terminal and Flatpak apps, slides, spreadsheets, HTML files, and images open via `xdg-open`. Match positions are cached from the most recent search, so activating an ID from an older search opens the file at the beginning.

## `org.gnome.Shell.SearchProvider2`

Runs the same search logic as KRunner's `Match`: best chunk per file meeting its format's `min_score`, returning the top 5 text files followed by the top 5 images (or only the top 5 code files for queries starting with `code:`). Result IDs are file paths.

| Method | Description |
|---|---|
| `GetInitialResultSet(as terms)` | Joins `terms` with spaces and searches. Queries shorter than 3 characters return an empty set, matching KRunner's 3-character minimum. |
| `GetSubsearchResultSet(as previous, as terms)` | Runs a fresh semantic search, since semantic results do not simply narrow as additional words are typed. |
| `GetResultMetas(as ids)` | Returns `id`, `name` (filename), `description` (directory, line, and snippet; directory only for images), and `gicon` (MIME icon from `shared-mime-info` such as `text-markdown` or `image-png`, falling back to `text-x-generic` or `image-x-generic`). IDs from older searches use the path as description. |
| `ActivateResult(s id, as terms, u timestamp)` | Opens the file at the match, using the rules under [Opening at the match](#opening-at-the-match). |
| `LaunchSearch(as terms, u timestamp)` | Opens the search window with `terms` joined by spaces (`vinosearch-settings --search`), if `vinosearch-settings` is installed alongside `vinosearch`. |

Because GNOME Shell only reads search providers from system data directories and ignores `~/.local/share`, enabling GNOME integration (`vinosearch integrate gnome`, run automatically by `krunner --install` in a GNOME session) writes the provider file to `~/.local/share/vinosearch/xdg/gnome-shell/search-providers/` and appends that directory to `XDG_DATA_DIRS` in `~/.config/environment.d/60-vinosearch.conf` (similar to how Flatpak exports user apps).

It also ensures `~/.local/share/applications/dev.vinosearch.desktop` exists and is not hidden, which GNOME Shell requires before enabling the provider. Changes take effect on the next login. Running `vinosearch integrate gnome --off` removes the provider and `environment.d` file; the `.desktop` entry is shared by the app menu across desktops and is only removed by `krunner --uninstall`.

## `dev.vinosearch.Control1`

All methods return promptly even while indexing or downloading: `Status` reads state published by the worker, `RecentFiles` queries SQLite directly, and mutating methods enqueue work. `Search` waits for the worker just like KRunner's `Match` (between files while indexing, or after loading the model if unloaded). `FileText` runs document extraction on a dedicated thread so large PDFs (which can take seconds to parse) do not block other calls. Errors are returned as `org.freedesktop.DBus.Error.Failed` with a human-readable message.

| Method | Signature | Description |
|---|---|---|
| `Status` | `() → a{sv}` | Returns current service state (see keys below). |
| `Reindex` | `(s path) → ()` | Queues a directory for indexing and starts watching it. Fails if the path is missing, not a directory, or excluded. Monitor `indexing` in `Status` for completion. |
| `Forget` | `(s path) → ()` | Stops indexing and watching a root directory and deletes its entries from the index. |
| `CheckNow` | `() → ()` | Compares every root against the index by mtime and size (the same check run hourly) and indexes differences. Runs after any active indexing job finishes; `last_checked` updates when complete. |
| `PauseIndexing` | `() → ()` | Pauses indexing after the current chunk (~35 ms on the NPU) and holds new indexing work until `ResumeIndexing`. Already-indexed files remain searchable and queries continue working. Does not persist across a runner restart. |
| `ResumeIndexing` | `() → ()` | Resumes indexing held by `PauseIndexing`. Unchanged files are skipped by mtime and size. |
| `Unload` | `() → ()` | Unloads the model immediately rather than waiting for the idle timeout. |
| `FetchModel` | `() → ()` | Downloads any missing or corrupt model files in the background (340 MB base text + OCR models, plus the 671 MB vision encoder when images or media are enabled, and the 295 MB audio encoder when media is enabled; each file is verified against a pinned SHA-256 and size). Fails if a download is already in progress. Track progress via `fetching`, `fetch_done`, `fetch_total`, and `fetch_error`. |
| `DeleteModel` | `() → ()` | Unloads and deletes all model files (text, vision, OCR, and audio). Fails if a download is running. Queries and indexing fail until models are downloaded again. |
| `ClearCache` | `() → ()` | Deletes OpenVINO's compiled-model cache. The next load recompiles (~4 s on the NPU). |
| `Quit` | `() → ()` | Releases the bus name and exits cleanly until the next login or D-Bus call. Used by `krunner --install` and `--uninstall`. Because the reply can race with process exit, callers should wait for the bus name to vanish. |
| `ApplyEncryption` | `() → ()` | Immediately reloads `[encryption]` from `config.toml` and encrypts or decrypts the index to match instead of waiting for the 30-second poll; also retries after a prior refusal ([#4](https://github.com/karanshukla/vinoSearch/issues/4)). Queued behind an active indexing job (which it interrupts). Results appear in `encrypted`, `seal`, and `encryption_error`. When waiting on a locked keyring (`seal_waiting`), re-triggers the keyring unlock prompt; if encryption is turned off while the key remains unavailable, rebuilds the index in plaintext. Fails if `config.toml` cannot be parsed. |
| `Rekey` | `() → ()` | Seals a new key and re-encrypts the index under it. Fails if the index is not currently encrypted. |
| `LargestFolders` | `(u limit) → a(st)` | Returns up to `limit` non-overlapping subfolders within indexed roots holding the most chunks as `(path, chunks)`, largest first (excluding roots themselves). A folder yields to a subfolder holding at least 3/4 of its chunks. Reads every chunk row, so call only when `chunks` changes rather than on every poll. |
| `RecentFiles` | `(u limit) → a(ssx)` | Returns up to `limit` (capped at 50, `dbus::recent::MAX`) indexed text files and documents (excluding images), newest first by recorded mtime, as `(path, kind, mtime_unix)`. `kind` is `text` or `document` (`dbus::recent::{TEXT, DOCUMENT}`). Deleted files are omitted. Requires no ML model, making it suitable for populating an initial list before typing. |
| `Search` | `(s query, u limit) → a(ssdssux)` | Full search for the search window ([#111](https://github.com/karanshukla/vinoSearch/issues/111)): best chunk per file meeting its format's `min_score`, up to `limit` (capped at 100, `dbus::results::MAX`) per kind in order: text, documents, vinoWhisper sessions, audio/video, then images (sorted best-first within each). Returns `(path, kind, score, snippet, unit, at, mtime_unix)`. `kind` is `text`, `document`, `session`, `media` ([#187](https://github.com/karanshukla/vinoSearch/issues/187)), or `image` (`dbus::results`; OCR text inside images reports `image` as in KRunner), or exclusively `code` for `code:` queries ([#110](https://github.com/karanshukla/vinoSearch/issues/110)). `snippet` is up to 140 characters (empty for photos); `unit` is `line`, `page`, `slide`, `chapter`, `row`, `paragraph`, `second` (for media and session transcripts with an indexed recording), or empty; `at` is 1-based (`0` if none; `second` is 0-based for `Play`). Missing files are omitted. Loads the model if unloaded. |
| `Open` | `(s path, u at) → ()` | Opens an indexed file at position `at` returned by `Search` (same rules as KRunner's `Run`; `0` opens at the start). Audio/video plays from second `at` in the configured `player` or mpv/VLC/Celluloid ([#188](https://github.com/karanshukla/vinoSearch/issues/188)); other players open without seeking. Fails if `path` is not indexed. |
| `Play` | `(s path, u second) → ()` | Plays an indexed audio/video file from `second`, or plays the underlying recording of an indexed vinoWhisper session ([#188](https://github.com/karanshukla/vinoSearch/issues/188)) matched by the attestation SHA-256 (requires media indexing enabled and the recording under an indexed folder). Fails if not indexed, if a session has no indexed recording, or for other file kinds. |
| `OpenFolder` | `(s path) → ()` | Opens the parent directory of an indexed file via `xdg-open`. Fails if `path` is not indexed. |
| `FileText` | `(s path) → s` | Returns the full extracted text of an indexed text file or document (for read-aloud). Text files return up to 1 MiB of UTF-8; documents run through the sandboxed `vinosearch extract` child (32 MiB file limit, 1 MiB text limit), separating pages/slides with blank lines, stripping PDF running headers/footers/page numbers, and repeating spreadsheet headers once per sheet. Fails for non-indexed files, images, non-UTF-8 text files, or unparseable documents. Scanned PDFs without a text layer return an empty string (their OCR text is stored in the index rather than re-extracted here). |

### `Status` keys

| Key | Type | Meaning |
|---|---|---|
| `model_loaded` | `b` | Whether the model is currently loaded in memory. |
| `indexing` | `b` | Whether a `Reindex` is queued/running or the worker is embedding watched file changes (`true` immediately once `Reindex` returns). |
| `paused` | `b` | Whether `PauseIndexing` is active (`indexing` is `false` while paused). |
| `power_hold` | `s` | Reason indexing is held for power ([#109](https://github.com/karanshukla/vinoSearch/issues/109)): `charger` (on battery with `index_on_battery = false`), `power_saver` (`power-profiles-daemon` or `tuned-ppd` reports `power-saver`), or empty. Behaves like pause except it resumes automatically when the condition clears (`indexing` is `false` while held). |
| `indexed_files` | `t` | Number of files embedded during the current indexing run (since the worker was last idle). |
| `indexing_rest` | `b` | Whether the worker is currently indexing remaining parts of a long document after its initial 30 parts ([#108](https://github.com/karanshukla/vinoSearch/issues/108)). |
| `partial` | `a(sstt)` | Up to 100 partially indexed documents, next-in-line first: `(path, unit, parts_done, parts_total)`. `unit` is `page`, `slide`, `chapter`, or `part` (3 chunks, roughly one printed page). |
| `device` | `s` | Active OpenVINO device (for example `NPU`). |
| `files` | `x` | Total files tracked in the index. |
| `chunks` | `x` | Total chunks in the index. |
| `text_files`, `text_chunks`, `document_files`, `document_chunks`, `image_files`, `code_files`, `code_chunks`, `session_files`, `session_chunks`, `media_files`, `media_chunks` | `x` | Per-format file and chunk counts ([#73](https://github.com/karanshukla/vinoSearch/issues/73)). Files count under the kind of their chunks (a skipped file with no chunks appears only in `files`). Each image contributes one pixel chunk plus one chunk per OCR text run ([#106](https://github.com/karanshukla/vinoSearch/issues/106)). |
| `sessions_unread` | `x` | Count of vinoWhisper sessions indexed without text due to an unsupported `format` version or unreadable bundle ([#143](https://github.com/karanshukla/vinoSearch/issues/143)). |
| `roots` | `as` | Canonical paths of indexed root directories. |
| `model_present` | `b` | Whether all required model files are present on disk (including the vision encoder when images/media are enabled and audio encoder when media is enabled). |
| `images_model_present` | `b` | Whether image models (vision encoder and OCR) are on disk regardless of whether images are enabled. Older services prior to media support ([#187](https://github.com/karanshukla/vinoSearch/issues/187)) omit this key (fall back to `model_present`). |
| `media_model_present` | `b` | Whether media models (vision and audio encoders) are on disk regardless of whether media indexing is enabled. |
| `ffmpeg` | `b` | Whether `ffmpeg` and `ffprobe` are available on the service's `PATH` (required for media indexing). Omitted by services prior to media support. |
| `fetching` | `b` | Whether a `FetchModel` download is currently running. |
| `fetch_done`, `fetch_total` | `t` | Bytes downloaded so far and total download size (for progress indicators). |
| `fetch_error` | `s` | Error message from the last failed download, or empty (cleared when a new download starts). |
| `index_bytes` | `t` | On-disk size of the SQLite index database, including WAL. |
| `model_bytes` | `t` | Total disk usage of all model files (text, vision, OCR, and audio), including partial downloads. |
| `cache_bytes` | `t` | Disk usage of OpenVINO's compiled-model cache (~270 MB per stage on the NPU). |
| `last_checked` | `t` | Unix timestamp when all roots were last compared against the index (at startup, hourly, after wake, after dropped inotify events, or on `CheckNow`); `0` before the first check. |
| `last_indexed` | `t` | Unix timestamp when an indexing job last embedded or removed a file; `0` if none since runner startup. |
| `no_decoder` | `x` | Count of HEIC, HEIF, and AVIF files skipped because `heif-convert` (`libheif`) is not installed ([#79](https://github.com/karanshukla/vinoSearch/issues/79)); resets to `0` once installed. |
| `model_changed` | `s` | Explanation when part of the index was dropped at startup for re-indexing because the text or image model changed ([#80](https://github.com/karanshukla/vinoSearch/issues/80)); empty once re-indexing finishes. |
| `encrypted` | `b` | Whether the on-disk index is encrypted ([#4](https://github.com/karanshukla/vinoSearch/issues/4)). |
| `seal` | `s` | Key sealing method (`dbus::seal`): `tpm` (TPM + systemd host key), `host` (host key alone, requires `allow_host_key`), `keyring` (desktop Secret Service keyring without TPM), `memory` (ephemeral in-memory key; index rebuilds on next start), or empty if unencrypted. |
| `tpm` | `b` | Whether systemd can seal a key to a TPM (`systemd-analyze has-tpm2`; evaluated on the host inside Flatpak), checked at runner startup. |
| `encryption_error` | `s` | Reason the last encryption change failed or was refused (such as missing TPM, systemd too old for `--user`, or another process holding the index); empty otherwise. The existing index is left untouched. |
| `seal_broken` | `s` | Reason a previously sealed key failed at startup (failed to unseal or open the database), triggering an index rebuild; empty otherwise. |
| `seal_waiting` | `s` | Reason the key is temporarily unavailable: locked keyring, dismissed or timed-out unlock prompt (1 minute), absent Secret Service, missing keyring item, or unreachable host `systemd-creds` in Flatpak. While waiting, the index is preserved, counts read `0`, searches return empty, and the runner retries every 30 seconds (or re-prompts on `ApplyEncryption`). Empty otherwise. |
| `new_seal` | `s` | Which `seal` backend will be used for a newly sealed key (`tpm` or `host` if `systemd-creds` is available, otherwise `keyring`), determined at runner startup. |

## Settings

Settings are not stored over D-Bus; they live in `~/.config/vinosearch/config.toml` (`vinosearch_shared::config::{default_path, Config}`), which the settings window edits directly:

```toml
exclude = ["~/Downloads"]
device = "NPU"
idle_unload_minutes = 5
index_busy_percent = 50
index_on_battery = true

[formats.text]
min_score = 0.65

[formats.images]
enabled = true
extensions = ["jpg", "png"]
min_score = 0.68

[formats.code]
enabled = true
min_score = 0.72

[formats.sessions]
enabled = true

[encryption]
enabled = true
allow_host_key = false
```

A running runner re-reads `config.toml` every 30 seconds and applies any changes:

| Key | When it takes effect |
|---|---|
| `exclude` (defaults to `config::DEFAULT_EXCLUDE` when omitted, and is not written back while unchanged) | Immediately (interrupting and restarting any active indexing job): newly excluded paths are dropped from the index and unwatched. |
| `device` | Unloads the current model and reloads on the new device on the next query or indexing job; `Status` reports the new device immediately. |
| `idle_unload_minutes` | Applies starting with the next idle wait (`0` keeps the model loaded). |
| `index_busy_percent` | Immediately, including mid-index: caps the fraction of device time background indexing may use (1–100, default `50`; halved on battery). |
| `index_on_battery` | Immediately, including mid-index: `false` pauses indexing on battery (checked every 10 s) and sets `power_hold` to `charger`. Default `true`. |
| `formats.images.enabled`, `formats.images.extensions` | Immediately (interrupting any active indexing job): unwanted images are dropped and roots are scanned for newly enabled extensions. Default off; default extensions are `jpg`, `jpeg`, `png`, `webp`, `gif`, `bmp`, `tif`, `tiff`, `heic`, `heif`, `avif`. |
| `formats.code.enabled` | Same as `formats.images.enabled`: drops code when disabled and scans roots when enabled. Default off. |
| `formats.sessions.enabled` | Same as `formats.images.enabled`, and automatically adds/removes vinoWhisper's session directories as roots: `~/.local/share/vinowhisper/transcripts` (`$XDG_DATA_HOME` outside Flatpak) and `~/.var/app/io.github.karanshukla.vinowhisper/data/vinowhisper/transcripts`. Default off. |
| `formats.text.min_score`, `formats.images.min_score`, `formats.code.min_score` | Applies on the next query: filters out results below the cosine threshold (defaults `0.65`, `0.68`, and `0.72`). |
| `encryption.enabled`, `encryption.allow_host_key` | Immediately (or on demand via `ApplyEncryption`), interrupting any active indexing job: converts the database in place without re-indexing. Default off. Without a TPM, enabling fails unless `allow_host_key = true`; without `systemd-creds`, falls back to the desktop keyring. |

CLI flags and environment variables (`--device`, `--idle-unload-minutes`, `VINOSEARCH_IDLE_UNLOAD_MINUTES`, or flags baked into the service by `krunner --install --device ...`) take precedence over `config.toml` until the runner is restarted without them. Unparseable config files or invalid `exclude` entries are ignored without modifying state; an invalid `idle_unload_minutes` keeps the existing timeout.

## Trying it

```bash
busctl --user call dev.vinosearch.Runner /control dev.vinosearch.Control1 Status
busctl --user call dev.vinosearch.Runner /control dev.vinosearch.Control1 Reindex s ~/Documents
busctl --user introspect dev.vinosearch.Runner /control
```
