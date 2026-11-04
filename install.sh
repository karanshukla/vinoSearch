#!/usr/bin/env bash
# Install or upgrade vinoSearch from a GitHub release, for this user:
#
#   curl -fsSL https://github.com/karanshukla/vinoSearch/releases/latest/download/install.sh | bash
#
# Options go after `bash -s --`, e.g. `| bash -s -- --index ~/Documents`:
#
#   vN          that release instead of the newest
#   anything else is passed to the release's scripts/install.sh:
#               --index DIR, --device NPU|GPU|CPU, --no-settings, --uninstall
#
# It downloads the release's tarball and its .sha256, refuses the tarball
# unless the checksum matches, unpacks it into a private temporary directory
# and runs the release's own scripts/install.sh, which installs the binaries
# into ~/.local/bin, fetches OpenVINO and the model, and restarts the service
# if it was running. That also leaves this script as vinosearch-upgrade, which
# the settings window's Update button runs (with VINO_UPDATE_TAG set).
#
# From a checkout, scripts/install.sh builds from source instead.

# Everything is in main, called on the last line: piped into bash, a download
# cut short must not run half a script.
set -euo pipefail

REPO="karanshukla/vinoSearch"
ASSET="vinosearch-x86_64-linux.tar.gz"

main() {
    local tag="${VINO_UPDATE_TAG:-}" arg
    local args=()
    for arg in "$@"; do
        case "$arg" in
            v[0-9]*)
                if ! [[ "$arg" =~ ^v[0-9]+(\.[0-9]+)*$ ]]; then
                    echo "✗ Not a release tag: '$arg'" >&2
                    exit 2
                fi
                tag="$arg" ;;
            *) args+=("$arg") ;;
        esac
    done
    if [ -n "$tag" ] && ! [[ "$tag" =~ ^v[0-9]+(\.[0-9]+)*$ ]]; then
        echo "✗ Not a release tag: '$tag'" >&2
        exit 2
    fi

    if [ "$(id -u)" -eq 0 ]; then
        echo "✗ Run this as yourself, not root: vinoSearch installs into your home" >&2
        exit 1
    fi
    if [ "$(uname -m)" != x86_64 ]; then
        echo "✗ The release binaries are x86_64 only; build from source on $(uname -m)" >&2
        exit 1
    fi
    local tool
    for tool in curl tar sha256sum; do
        command -v "$tool" >/dev/null || { echo "✗ Missing $tool: install it, then run this again" >&2; exit 1; }
    done

    # Where the assets are: a mirror, or CI's file:// fake release.
    local base
    if [ -n "${VINOSEARCH_RELEASE_BASE:-}" ]; then
        base="$VINOSEARCH_RELEASE_BASE"
    elif [ -n "$tag" ]; then
        base="https://github.com/$REPO/releases/download/$tag"
    else
        base="https://github.com/$REPO/releases/latest/download"
    fi

    local work
    work="$(mktemp -d)"
    # shellcheck disable=SC2064 # expand now: $work is local
    trap "rm -rf '$work'" EXIT

    echo "== Downloading ${tag:-the latest release} from $base"
    local name
    for name in "$ASSET" "$ASSET.sha256"; do
        if ! curl -fsSL --retry 5 --retry-delay 2 --retry-all-errors --connect-timeout 20 \
                -o "$work/$name" "$base/$name"; then
            echo "✗ Download failed: $base/$name" >&2
            exit 1
        fi
    done
    # The .sha256 must name exactly this file: sha256sum -c alone would pass
    # whatever file it happened to list.
    if ! (cd "$work" \
            && grep -E "^[0-9a-f]{64} [ *]$ASSET\$" "$ASSET.sha256" > want \
            && [ "$(wc -l < want)" -eq 1 ] \
            && sha256sum -c --strict --quiet want); then
        echo "✗ Checksum verification failed for $ASSET. Nothing was installed." >&2
        exit 1
    fi
    echo "checksum verified"

    tar -xzf "$work/$ASSET" -C "$work" --no-same-owner
    [ -x "$work/vinosearch/scripts/install.sh" ] || { echo "✗ $ASSET has no scripts/install.sh" >&2; exit 1; }

    # At a terminal, the release's installer may ask which folder to index.
    # Piped into bash, stdin is this script, so it reads the terminal instead;
    # with none (the settings window's Update button) it asks nothing.
    if [ -t 1 ] && (: </dev/tty) 2>/dev/null; then
        "$work/vinosearch/scripts/install.sh" "${args[@]}" </dev/tty
    else
        "$work/vinosearch/scripts/install.sh" "${args[@]}" </dev/null
    fi
}

main "$@"
