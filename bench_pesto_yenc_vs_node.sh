#!/usr/bin/env bash
# yEnc benchmark: pesto vs node-yencode
#
# Usage: ./bench_pesto_yenc_vs_node.sh [SIZE_MB ...]
#   Default sizes: 100 (MB)
#   Example:       ./bench_pesto_yenc_vs_node.sh 10 50 100

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PESTO_BENCH="$SCRIPT_DIR/target/release/examples/yenc-bench"
NODE_BENCH="$SCRIPT_DIR/bench_yencode.js"
OUTDIR="$SCRIPT_DIR/bench_yenc_out"
LINE_LEN=128

# File sizes in MB, overridable via CLI args.
if [[ $# -gt 0 ]]; then
    SIZE_LIST=("$@")
else
    SIZE_LIST=(100)
fi

# ── helpers ──────────────────────────────────────────────────────────────────

bold()  { printf '\033[1m%s\033[0m' "$1"; }
green() { printf '\033[32m%s\033[0m' "$1"; }
red()   { printf '\033[31m%s\033[0m' "$1"; }
hr()    { printf '%0.s─' {1..72}; echo; }

speedup_pct() {
    local p=$1 a=$2
    awk -v p="$p" -v a="$a" 'BEGIN {
        if (a <= 0 || p <= 0) { print "n/a"; exit }
        pct = (p / a - 1) * 100
        if (pct >= 0) printf "+%.1f%%", pct
        else          printf "%.1f%%", pct
    }'
}

# ── prerequisites ─────────────────────────────────────────────────────────────

fail() { red "ERROR: $*"; echo; exit 1; }

[[ -x "$PESTO_BENCH" ]] || fail "pesto benchmark not found at $PESTO_BENCH — run: cargo build --release --example yenc-bench"

# Check for yencode
if ! node -e "require('yencode')" >/dev/null 2>&1; then
    echo "yencode not found. Installing locally..."
    npm install yencode --no-save --silent
fi

mkdir -p "$OUTDIR"

# ── header ────────────────────────────────────────────────────────────────────

echo
bold "BENCHMARK: pesto vs node-yencode — yEnc encoding"; echo
echo "  Line Length: ${LINE_LEN}"
echo "  Sizes      : ${SIZE_LIST[*]} MB"
printf "  CPU        : %s\n" "$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | xargs || echo unknown)"
printf "  Cores      : %s logical\n" "$(nproc 2>/dev/null || echo '?')"
hr

declare -A RES_TP RES_OK

# ── run one benchmark ─────────────────────────────────────────────────────────

run_bench() {
    local tool=$1   # "pesto" or "yencode"
    local src=$2    # absolute path to source file
    local key="${tool}:${src}"

    local tp="0.0"
    local exit_code=0

    if [[ "$tool" == "pesto" ]]; then
        tp=$("$PESTO_BENCH" "$src" "$LINE_LEN" "auto" 2>/dev/null) || exit_code=$?
    else
        tp=$(node "$NODE_BENCH" "$src" "$LINE_LEN" 2>/dev/null) || exit_code=$?
    fi

    RES_TP["$key"]="$tp"
    RES_OK["$key"]="$exit_code"
}

# ── main loop ─────────────────────────────────────────────────────────────────

CSV="$OUTDIR/results.csv"
printf "file_mb,tool,throughput_mbps,ok\n" > "$CSV"

for size_mb in "${SIZE_LIST[@]}"; do
    file="bench_yenc_${size_mb}M.bin"
    src="$OUTDIR/$file"

    if [[ ! -f "$src" ]]; then
        printf "Creating %s…\n" "$file"
        # Create non-sparse file with some variety to avoid extreme compression/sparse optimizations
        dd if=/dev/urandom of="$src" bs=1M count="$size_mb" status=none
    fi

    bold "── ${size_mb} MB ──"; echo

    for tool in pesto yencode; do
        printf "  %-12s  running… " "$tool"
        run_bench "$tool" "$src"

        key="${tool}:${src}"
        tp="${RES_TP[$key]}"
        ok="${RES_OK[$key]}"

        if [[ "$ok" -ne 0 ]]; then
            printf '\r  %-12s  ' "$tool"; red "FAILED (exit $ok)"; echo
        else
            printf '\r  %-12s  %s MB/s\n' "$tool" "$tp"
        fi

        printf "%s,%s,%s,%s\n" "$size_mb" "$tool" "$tp" "$ok" >> "$CSV"
    done

    # Speedup line
    p_key="pesto:${src}"
    n_key="yencode:${src}"
    if [[ "${RES_OK[$p_key]}" -eq 0 && "${RES_OK[$n_key]}" -eq 0 ]]; then
        p_tp="${RES_TP[$p_key]}"
        n_tp="${RES_TP[$n_key]}"
        sp=$(speedup_pct "$p_tp" "$n_tp")
        if [[ "$sp" == +* ]]; then
            printf "  → pesto faster by "; green "$sp"; echo " vs yencode"
        else
            printf "  → pesto slower by "; red "$sp"; echo " vs yencode"
        fi
    fi
    echo
done

# ── summary table ─────────────────────────────────────────────────────────────

hr
bold "SUMMARY  (throughput MB/s)"; echo
hr
printf "%-12s  %-14s  %-14s  %-16s\n" "Size" "pesto" "yencode" "pesto vs yencode"
hr

for size_mb in "${SIZE_LIST[@]}"; do
    src="$OUTDIR/bench_yenc_${size_mb}M.bin"
    p_key="pesto:${src}"
    n_key="yencode:${src}"

    p_ok="${RES_OK[$p_key]:-1}"
    n_ok="${RES_OK[$n_key]:-1}"

    p_tp=$( [[ "$p_ok" -eq 0 ]] && echo "${RES_TP[$p_key]}" || echo "FAIL" )
    n_tp=$( [[ "$n_ok" -eq 0 ]] && echo "${RES_TP[$n_key]}" || echo "FAIL" )

    if [[ "$p_ok" -eq 0 && "$n_ok" -eq 0 ]]; then
        sp=$(speedup_pct "$p_tp" "$n_tp")
    else
        sp="n/a"
    fi

    printf "%-12s  %-14s  %-14s  %-16s\n" "${size_mb}M" "$p_tp" "$n_tp" "$sp"
done

hr
printf "Results saved to: %s\n" "$CSV"
echo
