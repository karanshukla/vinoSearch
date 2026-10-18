#!/usr/bin/env bash
# Query latency through the installed KRunner runner, over D-Bus, the way
# KRunner calls it. Needs `vinosearch krunner --install` and an index.
#
#   bench/query_latency.sh [rounds]
#
# The first call starts the runner (D-Bus activation) and loads the model and
# vectors; the rest are warm. An Actions call, which does no work, is timed the
# same way and subtracted, so busctl's own start-up is not counted. The queries
# are aimed at the author's ~/Development.
set -euo pipefail

ROUNDS="${1:-5}"
DEST=dev.vinosearch.Runner
CALL=(busctl --user --timeout=120 call "$DEST" /runner org.kde.krunner1)
QUERIES=(
    "how is the NPU driver version checked"
    "rotary embedding conversion bug"
    "face authentication PAM module"
    "unload the model after idle timeout"
    "whisper speech to text on the NPU"
    "hash consensus across PyPI mirrors"
    "pipewire filter chain equalizer"
    "restaurant menu ordering"
    "android container window size"
    "sqlite schema migration"
)

ms() { echo $(( ($2 - $1) / 1000000 )); }
now() { date +%s%N; }

stats() {
    sort -n | awk '{a[NR]=$1; s+=$1} END {
        printf "n=%d  mean=%.1f  p50=%d  p95=%d  max=%d ms\n", NR, s/NR, a[int((NR+1)/2)], a[int(NR*0.95+0.5)>NR?NR:int(NR*0.95+0.5)], a[NR]}'
}

if busctl --user status "$DEST" >/dev/null 2>&1; then
    echo "runner already running; stop it for a cold number: busctl --user call $DEST /runner org.kde.krunner1 Teardown" >&2
fi

t0=$(now); "${CALL[@]}" Match s "${QUERIES[0]}" >/dev/null; t1=$(now)
echo "cold (activation + model + vectors + query): $(ms "$t0" "$t1") ms"

overhead=()
for _ in $(seq 10); do
    t0=$(now); "${CALL[@]}" Actions >/dev/null; t1=$(now)
    overhead+=("$(ms "$t0" "$t1")")
done
base=$(printf '%s\n' "${overhead[@]}" | sort -n | sed -n 5p)
echo "busctl overhead (median Actions): $base ms"

warm=()
for _ in $(seq "$ROUNDS"); do
    for q in "${QUERIES[@]}"; do
        t0=$(now); "${CALL[@]}" Match s "$q" >/dev/null; t1=$(now)
        warm+=("$(( $(ms "$t0" "$t1") - base ))")
    done
done
printf 'warm Match, overhead subtracted: '
printf '%s\n' "${warm[@]}" | stats

pid=$(busctl --user status "$DEST" 2>/dev/null | awk -F= '/^PID=/{print $2}')
[ -n "$pid" ] && grep -E 'VmRSS|VmHWM' "/proc/$pid/status"
