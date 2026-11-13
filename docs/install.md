---
title: Installing
nav_order: 2
---

# Installing

## Requirements

- Linux with an Intel NPU (Core Ultra or newer) and the [NPU driver](https://github.com/intel/linux-npu-driver). No NPU (AMD, older Intel)? `install.sh` runs it on the CPU, just slower; `--device CPU` does the same explicitly.
- KDE Plasma or GNOME on Wayland. Sway, Hyprland, COSMIC and other Wayland desktops work through `vinosearch menu` and a dmenu-style launcher (fuzzel, wofi, rofi, tofi or bemenu). The settings window is Wayland only.
- Rust, a C compiler and OpenSSL's headers, only to build from source.

## Release download

```bash
curl -L https://github.com/karanshukla/vinoSearch/releases/latest/download/vinosearch-x86_64-linux.tar.gz | tar xz
vinosearch/scripts/install.sh --index ~/Documents
```

That installs the prebuilt binaries into `~/.local/bin` (x86_64, glibc 2.39 or newer), fetches
OpenVINO and the 340 MB model, indexes the folders you name (`--index` is repeatable, or it asks),
and registers vinoSearch with the running desktop's search (KRunner on Plasma, GNOME Shell on GNOME)
as a service started at login. It skips whatever is already done, so re-running it from a newer
download upgrades in place. If something is missing (the NPU driver, say), it stops with one line
saying what.

## From source

Needs Rust and a C compiler. The same script builds the binaries with cargo into `~/.cargo/bin`
when run from a checkout, and `git pull` then a re-run upgrades:

```bash
git clone https://github.com/karanshukla/vinoSearch && cd vinoSearch
scripts/install.sh --index ~/Documents
```

Or step by step:

```bash
cargo install --path .                     # vinosearch into ~/.cargo/bin
cargo install --path crates/settings       # vinosearch-settings, the settings window (optional)
cargo install ovfetch                      # 0.6.0 or newer
scripts/install-runtime.sh                 # OpenVINO via ovfetch
vinosearch fetch-model                     # 340 MB, every file checked against a pinned SHA-256
vinosearch estimate ~/Development          # how long indexing would take, and which subfolders take longest
vinosearch index ~/Documents ~/Development # once per root; unchanged files are skipped on re-runs
vinosearch krunner --install               # the service (started now and at login), the app menu entry and the running desktop's search
vinosearch integrate gnome                 # or kde: add the other desktop's search; --off removes one
```

`vinosearch doctor` checks the NPU, driver and runtime, then compiles and runs a probe model.

## Other distros

The release download runs on x86_64 with glibc 2.39 or newer: Fedora 40+, Ubuntu 24.04+, Debian 13,
Arch, openSUSE Tumbleweed, and the image-based Fedora Atomic (Silverblue, Kinoite, Bazzite) and
openSUSE Aeon. On anything older (Debian 12, Ubuntu 22.04, openSUSE Leap 15) `install.sh` says so;
build from source there. The binaries also need the standard dynamic linker, so on NixOS enable
`programs.nix-ld`.

Building needs Rust 1.95 or newer, which Debian's and Ubuntu's packaged Rust isn't, so get it from
[rustup](https://rustup.rs). The C compiler and OpenSSL's headers (SQLCipher links the system's
libcrypto) are:

| Distro | Command |
|---|---|
| Fedora | `sudo dnf install gcc openssl-devel` |
| Debian, Ubuntu | `sudo apt install build-essential libssl-dev` |
| Arch | `sudo pacman -S --needed base-devel openssl` |
| openSUSE | `sudo zypper install gcc libopenssl-devel` |

## Fedora Atomic and openSUSE Aeon

Everything goes in your home: binaries in `~/.local/bin`, the runtime, model and index under
`~/.local`, the service's autostart entry in `~/.config/autostart`. Nothing touches `/usr`, so the release download
needs no layering or reboot. The NPU driver is the one system package; layer it (`rpm-ostree install`
on Fedora Atomic, `transactional-update pkg install` on Aeon) or use an image that ships it.

To build from source instead, run `install.sh` inside a toolbox or distrobox whose release matches
the host: the home directory is shared, so the host runs the same binaries and index, and the host's
session starts the service from the same autostart entry.
`/var/home` and a `/home` symlink are both fine; indexed paths are resolved to the real one.

## Flatpak

A `.flatpak` bundle holds the service, the CLI and the settings window, with OpenVINO and Intel's
NPU and GPU drivers inside it, so the host needs only the kernel's NPU driver (`intel_vpu`) and
its firmware:

```bash
flatpak install --user vinosearch.flatpak
```

Open vinoSearch from the app menu once: it starts the service, asks the desktop to start it at
login (the Background portal), turns on the running desktop's search, and its Model page downloads
the model. GNOME Shell's search finds it from the next login. To build the bundle from a checkout:

```bash
flatpak install --user flathub org.flatpak.Builder org.freedesktop.Sdk//26.08 \
  org.freedesktop.Sdk.Extension.rust-stable//26.08
flatpak run --filesystem=$HOME org.flatpak.Builder --user --install --force-clean \
  --repo=repo build packaging/flatpak/dev.vinosearch.VinoSearch.yml
flatpak build-bundle repo vinosearch.flatpak dev.vinosearch.VinoSearch
```

The CLI is `flatpak run dev.vinosearch.VinoSearch` (for example `... doctor` or `... index ~/Documents`).

What it is allowed, and why:

| Permission | For |
|---|---|
| `--device=all` | the NPU (`/dev/accel`), which nothing narrower exposes, and the GPU |
| `--filesystem=home` | the files it indexes |
| `--filesystem=host-os:ro`, `--filesystem=/var/lib/flatpak/exports/share/applications:ro` | the host's app entries: which app opens a file at its line, page or second, and the editor and player lists |
| `--share=network` | downloading the models |
| `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc` | the settings and search windows |
| `--talk-name=org.freedesktop.Flatpak` | opening results in the host's apps, and turning search places on and off in the host's settings (`flatpak-spawn --host`) |
| `--system-talk-name=net.hadess.PowerProfiles` | holding indexing while power saver is on |

What differs from the native install:

- Its config, index, models and log are its own, under `~/.var/app/dev.vinosearch.VinoSearch`
  (`config/vinosearch/config.toml`, `data/vinosearch`, `.local/state/vinosearch/service.log`), so it
  can sit beside a native install; `doctor` prints the log's path. Its bus name is
  `dev.vinosearch.VinoSearch.Runner`.
- The KRunner plugin, GNOME Shell provider and D-Bus activation file come with it. Flatpak leaves
  exported search providers off, so the Integration page's switches (and `integrate kde|gnome`)
  turn them on and off in the desktop's own setting, the one its search settings page shows. The
  search window is the **Search Files** action of the app menu entry rather than an entry of its
  own.
- The service switch, and `krunner --install` / `--uninstall`, turn starting at login on and off
  through the Background portal, with the running desktop's search place.
- With no `device` in `config.toml` it uses the NPU, else the GPU, else the CPU (natively too).
- Encrypting the index needs `systemd-creds`, which isn't in the sandbox, so it isn't available.
- HEIC and AVIF photos aren't indexed: the runtime has no `heif-convert`.

`flatpak uninstall dev.vinosearch.VinoSearch` removes it; add `--delete-data` to delete its index,
models and config too.

## Uninstalling

`vinosearch/scripts/install.sh --uninstall` removes the service, the KRunner and GNOME files and the
binaries, and deletes the models (`~/.local/share/vinosearch/models`) and their compiled cache
(`~/.cache/vinosearch/ov-cache`). The index, `config.toml` and the OpenVINO runtime are kept. Add
`--purge` to also delete the index and the runtime; `config.toml` is still kept.

To delete only the models, use **Delete…** on the settings window's Model page, or run
`vinosearch delete-model`. Search and indexing need `vinosearch fetch-model` (or Download) again
afterwards. With the service not running, the Model page deletes the files itself.
