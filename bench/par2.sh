#!/usr/bin/env bash
# bench/par2.sh — PAR2 creation throughput: parmesan vs parpar vs par2cmdline
#
# Usage: ./bench/par2.sh [SIZE_GB ...]
#   Default sizes: 1 5 (GB)
#   Example:       ./bench/par2.sh 1 5 10
#
# All tools are run with matched parameters:
#   - 10% recovery
#   - ~1000 input slices (controlled via --slice-count / -s flags)
#
# Test files are filled with random data (NOT sparse/zero files).
# Zero files allow trivially fast GF(2^16) computation and produce
# meaningless results for parpar and par2cmdline.
#
# Requirements:
#   - cargo build --release (builds parmesan binary)
#   - parpar in PATH (optional; skipped if absent)
#   - par2 in PATH  (optional; skipped if absent)
#
# Results are written to bench/results/par2-<hostname>-<date>.csv

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=bench/lib.sh
source "$REPO_ROOT/bench/lib.sh"

PARMESAN="$REPO_ROOT/target/release/parmesan"
RESULTS_DIR="$REPO_ROOT/bench/results"
OUTDIR="$REPO_ROOT/bench/par2_out"
RECOVERY_PCT=10
# Target ~1000 input slices so all tools do comparable amounts of work.
# parmesan: --slice-count 1000
# parpar:   -s1000 (slices)
# par2:     -n1000 (blocks)
TARGET_SLICES=1000

if [[ $# -gt 0 ]]; then
    SIZE_LIST=("$@")
else
    SIZE_LIST=(1 5)
fi

# ── prerequisites ─────────────────────────────────────────────────────────────

[[ -x "$PARMESAN" ]] || {
    echo "parmesan binary not found. Building…"
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
}

HAS_PARPAR=false; command -v parpar >/dev/null 2>&1 && HAS_PARPAR=true
HAS_PAR2=false;   command -v par2   >/dev/null 2>&1 && HAS_PAR2=true

mkdir -p "$OUTDIR/parmesan" "$OUTDIR/parpar" "$OUTDIR/par2cmdline" "$RESULTS_DIR"

HOSTNAME_SLUG="$(cat /etc/hostname 2>/dev/null || uname -n 2>/dev/null || echo "local")"
HOSTNAME_SLUG="${HOSTNAME_SLUG//./-}"
DATE_SLUG="$(date -u '+%Y%m%d')"
CSV="$RESULTS_DIR/par2-${HOSTNAME_SLUG}-${DATE_SLUG}.csv"
csv_header "$CSV" "size_gb" "tool" "recovery_pct" "slices" "elapsed_ms" "throughput_mbps"

# ── header ────────────────────────────────────────────────────────────────────

echo
bold "BENCHMARK: PAR2 creation throughput"; echo
print_system_info
echo
printf "  Recovery : %s%%\n" "$RECOVERY_PCT"
printf "  Slices   : ~%s input slices (matched across tools)\n" "$TARGET_SLICES"
printf "  Sizes    : %s GB\n" "${SIZE_LIST[*]}"
printf "  Files    : random data (not sparse)\n"
printf "  Tools    : parmesan"
$HAS_PARPAR && printf ", parpar"
$HAS_PAR2   && printf ", par2cmdline"
echo; hr

# ── run helpers ───────────────────────────────────────────────────────────────

declare -A R_MS R_BYTES

_record() {
    local tool=$1 size_gb=$2 bytes=$3 elapsed_ms=$4
    R_MS["${tool}:${size_gb}"]=$elapsed_ms
    R_BYTES["${tool}:${size_gb}"]=$bytes
    local tp
    tp=$(throughput_mbps "$bytes" "$elapsed_ms")
    csv_row "$CSV" "$size_gb" "$tool" "$RECOVERY_PCT" "$TARGET_SLICES" "$elapsed_ms" "$tp"
    printf "  %-14s  %s  %s MB/s\n" "$tool" "$(ms_to_str "$elapsed_ms")" "$tp"
}

run_parmesan() {
    local size_gb=$1 src=$2
    local bytes; bytes=$(file_size_bytes "$src")
    local stem; stem=$(basename "$src")

    rm -f "$OUTDIR/parmesan/${stem}"*.par2
    ln -sf "$src" "$OUTDIR/parmesan/${stem}"

    drop_caches
    local t0 t1 exit_code=0
    t0=$(now_ms)
    (cd "$OUTDIR/parmesan" \
        && "$PARMESAN" --recovery-pct "$RECOVERY_PCT" --slice-count "$TARGET_SLICES" "$stem" \
    ) >/dev/null 2>&1 || exit_code=$?
    t1=$(now_ms)

    rm -f "$OUTDIR/parmesan/${stem}"
    if [[ $exit_code -ne 0 ]]; then
        printf "  %-14s  FAILED (exit %s)\n" "parmesan" "$exit_code"; return
    fi
    _record "parmesan" "$size_gb" "$bytes" $(( t1 - t0 ))
}

run_parpar() {
    local size_gb=$1 src=$2
    local bytes; bytes=$(file_size_bytes "$src")
    local stem; stem=$(basename "$src")

    rm -f "$OUTDIR/parpar/${stem}"*.par2

    drop_caches
    local t0 t1 exit_code=0
    t0=$(now_ms)
    # -s<N>: target slice count; -r: recovery percentage
    parpar -s "${TARGET_SLICES}" -r "${RECOVERY_PCT}%" \
        -o "$OUTDIR/parpar/${stem}.par2" "$src" >/dev/null 2>&1 || exit_code=$?
    t1=$(now_ms)

    if [[ $exit_code -ne 0 ]]; then
        printf "  %-14s  FAILED (exit %s)\n" "parpar" "$exit_code"; return
    fi
    _record "parpar" "$size_gb" "$bytes" $(( t1 - t0 ))
}

run_par2cmdline() {
    local size_gb=$1 src=$2
    local bytes; bytes=$(file_size_bytes "$src")
    local stem; stem=$(basename "$src")

    rm -f "$OUTDIR/par2cmdline/${stem}"*.par2
    ln -sf "$src" "$OUTDIR/par2cmdline/${stem}"

    drop_caches
    local t0 t1 exit_code=0
    t0=$(now_ms)
    # -b: block count; -r: redundancy %; first positional arg is output basename
    (cd "$OUTDIR/par2cmdline" \
        && par2 create -r"$RECOVERY_PCT" -b"$TARGET_SLICES" "$stem" "$stem" \
    ) >/dev/null 2>&1 || exit_code=$?
    t1=$(now_ms)

    rm -f "$OUTDIR/par2cmdline/${stem}"

    if [[ $exit_code -ne 0 ]]; then
        printf "  %-14s  FAILED (exit %s)\n" "par2cmdline" "$exit_code"
        return
    fi
    _record "par2cmdline" "$size_gb" "$bytes" $(( t1 - t0 ))
}

# ── main loop ─────────────────────────────────────────────────────────────────

for size_gb in "${SIZE_LIST[@]}"; do
    local_src="$REPO_ROOT/bench_${size_gb}G.bin"
    ensure_bench_file "$local_src" "$(( size_gb * 1073741824 ))"

    bold "── ${size_gb} GB ──"; echo
    run_parmesan "$size_gb" "$local_src"
    $HAS_PARPAR && run_parpar       "$size_gb" "$local_src" \
               || printf "  %-14s  (not installed — skipped)\n" "parpar"
    $HAS_PAR2   && run_par2cmdline  "$size_gb" "$local_src" \
               || printf "  %-14s  (not installed — skipped)\n" "par2cmdline"
    echo
done

# ── summary table ─────────────────────────────────────────────────────────────

hr
bold "SUMMARY  (throughput MB/s, ${RECOVERY_PCT}% recovery, ~${TARGET_SLICES} slices)"; echo
hr

COLS=("Size" "parmesan (MB/s)")
$HAS_PARPAR && COLS+=("parpar (MB/s)" "vs parpar")
$HAS_PAR2   && COLS+=("par2cmdline (MB/s)" "vs par2cmdline")
md_header "${COLS[@]}"

for size_gb in "${SIZE_LIST[@]}"; do
    pm_ms="${R_MS["parmesan:${size_gb}"]:-0}"
    pm_bytes="${R_BYTES["parmesan:${size_gb}"]:-0}"
    pm_tp=$(throughput_mbps "$pm_bytes" "$pm_ms")

    row=("${size_gb} GB" "$pm_tp")

    if $HAS_PARPAR; then
        pp_ms="${R_MS["parpar:${size_gb}"]:-0}"
        pp_bytes="${R_BYTES["parpar:${size_gb}"]:-0}"
        pp_tp=$(throughput_mbps "$pp_bytes" "$pp_ms")
        sp=$(speedup_pct "$pm_ms" "$pp_ms")
        row+=("$pp_tp" "$sp")
    fi

    if $HAS_PAR2; then
        p2_ms="${R_MS["par2cmdline:${size_gb}"]:-0}"
        p2_bytes="${R_BYTES["par2cmdline:${size_gb}"]:-0}"
        p2_tp=$(throughput_mbps "$p2_bytes" "$p2_ms")
        sp=$(speedup_pct "$pm_ms" "$p2_ms")
        row+=("$p2_tp" "$sp")
    fi

    md_row "${row[@]}"
done

echo
printf "Raw results: %s\n" "$CSV"
echo
