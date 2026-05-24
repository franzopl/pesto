#!/usr/bin/env bash
# bench/lib.sh — shared helpers for all pesto benchmark scripts
# Source with: source "$(dirname "$0")/lib.sh"

set -euo pipefail

# ── terminal helpers ─────────────────────────────────────────────────────────

bold()   { printf '\033[1m%s\033[0m' "$1"; }
green()  { printf '\033[32m%s\033[0m' "$1"; }
red()    { printf '\033[31m%s\033[0m' "$1"; }
yellow() { printf '\033[33m%s\033[0m' "$1"; }
cyan()   { printf '\033[36m%s\033[0m' "$1"; }
hr()     { printf '%0.s─' {1..72}; echo; }

# ── system info ──────────────────────────────────────────────────────────────

cpu_model() {
    if [[ -f /proc/cpuinfo ]]; then
        grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs
    elif command -v sysctl >/dev/null 2>&1; then
        sysctl -n machdep.cpu.brand_string 2>/dev/null || echo "unknown"
    else
        echo "unknown"
    fi
}

cpu_cores() {
    nproc 2>/dev/null || sysctl -n hw.logicalcpu 2>/dev/null || echo "?"
}

simd_flags() {
    local flags=""
    if [[ -f /proc/cpuinfo ]]; then
        local cpu_flags
        cpu_flags=$(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2)
        for flag in ssse3 avx2 avx512f gfni neon; do
            echo "$cpu_flags" | grep -qw "$flag" && flags+="$flag "
        done
    fi
    echo "${flags:-unknown}"
}

print_system_info() {
    printf "  CPU   : %s\n" "$(cpu_model)"
    printf "  Cores : %s logical\n" "$(cpu_cores)"
    printf "  SIMD  : %s\n" "$(simd_flags)"
    printf "  Date  : %s\n" "$(date -u '+%Y-%m-%d %H:%M UTC')"
}

# ── time & throughput ────────────────────────────────────────────────────────

# now_ms — current time in milliseconds
now_ms() { date +%s%3N; }

# throughput_mbps <bytes> <elapsed_ms>
throughput_mbps() {
    local bytes=$1 ms=$2
    awk -v b="$bytes" -v m="$ms" 'BEGIN {
        if (m <= 0) { print "0.0"; exit }
        printf "%.1f", (b / 1048576) / (m / 1000)
    }'
}

# throughput_gbps <bytes> <elapsed_ms>
throughput_gbps() {
    local bytes=$1 ms=$2
    awk -v b="$bytes" -v m="$ms" 'BEGIN {
        if (m <= 0) { print "0.00"; exit }
        printf "%.2f", (b / 1073741824) / (m / 1000)
    }'
}

# ms_to_str <ms> → "1m23s" or "4s"
ms_to_str() {
    local ms=$1 s
    s=$(( ms / 1000 ))
    if (( s >= 60 )); then
        printf '%dm%02ds' "$(( s / 60 ))" "$(( s % 60 ))"
    else
        printf '%ds' "$s"
    fi
}

# speedup_pct <subject_ms> <reference_ms> → "+12.3%" or "-5.1%"
# Positive = subject is faster than reference.
speedup_pct() {
    local subject=$1 ref=$2
    awk -v s="$subject" -v r="$ref" 'BEGIN {
        if (r <= 0 || s <= 0) { print "n/a"; exit }
        pct = (r / s - 1) * 100
        if (pct >= 0) printf "+%.1f%%", pct
        else          printf "%.1f%%", pct
    }'
}

# ── file helpers ─────────────────────────────────────────────────────────────

# ensure_bench_file <path> <size_in_bytes>
# Creates a file filled with pseudo-random data using OpenSSL (fast).
# Random data is required for accurate PAR2 benchmarks — sparse/zero files
# allow trivial GF(2^16) multiplication that skews results by 10–100×.
ensure_bench_file() {
    local path=$1 size=$2
    if [[ ! -f "$path" ]]; then
        local mb=$(( size / 1048576 ))
        printf "  Generating %s (%s MiB of random data)…\n" "$(basename "$path")" "$mb"
        # openssl rand is significantly faster than /dev/urandom for large sizes
        if command -v openssl >/dev/null 2>&1; then
            openssl rand "$size" > "$path"
        else
            dd if=/dev/urandom bs=1M count="$mb" of="$path" 2>/dev/null
        fi
        printf "  Done.\n"
    fi
}

# drop_caches — best-effort page-cache drop (Linux only, requires sudo)
drop_caches() {
    sync
    echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null 2>&1 || true
}

# file_size_bytes <path>
file_size_bytes() { stat -c%s "$1" 2>/dev/null || stat -f%z "$1"; }

# ── CSV output ───────────────────────────────────────────────────────────────

# csv_header <file> <columns…>
csv_header() {
    local file=$1; shift
    printf '%s\n' "$(IFS=,; echo "$*")" > "$file"
}

# csv_row <file> <values…>
csv_row() {
    local file=$1; shift
    printf '%s\n' "$(IFS=,; echo "$*")" >> "$file"
}

# ── markdown table ───────────────────────────────────────────────────────────

# md_header <col…>  — prints a markdown table header + separator row
md_header() {
    local sep=""
    printf '| '
    for col in "$@"; do printf '%s | ' "$col"; done
    echo
    printf '|'
    for col in "$@"; do
        sep="$(printf '%0.s-' $(seq 1 $(( ${#col} + 2 ))))"
        printf '%s|' "$sep"
    done
    echo
}

# md_row <val…>
md_row() {
    printf '| '
    for val in "$@"; do printf '%s | ' "$val"; done
    echo
}
