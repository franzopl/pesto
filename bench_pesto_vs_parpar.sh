#!/usr/bin/env bash
# PAR2 benchmark: pesto vs parpar
#
# Usage: ./bench_pesto_vs_parpar.sh [SIZE_GB ...]
#   Default sizes: 1 5 10 (GB)
#   Example:       ./bench_pesto_vs_parpar.sh 1 2
#
# Both tools are run with 10% recovery, slice target matching pesto's default.
# Source files are created as sparse files (instant, no real I/O on creation).
# PAR2 output goes to OUTDIR/pesto/ and OUTDIR/parpar/ respectively.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PESTO="$SCRIPT_DIR/target/release/pesto"
OUTDIR="$SCRIPT_DIR/bench_out"
RECOVERY_PCT=10
# Parpar slice target: match pesto's TARGET_PAR2_SLICES (poster.rs:38 = 1000,
# clamped up to 2000 depending on file size).
PARPAR_SLICES=1000

# File sizes in GB, overridable via CLI args.
if [[ $# -gt 0 ]]; then
    SIZE_LIST=("$@")
else
    SIZE_LIST=(5)
fi

# ── helpers ──────────────────────────────────────────────────────────────────

bold()  { printf '\033[1m%s\033[0m' "$1"; }
green() { printf '\033[32m%s\033[0m' "$1"; }
red()   { printf '\033[31m%s\033[0m' "$1"; }
hr()    { printf '%0.s─' {1..72}; echo; }

secs_to_str() {
    local t=$1
    if (( t >= 60 )); then
        printf '%dm%02ds' "$(( t / 60 ))" "$(( t % 60 ))"
    else
        printf '%ds' "$t"
    fi
}

# throughput_mbps <bytes> <milliseconds>
throughput_mbps() {
    local bytes=$1 ms=$2
    awk -v b="$bytes" -v m="$ms" 'BEGIN {
        if (m <= 0) { print "0.0"; exit }
        printf "%.1f", (b / 1048576) / (m / 1000)
    }'
}

# speedup_pct <pesto_ms> <parpar_ms>  →  "+12.3%" or "-5.1%" relative to parpar
speedup_pct() {
    local p=$1 a=$2
    awk -v p="$p" -v a="$a" 'BEGIN {
        if (a <= 0 || p <= 0) { print "n/a"; exit }
        pct = (a / p - 1) * 100
        if (pct >= 0) printf "+%.1f%%", pct
        else          printf "%.1f%%", pct
    }'
}

# ── prerequisites ─────────────────────────────────────────────────────────────

fail() { red "ERROR: $*"; echo; exit 1; }

[[ -x "$PESTO" ]] || fail "pesto binary not found at $PESTO — run: cargo build --release"
command -v parpar >/dev/null 2>&1 || fail "parpar not found in PATH"

mkdir -p "$OUTDIR/pesto" "$OUTDIR/parpar"

# ── header ────────────────────────────────────────────────────────────────────

echo
bold "BENCHMARK: pesto vs parpar — PAR2 generation"; echo
echo "  Recovery : ${RECOVERY_PCT}%"
echo "  Slices   : ~${PARPAR_SLICES} (parpar -s${PARPAR_SLICES}; pesto auto-targets same range)"
echo "  Sizes    : ${SIZE_LIST[*]} GB"
printf "  CPU      : %s\n" "$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | xargs || echo unknown)"
printf "  Cores    : %s logical\n" "$(nproc 2>/dev/null || echo '?')"
hr

declare -A RES_MS RES_BYTES RES_OK

# ── run one benchmark ─────────────────────────────────────────────────────────

run_bench() {
    local tool=$1   # "pesto" or "parpar"
    local src=$2    # absolute path to source file
    local key="${tool}:${src}"

    local bytes
    bytes=$(stat -c%s "$src")

    # Drop page cache for a fair cold-read comparison (best-effort; needs sudo).
    sync
    echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null 2>&1 || true

    # Clean previous output so disk-write time is real each run.
    local stem
    stem=$(basename "$src")
    rm -f "$OUTDIR/pesto/${stem}"*.par2 \
          "$OUTDIR/parpar/${stem}"*.par2 2>/dev/null || true

    local start_ms end_ms elapsed_ms exit_code=0
    start_ms=$(date +%s%3N)

    if [[ "$tool" == "pesto" ]]; then
        # --par2-only writes par2 files next to the source; we cd into OUTDIR/pesto
        # and use a symlink so output lands there instead of beside the source.
        ln -sf "$src" "$OUTDIR/pesto/${stem}"
        (
            cd "$OUTDIR/pesto"
            "$PESTO" --par2-only --par2 "$RECOVERY_PCT" "${stem}"
        ) >/dev/null 2>&1 || exit_code=$?
        rm -f "$OUTDIR/pesto/${stem}"
    else
        parpar \
            -s "${PARPAR_SLICES}" \
            -r "${RECOVERY_PCT}%" \
            -o "$OUTDIR/parpar/${stem}.par2" \
            "$src" >/dev/null 2>&1 || exit_code=$?
    fi

    end_ms=$(date +%s%3N)
    elapsed_ms=$(( end_ms - start_ms ))

    RES_MS["$key"]="$elapsed_ms"
    RES_BYTES["$key"]="$bytes"
    RES_OK["$key"]="$exit_code"
}

# ── main loop ─────────────────────────────────────────────────────────────────

CSV="$OUTDIR/results.csv"
printf "file_gb,tool,elapsed_ms,throughput_mbps,ok\n" > "$CSV"

for size_gb in "${SIZE_LIST[@]}"; do
    file="bench_${size_gb}G.bin"
    src="$SCRIPT_DIR/$file"

    if [[ ! -f "$src" ]]; then
        printf "Creating %s (sparse)…\n" "$file"
        truncate -s "${size_gb}G" "$src"
    fi

    bold "── ${size_gb} GB ──"; echo

    for tool in pesto parpar; do
        printf "  %-8s  running… " "$tool"
        run_bench "$tool" "$src"

        key="${tool}:${src}"
        ms="${RES_MS[$key]}"
        ok="${RES_OK[$key]}"
        tp=$(throughput_mbps "${RES_BYTES[$key]}" "$ms")
        elapsed_str=$(secs_to_str $(( ms / 1000 )))

        if [[ "$ok" -ne 0 ]]; then
            printf '\r  %-8s  ' "$tool"; red "FAILED (exit $ok)"; echo
        else
            printf '\r  %-8s  %s  %s MB/s\n' "$tool" "$elapsed_str" "$tp"
        fi

        printf "%s,%s,%s,%s,%s\n" "$size_gb" "$tool" "$ms" "$tp" "$ok" >> "$CSV"
    done

    # Speedup line
    p_key="pesto:${src}"
    a_key="parpar:${src}"
    if [[ "${RES_OK[$p_key]}" -eq 0 && "${RES_OK[$a_key]}" -eq 0 ]]; then
        sp=$(speedup_pct "${RES_MS[$p_key]}" "${RES_MS[$a_key]}")
        if [[ "$sp" == +* ]]; then
            printf "  → pesto faster by "; green "$sp"; echo " vs parpar"
        else
            printf "  → pesto slower by "; red "$sp"; echo " vs parpar"
        fi
    fi
    echo
done

# ── summary table ─────────────────────────────────────────────────────────────

hr
bold "SUMMARY  (throughput MB/s, 10% recovery)"; echo
hr
printf "%-12s  %-12s  %-12s  %-14s\n" "Size" "pesto" "parpar" "pesto vs parpar"
hr

for size_gb in "${SIZE_LIST[@]}"; do
    src="$SCRIPT_DIR/bench_${size_gb}G.bin"
    p_key="pesto:${src}"
    a_key="parpar:${src}"

    p_ok="${RES_OK[$p_key]:-1}"
    a_ok="${RES_OK[$a_key]:-1}"

    p_tp=$( [[ "$p_ok" -eq 0 ]] && throughput_mbps "${RES_BYTES[$p_key]}" "${RES_MS[$p_key]}" || echo "FAIL" )
    a_tp=$( [[ "$a_ok" -eq 0 ]] && throughput_mbps "${RES_BYTES[$a_key]}" "${RES_MS[$a_key]}" || echo "FAIL" )

    if [[ "$p_ok" -eq 0 && "$a_ok" -eq 0 ]]; then
        sp=$(speedup_pct "${RES_MS[$p_key]}" "${RES_MS[$a_key]}")
    else
        sp="n/a"
    fi

    printf "%-12s  %-12s  %-12s  %-14s\n" "${size_gb}G" "$p_tp" "$a_tp" "$sp"
done

hr
printf "Results saved to: %s\n" "$CSV"
echo
