#!/usr/bin/env bash
# Prevent new oversized Rust files and growth of known source-size debt.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

readonly limit=800
readonly baseline_file="scripts/source-size-baseline.txt"

if [[ ! -f "$baseline_file" ]]; then
    echo "source-size baseline not found: $baseline_file" >&2
    exit 1
fi

declare -A baselines=()
while read -r max_lines path extra; do
    [[ -z "${max_lines:-}" || "$max_lines" == \#* ]] && continue
    if [[ -n "${extra:-}" || ! "$max_lines" =~ ^[0-9]+$ || -z "${path:-}" ]]; then
        echo "invalid source-size baseline entry: $max_lines ${path:-} ${extra:-}" >&2
        exit 1
    fi
    if [[ -n "${baselines[$path]+present}" ]]; then
        echo "duplicate source-size baseline entry: $path" >&2
        exit 1
    fi
    baselines["$path"]="$max_lines"
done < "$baseline_file"

status=0
declare -A oversized=()
while IFS= read -r -d '' path; do
    path="${path#./}"
    lines="$(wc -l < "$path")"
    if (( lines <= limit )); then
        continue
    fi

    oversized["$path"]="$lines"
    if [[ -z "${baselines[$path]+present}" ]]; then
        echo "$path has $lines lines (limit: $limit) and is not baselined" >&2
        status=1
    elif (( lines > baselines[$path] )); then
        echo "$path grew to $lines lines (baseline: ${baselines[$path]})" >&2
        status=1
    fi
done < <(find crates -type f -name '*.rs' -print0)

for path in "${!baselines[@]}"; do
    if [[ ! -f "$path" ]]; then
        echo "stale source-size baseline for missing file: $path" >&2
        status=1
    elif [[ -z "${oversized[$path]+present}" ]]; then
        echo "$path is now at or below $limit lines; remove its baseline entry" >&2
        status=1
    fi
done

if (( status != 0 )); then
    echo "Update module boundaries or lower the matching baseline after a split." >&2
    exit "$status"
fi

printf 'Source-size guard passed (%d existing files above %d lines).\n' \
    "${#oversized[@]}" "$limit"
