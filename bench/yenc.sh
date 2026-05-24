#!/usr/bin/env bash
# bench/yenc.sh — yEnc throughput: pesto SIMD paths vs node-yencode
#
# Usage: ./bench/yenc.sh [SIZE_MB ...]
#   Default sizes: 100 MB
#   Example:       ./bench/yenc.sh 10 100 500
#
# pesto throughput is taken from the yenc-bench binary's own internal timer
# (warmup + N iterations via Rust Instant), so disk I/O does not skew results.
# node-yencode is measured via wall time of the node process.
#
# Requirements:
#   - cargo build --release --example yenc-bench
#   - node (for node-yencode comparison; skipped if absent)
#
# Results are written to bench/results/yenc-<hostname>-<date>.csv

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=bench/lib.sh
source "$REPO_ROOT/bench/lib.sh"

BENCH_BIN="$REPO_ROOT/target/release/examples/yenc-bench"
NODE_BENCH="$REPO_ROOT/bench/yencode.js"
RESULTS_DIR="$REPO_ROOT/bench/results"
LINE_LEN=128

if [[ $# -gt 0 ]]; then
    SIZE_LIST=("$@")
else
    SIZE_LIST=(100)
fi

# ── prerequisites ─────────────────────────────────────────────────────────────

[[ -x "$BENCH_BIN" ]] || {
    echo "yenc-bench binary not found. Building…"
    cargo build --release --example yenc-bench --manifest-path "$REPO_ROOT/Cargo.toml"
}

HAS_NODE=false
if command -v node >/dev/null 2>&1 && [[ -f "$NODE_BENCH" ]]; then
    HAS_NODE=true
fi

mkdir -p "$RESULTS_DIR"
HOSTNAME_SLUG="$(cat /etc/hostname 2>/dev/null || uname -n 2>/dev/null || echo "local")"
HOSTNAME_SLUG="${HOSTNAME_SLUG//./-}"
DATE_SLUG="$(date -u '+%Y%m%d')"
CSV="$RESULTS_DIR/yenc-${HOSTNAME_SLUG}-${DATE_SLUG}.csv"
csv_header "$CSV" "size_mb" "tool" "line_len" "throughput_mbps" "throughput_gbps"

# ── header ────────────────────────────────────────────────────────────────────

echo
bold "BENCHMARK: yEnc encoding throughput"; echo
print_system_info
echo
printf "  Line length : %s bytes\n" "$LINE_LEN"
printf "  Sizes       : %s MB\n" "${SIZE_LIST[*]}"
hr

# ── run ───────────────────────────────────────────────────────────────────────

declare -A R_PESTO_MBPS R_NODE_MBPS

run_pesto() {
    local size_mb=$1
    local src="$REPO_ROOT/bench_${size_mb}M.bin"
    ensure_bench_file "$src" "$(( size_mb * 1048576 ))"

    drop_caches
    # The binary prints MB/s computed via internal Rust Instant after warmup + N iters.
    local tp_mb
    tp_mb=$("$BENCH_BIN" "$src" "$LINE_LEN" 2>/dev/null)
    local tp_gb
    tp_gb=$(awk -v m="$tp_mb" 'BEGIN { printf "%.2f", m / 1024 }')

    R_PESTO_MBPS["$size_mb"]=$tp_mb
    csv_row "$CSV" "$size_mb" "pesto" "$LINE_LEN" "$tp_mb" "$tp_gb"
    printf "  %-16s  %s MB/s  (%s GB/s)\n" "pesto" "$tp_mb" "$tp_gb"
}

run_node() {
    local size_mb=$1
    local src="$REPO_ROOT/bench_${size_mb}M.bin"

    drop_caches
    # node bench_yencode.js uses the same structure as yenc-bench: warmup + N iters
    # with process.hrtime.bigint internal timer. Capture its stdout directly.
    local tp_mb
    tp_mb=$(node "$NODE_BENCH" "$src" "$LINE_LEN" 2>/dev/null)
    local tp_gb
    tp_gb=$(awk -v m="$tp_mb" 'BEGIN { printf "%.2f", m / 1024 }')

    R_NODE_MBPS["$size_mb"]=$tp_mb
    csv_row "$CSV" "$size_mb" "node-yencode" "$LINE_LEN" "$tp_mb" "$tp_gb"
    printf "  %-16s  %s MB/s  (%s GB/s)\n" "node-yencode" "$tp_mb" "$tp_gb"
}

for size_mb in "${SIZE_LIST[@]}"; do
    bold "── ${size_mb} MB ──"; echo
    run_pesto "$size_mb"
    $HAS_NODE && run_node "$size_mb" || printf "  %-16s  (not installed — skipped)\n" "node-yencode"
    echo
done

# ── summary table ─────────────────────────────────────────────────────────────

hr
bold "SUMMARY"; echo
hr

if $HAS_NODE; then
    md_header "Size" "pesto (MB/s)" "node-yencode (MB/s)" "speedup"
    for size_mb in "${SIZE_LIST[@]}"; do
        p="${R_PESTO_MBPS[$size_mb]:-0}"
        n="${R_NODE_MBPS[$size_mb]:-0}"
        sp=$(awk -v p="$p" -v n="$n" 'BEGIN {
            if (n <= 0 || p <= 0) { print "n/a"; exit }
            pct = (p / n - 1) * 100
            if (pct >= 0) printf "+%.1f%%", pct
            else          printf "%.1f%%", pct
        }')
        md_row "${size_mb} MB" "$p" "$n" "$sp"
    done
else
    md_header "Size" "pesto (MB/s)"
    for size_mb in "${SIZE_LIST[@]}"; do
        md_row "${size_mb} MB" "${R_PESTO_MBPS[$size_mb]:-0}"
    done
fi

echo
printf "Raw results: %s\n" "$CSV"
echo
