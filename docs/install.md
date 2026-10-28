---
title: Installing
nav_order: 2
---

# Installing

## Requirements

- Linux with an Intel NPU (Core Ultra or newer) and the [NPU driver](https://github.com/intel/linux-npu-driver). No NPU? `--device CPU` works, just slower.
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
vinosearch krunner --install               # the systemd user service, the app menu entry and the running desktop's search
vinosearch integrate gnome                 # or kde: add the other desktop's search; --off removes one
```

`vinosearch doctor` checks the NPU, driver and runtime, then compiles and runs a probe model.

## Other distros

The release download runs on x86_64 with glibc 2.39 or newer: Fedora 40+, Ubuntu 24.04+, Debian 13,
Arch, openSUSE Tumbleweed, and the image-based Fedora Atomic (Silverblue, Kinoite, Bazzite) and
openSUSE Aeon. On anything older (Debian 12, Ubuntu 22.04, openSUSE Leap 15) `install.sh` says so;
build from source there.

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
`~/.local`, the service in `~/.config/systemd/user`. Nothing touches `/usr`, so the release download
needs no layering or reboot. The NPU driver is the one system package; layer it (`rpm-ostree install`
on Fedora Atomic, `transactional-update pkg install` on Aeon) or use an image that ships it.

To build from source instead, run `install.sh` inside a toolbox or distrobox whose release matches
the host: the home directory is shared, so the host runs the same binaries and index. If systemd
can't be reached from the container, `install.sh` prints the one command to run on the host.
`/var/home` and a `/home` symlink are both fine; indexed paths are resolved to the real one.

## Uninstalling

`vinosearch/scripts/install.sh --uninstall` removes the service, the KRunner and GNOME files and the
binaries. Add `--purge` to also delete the index, model, compiled cache and runtime.
