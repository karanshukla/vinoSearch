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
#
# When the NPU driver has no compiler (Fedora's 1.32.0 rpm), `ovfetch npu
# install` (0.6.0) fetches Intel's into ~/.local/lib/ovfetch-npu, which
# vinosearch preloads; see docs/how-it-works.md.
set -euo pipefail

PREFIX="${VINOSEARCH_OPENVINO_DIR:-$HOME/.local/lib/vinosearch/openvino}"
OVFETCH_MIN="0.6.0"

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

# An NPU whose driver is installed but cannot compile: every compile would
# fail with ZE_RESULT_ERROR_UNSUPPORTED_FEATURE. detect already counts a
# compiler in ~/.local/lib/ovfetch-npu, so this runs once.
detected="$("$OVFETCH" detect)"
if grep -q '"kind": "npu"' <<<"$detected" && ! grep -q '"npu_driver": null' <<<"$detected" \
   && grep -q '"npu_compiler": false' <<<"$detected"; then
    echo "the NPU driver has no compiler library; fetching Intel's with ovfetch npu install"
    "$OVFETCH" npu install
fi
echo "installed into $PREFIX; check it with: vinosearch doctor"
