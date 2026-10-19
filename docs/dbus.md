# D-Bus interface

The runner owns `dev.vinosearch.Runner` on the session bus. D-Bus starts it on first use (`vinosearch krunner --install` writes the activation file), so calling any method below is enough to bring it up; it does not load the model until a query or an indexing job needs it.

Names and `Status` keys are constants in `vinosearch_shared::dbus`; use those rather than string literals.

| Object | Interface | For |
|---|---|---|
| `/runner` | `org.kde.krunner1` | KRunner (`Match`, `Actions`, `Run`, `Teardown`) |
| `/gnome` | `org.gnome.Shell.SearchProvider2` | GNOME Shell's overview search |
| `/control` | `dev.vinosearch.Control1` | the settings app |

## `org.gnome.Shell.SearchProvider2`

The same search KRunner's `Match` runs: the best chunk per file, files scoring at least 0.65, top 5. Result ids are file paths.

| Method | What it does |
|---|---|
| `GetInitialResultSet(as terms)` | Joins the terms with spaces and searches. Under 3 characters returns nothing, as KRunner's plugin waits for 3 letters. |
| `GetSubsearchResultSet(as previous, as terms)` | Searches afresh: a semantic match does not narrow as the query grows. |
| `GetResultMetas(as ids)` | `id`, `name` (file name), `description` (directory, line and snippet), `gicon` (`text-x-generic`). Ids from an older search get the path as description. |
| `ActivateResult(s id, as terms, u timestamp)` | `xdg-open` the file. |
| `LaunchSearch(as terms, u timestamp)` | Nothing yet; the settings app (#5) will be the target. |

GNOME Shell only reads search providers from the system data dirs, never `~/.local/share`, so `krunner --install` writes the provider file to `~/.local/share/vinosearch/xdg/gnome-shell/search-providers/` and appends that directory to `XDG_DATA_DIRS` in `~/.config/environment.d/60-vinosearch.conf`, the way Flatpak exports per-user apps. It also writes `~/.local/share/applications/dev.vinosearch.desktop`, which GNOME Shell needs to show the provider (a hidden entry disables it). Results appear from the next login; `--uninstall` removes all three.

## `dev.vinosearch.Control1`

Every method returns promptly, including while the runner is indexing or downloading: `Status` reads state the worker publishes, and the others only queue work. Errors are `org.freedesktop.DBus.Error.Failed` with a human-readable message.

| Method | Signature | What it does |
|---|---|---|
| `Status` | `() → a{sv}` | Current state, keys below. |
| `Reindex` | `(s path) → ()` | Index a directory and watch it from now on. Fails if the path is missing, not a directory, or excluded. Queued; watch `indexing` in `Status` for it to finish. |
| `Forget` | `(s path) → ()` | Stop indexing and watching a root, and drop what was indexed under it. |
| `CheckNow` | `() → ()` | Compare every root with the index by mtime and size, as the watcher does hourly, and index what differs. Runs once any indexing job has finished; `last_checked` in `Status` moves when it has. |
| `Unload` | `() → ()` | Free the model now instead of after the idle timeout. |
| `FetchModel` | `() → ()` | Download the model, or whatever of it is missing or corrupt, in the background (308 MB, every file checked against a pinned SHA-256 and size). Fails if a download is already running. Progress in `fetching`, `fetch_done`, `fetch_total`, `fetch_error`. |
| `DeleteModel` | `() → ()` | Unload the model and delete its files. Fails while a download runs. Queries and indexing fail until it is fetched again. |
| `ClearCache` | `() → ()` | Delete OpenVINO's compiled-model cache. The next model load recompiles (~4 s on the NPU). |

### `Status` keys

| Key | Type | Meaning |
|---|---|---|
| `model_loaded` | `b` | The model is in memory. |
| `indexing` | `b` | A `Reindex` is queued or running, or the worker is embedding changes the watcher found. True from the moment `Reindex` returns. |
| `indexed_files` | `t` | Files embedded so far by the current indexing job. |
| `device` | `s` | OpenVINO device the model runs on, e.g. `NPU`. |
| `files` | `x` | Files in the index. |
| `chunks` | `x` | Chunks in the index. |
| `roots` | `as` | Indexed roots, canonical paths. |
| `model_present` | `b` | Every model file is on disk. |
| `fetching` | `b` | A `FetchModel` download is running. |
| `fetch_done`, `fetch_total` | `t` | Bytes of the model in place so far, and in total, for a progress bar. |
| `fetch_error` | `s` | Why the last download failed; empty when it did not, cleared when one starts. |
| `index_bytes` | `t` | The index database on disk, WAL included. |
| `model_bytes` | `t` | The model on disk. |
| `cache_bytes` | `t` | OpenVINO's compiled-model cache on disk (one blob per stage, ~270 MB each on the NPU). |
| `last_checked` | `t` | Unix seconds when every root was last compared with the index (at startup, hourly, after a resume, after dropped inotify events, or on `CheckNow`); 0 before the first. |
| `last_indexed` | `t` | Unix seconds when an indexing job last embedded or removed a file; 0 for not since the runner started. |

## Settings

Settings are not on D-Bus. They live in `~/.config/vinosearch/config.toml` (`vinosearch_shared::config::{default_path, Config}`), which the settings app edits directly:

```toml
exclude = ["~/Downloads"]
device = "NPU"
idle_unload_minutes = 5
```

A running runner re-reads the file every 30 seconds and applies what changed:

| Key | Takes effect |
|---|---|
| `exclude` | at once: anything newly excluded is dropped from the index and no longer watched for new directories |
| `device` | the model is unloaded, then loaded on the new device by the next query or indexing job; `Status` reports the new device straight away |
| `idle_unload_minutes` | from the next wait for a job (0 keeps the model loaded) |

A setting given as a flag or environment variable (`--device`, `--idle-unload-minutes`, `VINOSEARCH_IDLE_UNLOAD_MINUTES`, or baked into the service by `krunner --install --device ...`) wins over the file, so file changes to it are ignored until the runner restarts without the flag. A file that fails to parse, or has an invalid `exclude`, changes nothing; an invalid `idle_unload_minutes` keeps the current timeout.

## Trying it

```bash
busctl --user call dev.vinosearch.Runner /control dev.vinosearch.Control1 Status
busctl --user call dev.vinosearch.Runner /control dev.vinosearch.Control1 Reindex s ~/Documents
busctl --user introspect dev.vinosearch.Runner /control
```
