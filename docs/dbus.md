# D-Bus interface

The runner owns `dev.vinosearch.Runner` on the session bus. D-Bus starts it on first use (`vinosearch krunner --install` writes the activation file), so calling any method below is enough to bring it up; it does not load the model until a query or an indexing job needs it.

Names and `Status` keys are constants in `vinosearch_shared::dbus`; use those rather than string literals.

| Object | Interface | For |
|---|---|---|
| `/runner` | `org.kde.krunner1` | KRunner (`Match`, `Actions`, `Run`, `Teardown`), and `vinosearch menu` (`Match`) |
| `/gnome` | `org.gnome.Shell.SearchProvider2` | GNOME Shell's overview search |
| `/control` | `dev.vinosearch.Control1` | the settings app |

## `org.kde.krunner1`

| Method | What it does |
|---|---|
| `Actions() → a(sss)` | `open-folder` (Open containing folder). |
| `Match(s query)` | The search below. Each match sets `subtext`, `urls` and `category`, and `line` (u) for a text file's matching line or `page` (u) for a PDF's matching page, which `vinosearch menu` opens at. |
| `Run(s matchId, s actionId)` | Refuses an id that is not an indexed file. The default action opens the file at the match, as below, `open-folder` `xdg-open`s its directory. |

### Opening at the match

A text file opens at its matching line in the `editor` from the config file, else in the default app for its type (`xdg-mime query default`) when that is Visual Studio Code, Kate, KWrite, GNOME Text Editor, gedit, Zed or a JetBrains IDE. A PDF opens at its matching page when the default viewer is Okular, Evince, Papers or Zathura. Anything else, a terminal or Flatpak app included, and slides, sheets, HTML and images, are `xdg-open`ed as before. The place comes from the last search, so an id from an older one opens at the top.

## `org.gnome.Shell.SearchProvider2`

The same search KRunner's `Match` runs: the best chunk per file, files scoring at least their kind's `min_score`, the top 5 text files, then the top 5 images. Result ids are file paths.

| Method | What it does |
|---|---|
| `GetInitialResultSet(as terms)` | Joins the terms with spaces and searches. Under 3 characters returns nothing, as KRunner's plugin waits for 3 letters. |
| `GetSubsearchResultSet(as previous, as terms)` | Searches afresh: a semantic match does not narrow as the query grows. |
| `GetResultMetas(as ids)` | `id`, `name` (file name), `description` (directory, line and snippet; the directory alone for an image), `gicon` (the file type's icon from shared-mime-info, such as `text-markdown` or `image-png`, falling back to `text-x-generic` or `image-x-generic`). Ids from an older search get the path as description. |
| `ActivateResult(s id, as terms, u timestamp)` | Opens the file at the match, as below. |
| `LaunchSearch(as terms, u timestamp)` | Opens the settings window, if `vinosearch-settings` is installed beside `vinosearch`. |

GNOME Shell only reads search providers from the system data dirs, never `~/.local/share`, so turning the GNOME place on (`vinosearch integrate gnome`, which `krunner --install` does in a GNOME session) writes the provider file to `~/.local/share/vinosearch/xdg/gnome-shell/search-providers/` and appends that directory to `XDG_DATA_DIRS` in `~/.config/environment.d/60-vinosearch.conf`, the way Flatpak exports per-user apps. It also makes sure `~/.local/share/applications/dev.vinosearch.desktop` is there, which GNOME Shell needs to show the provider (a hidden entry disables it). Results appear from the next login; `integrate gnome --off` removes the provider and the environment.d file. The desktop entry is the app menu's on every desktop, so `krunner --install` writes it and only `krunner --uninstall` removes it.

## `dev.vinosearch.Control1`

Every method returns promptly, including while the runner is indexing or downloading: `Status` reads state the worker publishes, `RecentFiles` reads the index, and the others only queue work. `FileText` is the exception: a document's text takes as long as parsing it (seconds for a big PDF), on a thread of its own, so other calls are answered meanwhile. Errors are `org.freedesktop.DBus.Error.Failed` with a human-readable message.

| Method | Signature | What it does |
|---|---|---|
| `Status` | `() → a{sv}` | Current state, keys below. |
| `Reindex` | `(s path) → ()` | Index a directory and watch it from now on. Fails if the path is missing, not a directory, or excluded. Queued; watch `indexing` in `Status` for it to finish. |
| `Forget` | `(s path) → ()` | Stop indexing and watching a root, and drop what was indexed under it. |
| `CheckNow` | `() → ()` | Compare every root with the index by mtime and size, as the watcher does hourly, and index what differs. Runs once any indexing job has finished; `last_checked` in `Status` moves when it has. |
| `PauseIndexing` | `() → ()` | Stop indexing after the chunk in progress (~35 ms on the NPU) and hold new indexing work until `ResumeIndexing`. Every file finished so far stays in the index; queries keep working. Not kept across a runner restart, which indexes as usual. |
| `ResumeIndexing` | `() → ()` | Run the indexing held since `PauseIndexing`. Unchanged files are skipped by mtime and size, so it carries on where it stopped. |
| `Unload` | `() → ()` | Free the model now instead of after the idle timeout. |
| `FetchModel` | `() → ()` | Download the model, or whatever of it is missing or corrupt, in the background (308 MB, plus the 671 MB vision encoder while images are on; every file checked against a pinned SHA-256 and size). Fails if a download is already running. Progress in `fetching`, `fetch_done`, `fetch_total`, `fetch_error`. |
| `DeleteModel` | `() → ()` | Unload the model and delete its files. Fails while a download runs. Queries and indexing fail until it is fetched again. |
| `ClearCache` | `() → ()` | Delete OpenVINO's compiled-model cache. The next model load recompiles (~4 s on the NPU). |
| `LargestFolders` | `(u limit) → a(st)` | The folders inside the indexed roots holding the most chunks, as (path, chunks), biggest first: at most `limit`, none inside another, never a root itself. A folder gives way to a subfolder holding 3/4 of its chunks. It reads every chunk row, so call it when `chunks` changes, not every poll. |
| `RecentFiles` | `(u limit) → a(ssx)` | Indexed text files and documents (never images), newest first by modification time as the index recorded it, as (path, kind, mtime in Unix seconds). Kind is `text` or `document` (`dbus::recent::{TEXT, DOCUMENT}`). At most `limit`, capped at 50 (`dbus::recent::MAX`); files gone since they were indexed are left out. No model is used, so it answers before any search: for a list to pick from before the user types. |
| `FileText` | `(s path) → s` | The whole text of an indexed text file or document, for reading aloud. A text file's UTF-8 contents (1 MiB at most, as indexing takes); a document's text extracted as indexing does, in the sandboxed `vinosearch extract` child (32 MiB file, 1 MiB of text), pages and slides apart by a blank line, a table's header once per sheet. Fails for a path that is not an indexed file, an image, a file that is not UTF-8, or a document that does not parse. A scanned PDF with no text layer gives an empty string. |

### `Status` keys

| Key | Type | Meaning |
|---|---|---|
| `model_loaded` | `b` | The model is in memory. |
| `indexing` | `b` | A `Reindex` is queued or running, or the worker is embedding changes the watcher found. True from the moment `Reindex` returns. |
| `paused` | `b` | `PauseIndexing` was called and `ResumeIndexing` not since. `indexing` is false while paused. |
| `indexed_files` | `t` | Files embedded so far by the current indexing job. |
| `device` | `s` | OpenVINO device the model runs on, e.g. `NPU`. |
| `files` | `x` | Files in the index. |
| `chunks` | `x` | Chunks in the index. |
| `text_files`, `text_chunks`, `document_files`, `document_chunks`, `image_files` | `x` | The same per format ([#73](https://github.com/karanshukla/vinoSearch/issues/73)); a file counts under the kind of its chunks, so one recorded with none (a scan, a photo with no decoder) is in `files` only. An image is one chunk. |
| `roots` | `as` | Indexed roots, canonical paths. |
| `model_present` | `b` | Every model file is on disk, the vision encoder's included while images are on. |
| `fetching` | `b` | A `FetchModel` download is running. |
| `fetch_done`, `fetch_total` | `t` | Bytes of the model in place so far, and in total, for a progress bar. |
| `fetch_error` | `s` | Why the last download failed; empty when it did not, cleared when one starts. |
| `index_bytes` | `t` | The index database on disk, WAL included. |
| `model_bytes` | `t` | The model on disk, the vision encoder included. |
| `cache_bytes` | `t` | OpenVINO's compiled-model cache on disk (one blob per stage, ~270 MB each on the NPU). |
| `last_checked` | `t` | Unix seconds when every root was last compared with the index (at startup, hourly, after a resume, after dropped inotify events, or on `CheckNow`); 0 before the first. |
| `last_indexed` | `t` | Unix seconds when an indexing job last embedded or removed a file; 0 for not since the runner started. |
| `no_decoder` | `x` | HEIC, HEIF and AVIF files skipped because no HEIF decoder (libheif's `heif-convert`) is installed ([#79](https://github.com/karanshukla/vinoSearch/issues/79)); 0 once one is. |
| `model_changed` | `s` | Why part of the index was dropped at this start to be indexed again: the text or image model is not the one that made those vectors ([#80](https://github.com/karanshukla/vinoSearch/issues/80)). Empty otherwise, and again once that indexing has finished. |

## Settings

Settings are not on D-Bus. They live in `~/.config/vinosearch/config.toml` (`vinosearch_shared::config::{default_path, Config}`), which the settings app edits directly:

```toml
exclude = ["~/Downloads"]
device = "NPU"
idle_unload_minutes = 5
index_busy_percent = 50

[formats.text]
min_score = 0.65

[formats.images]
enabled = true
extensions = ["jpg", "png"]
min_score = 0.63
```

A running runner re-reads the file every 30 seconds and applies what changed:

| Key | Takes effect |
|---|---|
| `exclude` (missing: `config::DEFAULT_EXCLUDE`, and not written back while unchanged) | at once, stopping an indexing job to apply it (the job runs again after): anything newly excluded is dropped from the index and no longer watched for new directories |
| `device` | the model is unloaded, then loaded on the new device by the next query or indexing job; `Status` reports the new device straight away |
| `idle_unload_minutes` | from the next wait for a job (0 keeps the model loaded) |
| `index_busy_percent` | at once, mid-index included: most of the device's time background indexing takes, 1 to 100, default 50, half that on battery |
| `formats.images.enabled`, `formats.images.extensions` | at once, stopping an indexing job to apply them: images no longer wanted are dropped from the index, and every root is checked for the ones now wanted. Default off, and `extensions` defaults to jpg, jpeg, png, webp, gif, bmp, tif, tiff, heic, heif, avif |
| `formats.text.min_score`, `formats.images.min_score` | from the next query: KRunner and GNOME leave out results of that kind scoring lower (cosine, defaults 0.65 and 0.6) |

A setting given as a flag or environment variable (`--device`, `--idle-unload-minutes`, `VINOSEARCH_IDLE_UNLOAD_MINUTES`, or baked into the service by `krunner --install --device ...`) wins over the file, so file changes to it are ignored until the runner restarts without the flag. A file that fails to parse, or has an invalid `exclude`, changes nothing; an invalid `idle_unload_minutes` keeps the current timeout.

## Trying it

```bash
busctl --user call dev.vinosearch.Runner /control dev.vinosearch.Control1 Status
busctl --user call dev.vinosearch.Runner /control dev.vinosearch.Control1 Reindex s ~/Documents
busctl --user introspect dev.vinosearch.Runner /control
```
