#!/usr/bin/env bash
# Install vinoSearch for this user: the binaries, the OpenVINO runtime, the
# model, the first indexed folder, and the running desktop's search (KRunner
# on Plasma, GNOME Shell on GNOME, `vinosearch menu` elsewhere) with a user
# service. Safe to re-run: each step skips what is done.
# `vinosearch integrate kde|gnome` adds or removes a search place later.
#
# From a release download (bin/ next to scripts/) it copies the prebuilt
# binaries into ~/.local/bin; from a source checkout it builds them with cargo.
#
#   scripts/install.sh [--index DIR]... [--device NPU|GPU|CPU] [--no-settings]
#   scripts/install.sh --uninstall [--purge]
#
#   --index DIR    index DIR (repeatable). Without it you're asked for one;
#                  nothing is indexed unless you name it.
#   --device D     run the model on D instead of the NPU (written to
#                  config.toml if it doesn't set a device already)
#   --no-settings  skip the settings window (vinosearch-settings)
#   --uninstall    remove the service, KRunner / GNOME files and binaries
#   --purge        with --uninstall, also delete the index, model, compiled
#                  cache and runtime (config.toml is kept)
set -euo pipefail

cd "$(dirname "$0")/.."

OVFETCH_MIN="0.5.0"
# The release binaries are built on Ubuntu 24.04 (release.yml).
GLIBC_MIN="2.39"
# Building: the settings window's egui needs 1.95, vinosearch alone 1.89.
RUST_MIN="1.95"
RUST_MIN_NO_SETTINGS="1.89"
USER_PATH="$PATH"
CARGO_BIN="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
LOCAL_BIN="$HOME/.local/bin"
prebuilt=0
[ -x bin/vinosearch ] && prebuilt=1
BIN="$CARGO_BIN"
[ "$prebuilt" = 1 ] && BIN="$LOCAL_BIN"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}/vinosearch"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/vinosearch"
RUNTIME="${VINOSEARCH_OPENVINO_DIR:-$HOME/.local/lib/vinosearch/openvino}"
CONFIG="${VINOSEARCH_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/vinosearch/config.toml}"

roots=()
device=""
settings=1
uninstall=0
purge=0
while [ $# -gt 0 ]; do
    case "$1" in
        --index) [ $# -ge 2 ] || { echo "--index needs a directory" >&2; exit 2; }
                 roots+=("$2"); shift ;;
        --device) [ $# -ge 2 ] || { echo "--device needs NPU, GPU or CPU" >&2; exit 2; }
                  device="$2"; shift ;;
        --no-settings) settings=0 ;;
        --uninstall) uninstall=1 ;;
        --purge) purge=1 ;;
        -h|--help) sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

die() { echo "install.sh: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

if [ "$uninstall" = 1 ]; then
    for dir in "$CARGO_BIN" "$LOCAL_BIN"; do
        [ -x "$dir/vinosearch" ] || continue
        step "Removing the service and the KRunner / GNOME search files"
        "$dir/vinosearch" krunner --uninstall
        break
    done
    step "Removing the binaries"
    for b in vinosearch vinosearch-settings; do
        if [ -x "$CARGO_BIN/$b" ]; then cargo uninstall "$b"; fi
        if [ -x "$LOCAL_BIN/$b" ]; then rm -f -- "$LOCAL_BIN/$b" && echo "removed $LOCAL_BIN/$b"; fi
    done
    if [ "$purge" = 1 ]; then
        step "Deleting the index, model, compiled cache and runtime"
        for d in "$DATA" "$CACHE" "$RUNTIME"; do
            if [ -e "$d" ]; then rm -rf -- "$d" && echo "removed $d"; fi
        done
        echo "kept $CONFIG and ovfetch"
    else
        echo "kept the index, model and runtime; --uninstall --purge deletes them"
    fi
    exit 0
fi
[ "$purge" = 0 ] || die "--purge only goes with --uninstall"

# `a` >= `b`, as dotted versions.
version_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" = "$2" ]; }

# Image-based systems (Fedora Atomic, openSUSE Aeon) have a read-only /usr
# and no compiler on the host. Everything vinoSearch installs lives in
# $HOME, so the release download needs nothing from the system there.
image_based() {
    [ -e /run/ostree-booted ] || findmnt -no OPTIONS / 2>/dev/null | tr , '\n' | grep -qx ro
}

# The command that installs the C compiler, from /etc/os-release.
cc_hint() {
    local id=""
    [ -r /etc/os-release ] && id="$(. /etc/os-release && echo "${ID:-} ${ID_LIKE:-}")"
    if image_based; then
        echo "use the release download (no build needed), or run this inside a toolbox or distrobox"
        return
    fi
    case " $id " in
        *" fedora "*|*" rhel "*) echo "sudo dnf install gcc" ;;
        *" debian "*|*" ubuntu "*) echo "sudo apt install build-essential" ;;
        *" arch "*) echo "sudo pacman -S --needed base-devel" ;;
        *" suse "*|*" opensuse "*) echo "sudo zypper install gcc" ;;
        *) echo "install gcc" ;;
    esac
}

# Prerequisites, before anything is built or downloaded.
if [ "$prebuilt" = 1 ]; then
    arch="$(uname -m)"
    [ "$arch" = x86_64 ] ||
        die "the release binaries are x86_64 and this is $arch: build from a source checkout instead"
    glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')" || true
    [ -n "$glibc" ] ||
        die "the release binaries need glibc (not musl): build from a source checkout instead"
    version_ge "$glibc" "$GLIBC_MIN" ||
        die "the release binaries need glibc $GLIBC_MIN or newer and this has $glibc: build from a source checkout instead"
else
    command -v cargo >/dev/null || [ -x "$HOME/.cargo/bin/cargo" ] ||
        die "Rust is not installed: get it from https://rustup.rs, then run this again"
    command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
    need="$RUST_MIN"
    [ "$settings" = 1 ] || need="$RUST_MIN_NO_SETTINGS"
    have="$(rustc --version 2>/dev/null | awk '{print $2}')" || true
    if [ -n "$have" ] && ! version_ge "${have%%-*}" "$need"; then
        die "Rust $have is too old (needs $need; distro packages often lag): get a current one from https://rustup.rs, then run this again"
    fi
    command -v cc >/dev/null || die "no C compiler (cc): $(cc_hint), then run this again"
fi
for r in "${roots[@]}"; do
    [ -d "$r" ] || die "--index $r: not a directory"
done
want="$device"
if [ -z "$want" ] && [ -f "$CONFIG" ]; then
    want="$(sed -n 's/^[[:space:]]*device[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$CONFIG" | head -1)"
fi
if [ "$(printf %s "${want:-NPU}" | tr a-z A-Z)" = NPU ] && ! compgen -G '/dev/accel/accel*' >/dev/null; then
    die "no NPU (/dev/accel/accel*): install Intel's NPU driver, or run on another device with --device CPU"
fi

ovfetch_ok() {
    local bin have
    for bin in "$(command -v ovfetch 2>/dev/null || true)" "$BIN/ovfetch"; do
        [ -n "$bin" ] && [ -x "$bin" ] || continue
        have="$("$bin" --version 2>/dev/null | awk '{print $2}')"
        [ -n "$have" ] &&
            [ "$(printf '%s\n%s\n' "$OVFETCH_MIN" "$have" | sort -V | head -1)" = "$OVFETCH_MIN" ] &&
            return 0
    done
    return 1
}

if [ "$prebuilt" = 1 ]; then
    step "Installing the binaries into $BIN"
    mkdir -p "$BIN"
    bins=(vinosearch)
    [ "$settings" = 1 ] && bins+=(vinosearch-settings)
    ovfetch_ok || bins+=(ovfetch)
    for b in "${bins[@]}"; do
        cp --remove-destination "bin/$b" "$BIN/$b" && echo "installed $BIN/$b"
    done
else
    step "Building vinosearch"
    cargo install --locked --path .
    if [ "$settings" = 1 ]; then
        step "Building vinosearch-settings"
        cargo install --locked --path crates/settings
    fi
    if ! ovfetch_ok; then
        step "Installing ovfetch"
        cargo install --locked ovfetch
    fi
fi
export PATH="$BIN:$PATH"

step "OpenVINO runtime"
scripts/install-runtime.sh

if [ -n "$device" ]; then
    if [ -f "$CONFIG" ] && grep -q '^[[:space:]]*device[[:space:]]*=' "$CONFIG"; then
        echo "$CONFIG already sets a device; leaving it (the settings window changes it)"
    else
        mkdir -p "$(dirname "$CONFIG")"
        printf 'device = "%s"\n' "$device" >>"$CONFIG"
        echo "device = \"$device\" written to $CONFIG"
    fi
fi

step "Checking the device"
vinosearch doctor ||
    die "vinosearch doctor failed (above). Fix the NPU driver, or run on another device with --device CPU"

step "Model"
vinosearch fetch-model

if [ ${#roots[@]} -eq 0 ] && [ -t 0 ]; then
    printf '\nFolder to index, e.g. ~/Documents (blank to skip): '
    read -r answer
    answer="${answer/#\~/$HOME}"
    if [ -n "$answer" ]; then
        [ -d "$answer" ] || die "$answer: not a directory"
        roots+=("$answer")
    fi
fi
if [ ${#roots[@]} -gt 0 ]; then
    step "Indexing ${roots[*]}"
    vinosearch index "${roots[@]}"
else
    echo "nothing indexed; add folders later with vinosearch index DIR or the settings window"
fi

step "Desktop search"
was_running=0
systemctl --user is-active --quiet vinosearch.service 2>/dev/null && was_running=1
# The service, and the search place for the running desktop; it restarts
# KRunner itself when that is running.
vinosearch krunner --install
# A re-run that replaced the binary: run the new one.
if [ "$was_running" = 1 ]; then
    systemctl --user restart vinosearch.service && echo "restarted vinosearch.service"
fi
# Inside a toolbox or distrobox the files land in the shared home, but the
# host's systemd may be out of reach, so the service would only start on the
# first search instead of at login.
if ! systemctl --user is-enabled --quiet vinosearch.service 2>/dev/null; then
    echo "couldn't enable vinosearch.service from here (inside a toolbox or distrobox?)."
    echo "On the host, run: systemctl --user daemon-reload && systemctl --user enable --now vinosearch.service"
fi

printf '\nDone. Search with: vinosearch search "..." , KRunner, or GNOME Shell (GNOME: from the next login).\n'
echo 'On sway, Hyprland and other desktops: bind a key to vinosearch menu (needs fuzzel, wofi, rofi, tofi or bemenu).'
echo 'Another desktop'"'"'s search: vinosearch integrate kde|gnome'
case ":$USER_PATH:" in
    *":$BIN:"*) ;;
    *) echo "$BIN is not on your PATH; add it to run vinosearch by name." ;;
esac
