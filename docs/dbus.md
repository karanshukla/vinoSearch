# D-Bus interface

The runner owns `dev.vinosearch.Runner` on the session bus. D-Bus starts it on first use (`vinosearch krunner --install` writes the activation file), so calling any method below is enough to bring it up; it does not load the model until a query or an indexing job needs it.

Names and `Status` keys are constants in `vinosearch_shared::dbus`; use those rather than string literals.

| Object | Interface | For |
|---|---|---|
| `/runner` | `org.kde.krunner1` | KRunner (`Match`, `Actions`, `Run`, `Teardown`) |
| `/control` | `dev.vinosearch.Control1` | the settings app |

## `dev.vinosearch.Control1`

Every method returns promptly, including while the runner is indexing: `Status` reads state the worker publishes, and the others only queue work. Errors are `org.freedesktop.DBus.Error.Failed` with a human-readable message.

| Method | Signature | What it does |
|---|---|---|
| `Status` | `() → a{sv}` | Current state, keys below. |
| `Reindex` | `(s path) → ()` | Index a directory and watch it from now on. Fails if the path is missing, not a directory, or excluded. Queued; watch `indexing` in `Status` for it to finish. |
| `Forget` | `(s path) → ()` | Stop indexing and watching a root, and drop what was indexed under it. |
| `Unload` | `() → ()` | Free the model now instead of after the idle timeout. |

### `Status` keys

| Key | Type | Meaning |
|---|---|---|
| `model_loaded` | `b` | The model is in memory. |
| `indexing` | `b` | The worker is embedding files (a `Reindex`, or changes the watcher found). |
| `device` | `s` | OpenVINO device the model runs on, e.g. `NPU`. |
| `files` | `x` | Files in the index. |
| `chunks` | `x` | Chunks in the index. |
| `roots` | `as` | Indexed roots, canonical paths. |

## Settings

Settings are not on D-Bus. They live in `~/.config/vinosearch/config.toml` (`vinosearch_shared::config::{default_path, Config}`), which the settings app edits directly:

```toml
exclude = ["~/Downloads"]
device = "NPU"
idle_unload_minutes = 5
```

A running runner re-reads the file every 30 seconds and applies `exclude` changes, dropping anything newly excluded from the index. `device` and `idle_unload_minutes` take effect the next time the runner starts.

## Trying it

```bash
busctl --user call dev.vinosearch.Runner /control dev.vinosearch.Control1 Status
busctl --user call dev.vinosearch.Runner /control dev.vinosearch.Control1 Reindex s ~/Documents
busctl --user introspect dev.vinosearch.Runner /control
```
