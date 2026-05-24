#!/usr/bin/env bash
# bench/posting.sh — end-to-end article pipeline throughput (no real server needed)
#
# Uses pesto's --dry-run mode to measure the full hot path:
#   file read → yEnc encode → article assembly → (simulated) send
#
# Usage: ./bench/posting.sh [SIZE_MB ...]
#   Default sizes: 100 500 (MB)
#   Example:       ./bench/posting.sh 100 500 1000
#
# Results are written to bench/results/posting-<hostname>-<date>.csv

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=bench/lib.sh
source "$REPO_ROOT/bench/lib.sh"

PESTO="$REPO_ROOT/target/release/pesto"
RESULTS_DIR="$REPO_ROOT/bench/results"

if [[ $# -gt 0 ]]; then
    SIZE_LIST=("$@")
else
    SIZE_LIST=(100 500)
fi

# ── prerequisites ─────────────────────────────────────────────────────────────

[[ -x "$PESTO" ]] || {
    echo "pesto binary not found. Building…"
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
}

# Confirm --dry-run is supported
"$PESTO" --dry-run --help >/dev/null 2>&1 || {
    red "ERROR: pesto does not support --dry-run (required for this benchmark)"; echo
    exit 1
}

mkdir -p "$RESULTS_DIR"
HOSTNAME_SLUG="$(cat /etc/hostname 2>/dev/null || uname -n 2>/dev/null || echo "local")"
HOSTNAME_SLUG="${HOSTNAME_SLUG//./-}"
DATE_SLUG="$(date -u '+%Y%m%d')"
CSV="$RESULTS_DIR/posting-${HOSTNAME_SLUG}-${DATE_SLUG}.csv"
csv_header "$CSV" "size_mb" "connections" "elapsed_ms" "throughput_mbps"

# ── header ────────────────────────────────────────────────────────────────────

echo
bold "BENCHMARK: pesto end-to-end article pipeline (dry-run)"; echo
print_system_info
echo
printf "  Article size : 768 KB (default)\n"
printf "  Sizes        : %s MB\n" "${SIZE_LIST[*]}"
hr

# ── run ───────────────────────────────────────────────────────────────────────

declare -A R_MS R_BYTES

for size_mb in "${SIZE_LIST[@]}"; do
    src="$REPO_ROOT/bench_${size_mb}M.bin"
    ensure_bench_file "$src" "$(( size_mb * 1048576 ))"
    bytes=$(file_size_bytes "$src")

    bold "── ${size_mb} MB ──"; echo

    # Run at a few connection counts to show scaling
    for conns in 1 4 8; do
        drop_caches
        t0=$(now_ms)
        "$PESTO" --dry-run --connections "$conns" "$src" >/dev/null 2>&1
        t1=$(now_ms)
        elapsed_ms=$(( t1 - t0 ))

        tp=$(throughput_mbps "$bytes" "$elapsed_ms")
        R_MS["${size_mb}:${conns}"]=$elapsed_ms
        R_BYTES["${size_mb}:${conns}"]=$bytes

        csv_row "$CSV" "$size_mb" "$conns" "$elapsed_ms" "$tp"
        printf "  connections=%-3s  %s  %s MB/s\n" "$conns" "$(ms_to_str "$elapsed_ms")" "$tp"
    done
    echo
done

# ── summary table ─────────────────────────────────────────────────────────────

hr
bold "SUMMARY  (throughput MB/s, dry-run)"; echo
hr
md_header "Size" "1 conn (MB/s)" "4 conns (MB/s)" "8 conns (MB/s)"
for size_mb in "${SIZE_LIST[@]}"; do
    r1=$(throughput_mbps "${R_BYTES["${size_mb}:1"]:-0}"  "${R_MS["${size_mb}:1"]:-1}")
    r4=$(throughput_mbps "${R_BYTES["${size_mb}:4"]:-0}"  "${R_MS["${size_mb}:4"]:-1}")
    r8=$(throughput_mbps "${R_BYTES["${size_mb}:8"]:-0}"  "${R_MS["${size_mb}:8"]:-1}")
    md_row "${size_mb} MB" "$r1" "$r4" "$r8"
done

echo
printf "Raw results: %s\n" "$CSV"
echo
