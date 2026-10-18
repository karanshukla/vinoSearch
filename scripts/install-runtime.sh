#!/usr/bin/env bash
# Install the OpenVINO runtime vinoSearch loads, through ovfetch, into a
# per-user prefix. No root: the daemon runs as the user.
#
#   scripts/install-runtime.sh
#
# vinoSearch calls OpenVINO directly, so it takes Intel's openvino wheel alone
# (`ovfetch install --openvino-only`): the newest OpenVINO the NPU driver
# supports, hash-checked, in seconds, with no ONNX Runtime and no compile.
# ovfetch skips the work when the prefix already holds the resolved build.
set -euo pipefail

PREFIX="${VINOSEARCH_OPENVINO_DIR:-$HOME/.local/lib/vinosearch/openvino}"
OVFETCH_MIN="0.5.0"

for arg in "$@"; do
    case "$arg" in
        -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

find_ovfetch() {
    local bin have
    for bin in "$(command -v ovfetch 2>/dev/null || true)" "$HOME/.cargo/bin/ovfetch"; do
        [ -n "$bin" ] && [ -x "$bin" ] || continue
        have="$("$bin" --version 2>/dev/null | awk '{print $2}')"
        if [ -n "$have" ] && [ "$(printf '%s\n%s\n' "$OVFETCH_MIN" "$have" | sort -V | head -1)" = "$OVFETCH_MIN" ]; then
            printf '%s\n' "$bin"
            return 0
        fi
        echo "ignoring $bin (${have:-unknown version}); needs $OVFETCH_MIN or newer" >&2
    done
    return 1
}

if ! OVFETCH="$(find_ovfetch)"; then
    echo "ovfetch $OVFETCH_MIN or newer not found: cargo install ovfetch" >&2
    exit 1
fi

"$OVFETCH" resolve --openvino-only
mkdir -p "$(dirname "$PREFIX")"
"$OVFETCH" install --openvino-only --prefix "$PREFIX"
"$OVFETCH" verify --prefix "$PREFIX"
echo "installed into $PREFIX; check it with: vinosearch doctor"
