---
title: Installing
nav_order: 2
---

# Installing

## Requirements

- Linux with an Intel NPU (Core Ultra or newer) and the [Intel Linux NPU driver](https://github.com/intel/linux-npu-driver). On systems without an NPU (such as AMD or older Intel CPUs), `install.sh` falls back to the CPU at lower speed; pass `--device CPU` to select CPU explicitly.
- KDE Plasma or GNOME on Wayland. Sway, Hyprland, COSMIC, and other Wayland desktops work via `vinosearch menu` with a dmenu-style launcher (`fuzzel`, `wofi`, `rofi`, `tofi`, or `bemenu`). The settings window requires Wayland.
- Rust, a C compiler, and OpenSSL development headers are only required when building from source.

## Release download

```bash
curl -L https://github.com/karanshukla/vinoSearch/releases/latest/download/vinosearch-x86_64-linux.tar.gz | tar xz
vinosearch/scripts/install.sh --index ~/Documents
```

This installs prebuilt binaries into `~/.local/bin` (`x86_64`, glibc 2.39+), downloads OpenVINO and the 340 MB base models, indexes the directories you pass (`--index` can be repeated, or the script will prompt you), and registers vinoSearch with your desktop's search (KRunner on Plasma, GNOME Shell on GNOME) as a login service. Already-completed steps are skipped, so re-running from a newer archive upgrades in place. If a prerequisite such as the NPU driver is missing, the script stops and prints what is required.

You can also run the release `install.sh` directly, which downloads the archive, verifies its `.sha256` checksum, and runs the same setup:

```bash
curl -fsSL https://github.com/karanshukla/vinoSearch/releases/latest/download/install.sh | bash -s -- --index ~/Documents
```

## Upgrading

Click **Check for updates** on the settings window's About page, or run `vinosearch-upgrade` (pass a tag such as `vinosearch-upgrade v6` to install a specific release). It skips steps that are already up to date and restarts the background service.

## From source

Building from source requires Rust and a C compiler. Running `scripts/install.sh` from a git checkout compiles and installs binaries with `cargo` into `~/.cargo/bin`; run `git pull` followed by `scripts/install.sh` to upgrade:

```bash
git clone https://github.com/karanshukla/vinoSearch && cd vinoSearch
scripts/install.sh --index ~/Documents
```

Or run the steps manually:

```bash
cargo install --path .                     # install vinosearch into ~/.cargo/bin
cargo install --path crates/settings       # install vinosearch-settings GUI (optional)
cargo install ovfetch                      # requires 0.6.0 or newer
scripts/install-runtime.sh                 # install OpenVINO via ovfetch
vinosearch fetch-model                     # download 340 MB base models (verified against pinned SHA-256)
vinosearch estimate ~/Development          # estimate indexing time and largest subfolders
vinosearch index ~/Documents ~/Development # index each root (unchanged files are skipped on re-runs)
vinosearch krunner --install               # install login service, app menu entry, and active desktop search integration
vinosearch integrate gnome                 # or kde: add another desktop's search integration (--off removes it)
```

Run `vinosearch doctor` to verify the NPU, driver, and runtime, and to compile and run a probe model.

## Other distros

Prebuilt binaries require `x86_64` with glibc 2.39 or newer: Fedora 40+, Ubuntu 24.04+, Debian 13, Arch, openSUSE Tumbleweed, Fedora Atomic (Silverblue, Kinoite, Bazzite), and openSUSE Aeon. On older distributions (Debian 12, Ubuntu 22.04, openSUSE Leap 15), `install.sh` detects the glibc version and directs you to build from source. On NixOS, enable `programs.nix-ld` so the dynamic linker is available.

Building from source requires Rust 1.95 or newer; because Debian and Ubuntu repositories ship older versions, install Rust via [rustup](https://rustup.rs). Install a C compiler and OpenSSL development headers (SQLCipher links against the system `libcrypto`):

| Distro | Command |
|---|---|
| Fedora | `sudo dnf install gcc openssl-devel` |
| Debian, Ubuntu | `sudo apt install build-essential libssl-dev` |
| Arch | `sudo pacman -S --needed base-devel openssl` |
| openSUSE | `sudo zypper install gcc libopenssl-devel` |

## Fedora Atomic and openSUSE Aeon

Everything installs inside your home directory: binaries in `~/.local/bin`, runtime/models/index under `~/.local`, and the autostart entry in `~/.config/autostart`. Because nothing modifies `/usr`, the prebuilt release requires no package layering or reboot. The only required system package is the Intel NPU driver; layer it with `rpm-ostree install` (Fedora Atomic) or `transactional-update pkg install` (Aeon), or use an image that already includes it.

If you prefer building from source, run `install.sh` inside a toolbox or distrobox matching your host distribution. Because the home directory is shared, the host runs the compiled binaries, uses the same index, and starts the service from the same autostart entry. Both `/var/home` and a `/home` symlink work properly because indexed paths are canonicalized.

## Flatpak

The `.flatpak` bundle packages the background service, CLI, and settings window together with OpenVINO and Intel's NPU and GPU userspace drivers, so the host only needs the kernel NPU driver (`intel_vpu`) and its firmware. Each release includes `vinosearch-x86_64.flatpak` and its checksum:

```bash
curl -fLO https://github.com/karanshukla/vinoSearch/releases/latest/download/vinosearch-x86_64.flatpak
curl -fLO https://github.com/karanshukla/vinoSearch/releases/latest/download/vinosearch-x86_64.flatpak.sha256
sha256sum -c vinosearch-x86_64.flatpak.sha256
flatpak install --user vinosearch-x86_64.flatpak
```

If `org.freedesktop.Platform` 26.08 is not installed yet, Flatpak fetches it from Flathub. Launch **vinoSearch** once from your application menu to start the service, register login autostart through the Background portal, enable search for the current desktop, and download the model from the Model page. GNOME Shell search results appear starting on the next login.

### Updating and building the Flatpak

- **In-app updates:** Click **Update** on the settings window's About page. It downloads the latest bundle and `.sha256` file, verifies the checksum, asks the host's `flatpak` to install it into the same scope, and restarts the service if it was running. Reopen the settings window to use the updated UI. System-wide installs (`flatpak install --system`) prompt for administrator authentication as usual.
- **Manual updates:** Download and verify the new bundle as shown above, then run `flatpak install --user vinosearch-x86_64.flatpak`. Because an already-running service continues using the previous version until stopped, restart it with:
  ```bash
  busctl --user call dev.vinosearch.VinoSearch.Runner /control dev.vinosearch.Control1 Quit
  ```
  The next search activates the updated service automatically.
- **Switching from a checkout build:** Self-updates only apply to release bundles (branch `stable`); local builds (branch `master`) link to the GitHub releases page instead. To switch to a release bundle, run `flatpak uninstall dev.vinosearch.VinoSearch//master` first (your index, models, and config under `~/.var/app` are preserved), then install the release bundle.
- **Building from a checkout:**
  ```bash
  flatpak install --user flathub org.flatpak.Builder org.freedesktop.Sdk//26.08 \
    org.freedesktop.Sdk.Extension.rust-stable//26.08
  flatpak run --filesystem=$HOME org.flatpak.Builder --user --install --force-clean \
    --repo=repo build packaging/flatpak/dev.vinosearch.VinoSearch.yml
  flatpak build-bundle repo vinosearch.flatpak dev.vinosearch.VinoSearch
  ```

Invoke the CLI via `flatpak run dev.vinosearch.VinoSearch` (for example, `flatpak run dev.vinosearch.VinoSearch doctor` or `... index ~/Documents`).

### Flatpak permissions

| Permission | Purpose |
|---|---|
| `--device=all` | Access to the NPU (`/dev/accel`, which no narrower Flatpak permission exposes) and GPU |
| `--filesystem=home` | Read files to be indexed |
| `--filesystem=host-os:ro`, `--filesystem=/var/lib/flatpak/exports/share/applications:ro` | Inspect host `.desktop` entries to populate editor/player lists and open files at a matching line, page, or timestamp |
| `--share=network` | Download models and check for updates |
| `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc` | Display the settings and search windows |
| `--talk-name=org.freedesktop.Flatpak` | Open search results in host apps, toggle desktop search settings, install Flatpak updates, and seal the index key with the host's `systemd-creds` via `flatpak-spawn --host` |
| `--talk-name=org.freedesktop.secrets` | Store the index key in the desktop keyring on hosts without `systemd-creds` |
| `--talk-name=org.kde.StatusNotifierWatcher` | Show the tray icon while indexing runs |
| `--system-talk-name=net.hadess.PowerProfiles` | Pause indexing when the system power-saver profile is active |

### Differences from a native install

- **Isolated data directory and bus name:** Configuration, index, and models are stored under `~/.var/app/dev.vinosearch.VinoSearch` (`config/vinosearch/config.toml`, `data/vinosearch`), allowing side-by-side use with a native installation. Logs go to the systemd journal when launched by systemd, or `.local/state/vinosearch/service.log` inside the app directory otherwise (`doctor` prints the exact location). The D-Bus name is `dev.vinosearch.VinoSearch.Runner`.
- **Search providers and app menu:** The KRunner plugin, GNOME Shell provider, and D-Bus activation file are bundled, but Flatpak leaves exported search providers disabled by default. Use the Integration page switches (or `integrate kde|gnome`) to enable them in your desktop's search settings. The search window is available as the **Search Files** action on the app menu entry; toggling the App menu switch (or running `integrate menu`) adds a standalone launcher in `~/.local/share/applications` (removed by turning off the switch or running `krunner --uninstall`; `flatpak uninstall` leaves the file behind, though the menu hides it).
- **Autostart:** The service switch and `krunner --install` / `--uninstall` manage login startup through the Background portal along with the current desktop's search integration.
- **dmenu launchers:** On sway, Hyprland, and similar desktops, bind `flatpak run dev.vinosearch.VinoSearch menu` to a shortcut. The launcher (`fuzzel`, `wofi`, `rofi`, `tofi`, `bemenu`, or `--launcher`) runs on the host.
- **Fallback behavior:** Configured editors or players missing on the host fall back to the default application, matching native installs. With no `device` specified in `config.toml`, device selection defaults to NPU, then GPU, then CPU.
- **Encryption:** Works identically to native installs. The index key is sealed to the host TPM using the host's `systemd-creds` via `flatpak-spawn --host`, so `index.sqlite` and `index.key` are interchangeable between native and Flatpak installations (startup adds ~10 ms for the host round-trip). On hosts without `systemd-creds`, the desktop keyring is used instead. If the host cannot be reached (for example, if `org.freedesktop.Flatpak` is revoked via `flatpak override`), vinoSearch preserves the encrypted index and waits rather than falling back to the keyring.
- **HEIC and AVIF photos:** Not indexed inside the Flatpak because the runtime does not include `heif-convert`.

Run `flatpak uninstall dev.vinosearch.VinoSearch` to remove the Flatpak; add `--delete-data` to also delete its index, models, and configuration.

## Uninstalling

Run `vinosearch/scripts/install.sh --uninstall` (or `curl -fsSL https://github.com/karanshukla/vinoSearch/releases/latest/download/install.sh | bash -s -- --uninstall`) to remove the background service, KRunner and GNOME Shell integration files, binaries, downloaded models (`~/.local/share/vinosearch/models`), and compiled model cache (`~/.cache/vinosearch/ov-cache`). Your index, `config.toml`, and OpenVINO runtime are kept by default; pass `--purge` to delete the index and runtime as well (`config.toml` is still preserved).

To remove only the downloaded models, click **Delete…** on the settings window's Model page or run `vinosearch delete-model`. You will need to run `vinosearch fetch-model` (or click **Download**) before searching or indexing again. If the service is stopped, the Model page deletes the files directly.
