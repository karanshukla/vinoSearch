#!/usr/bin/env bash
# Write the eval testbed (#107) into OUT, which must not exist yet or be
# empty: notes, code, French and Hindi files, PDFs, Office files, RTF, a
# Word 97 .doc, CSVs and two drawn pictures, with the content
# evals/queries.toml expects. Deterministic, and needs only python3.
#
#   scripts/make-testbed.sh OUT
#   vinosearch --device CPU eval OUT
set -euo pipefail

if [ $# -ne 1 ] || [ "$1" = -h ] || [ "$1" = --help ]; then
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi
out="$1"
if [ -e "$out" ] && [ -n "$(ls -A "$out")" ]; then
    echo "$out is not empty" >&2
    exit 1
fi
mkdir -p "$out"
python3 "$(dirname "$0")/make-testbed.py" "$out"
echo "testbed written to $out"
