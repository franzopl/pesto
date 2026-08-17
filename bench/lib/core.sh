#!/usr/bin/env bash
# bench/lib/core.sh — paths, terminal output, and the safety sandbox.
#
# Sourced by every suite (indirectly, via bench/lib.sh). Defines nothing that
# depends on a specific benchmark, only the ground rules every one of them
# runs under.

# Bash 4.4+ is required: associative arrays and `${var,,}` need 4.0, and
# expanding a possibly-empty array as `"${arr[@]}"` under `set -u` only stopped
# being an error in 4.4 — the suites rely on that to pass optional flag groups
# (compression, for instance) through to a tool. macOS ships 3.2;
# `brew install bash` is the fix there.
if [[ -z "${BASH_VERSINFO:-}" ]] ||
   (( BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4) )); then
    echo "bench: bash 4.4+ required (found ${BASH_VERSION:-unknown})" >&2
    exit 1
fi

BENCH_DIR="${BENCH_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
REPO_ROOT="${REPO_ROOT:-$(cd "$BENCH_DIR/.." && pwd)}"
BENCH_DATA_DIR="${BENCH_DATA_DIR:-$BENCH_DIR/data}"
BENCH_RESULTS_DIR="${BENCH_RESULTS_DIR:-$BENCH_DIR/results}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release"

# Created on load: several helpers (free-space checks, the filesystem
# fingerprint) stat this path before anything has had a reason to create it,
# and a failing `df` under `set -e` aborts the run with no message at all.
mkdir -p "$BENCH_DATA_DIR" "$BENCH_RESULTS_DIR"

PESTO_BIN="${PESTO_BIN:-$TARGET_DIR/pesto}"
PARMESAN_BIN="${PARMESAN_BIN:-$TARGET_DIR/parmesan}"
BENCH_GEN_BIN="${BENCH_GEN_BIN:-$TARGET_DIR/examples/bench-gen}"
YENC_BENCH_BIN="${YENC_BENCH_BIN:-$TARGET_DIR/examples/yenc-bench}"
MOCK_NNTP_BIN="${MOCK_NNTP_BIN:-$TARGET_DIR/examples/mock_nntp_server}"

# ── terminal helpers ─────────────────────────────────────────────────────────

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
    bold()   { printf '\033[1m%s\033[0m' "$1"; }
    green()  { printf '\033[32m%s\033[0m' "$1"; }
    red()    { printf '\033[31m%s\033[0m' "$1"; }
    yellow() { printf '\033[33m%s\033[0m' "$1"; }
    dim()    { printf '\033[2m%s\033[0m' "$1"; }
else
    bold()   { printf '%s' "$1"; }
    green()  { printf '%s' "$1"; }
    red()    { printf '%s' "$1"; }
    yellow() { printf '%s' "$1"; }
    dim()    { printf '%s' "$1"; }
fi

hr()   { printf '%0.s─' {1..76}; echo; }
info() { printf '  %s\n' "$*"; }
warn() { printf '  %s %s\n' "$(yellow warning:)" "$*" >&2; }
die()  { printf '%s %s\n' "$(red 'bench error:')" "$*" >&2; exit 1; }
step() { printf '\n%s\n' "$(bold "── $* ──")"; }

# ── unit helpers ─────────────────────────────────────────────────────────────

# human_bytes <bytes> → "8.0 GiB"
human_bytes() {
    awk -v b="$1" 'BEGIN {
        split("B KiB MiB GiB TiB", u, " ")
        i = 1
        while (b >= 1024 && i < 5) { b /= 1024; i++ }
        printf (i == 1 ? "%d %s" : "%.1f %s"), b, u[i]
    }'
}

# ms_to_str <ms> → "1m23s" or "4.2s"
ms_to_str() {
    awk -v ms="$1" 'BEGIN {
        s = ms / 1000
        if (s >= 60) printf "%dm%02ds", int(s / 60), s % 60
        else         printf "%.2fs", s
    }'
}

# throughput_mbps <bytes> <ms> — MiB/s, the unit every table in this suite uses.
throughput_mbps() {
    awk -v b="$1" -v m="$2" 'BEGIN {
        if (m <= 0) { print "0.0"; exit }
        printf "%.1f", (b / 1048576) / (m / 1000)
    }'
}

file_size_bytes() { stat -c%s "$1" 2>/dev/null || stat -f%z "$1"; }

# dir_size_bytes <dir> [glob] — total size of matching files, 0 if none.
dir_size_bytes() {
    local dir=$1 pattern=${2:-*} total=0 f
    shopt -s nullglob
    for f in "$dir"/$pattern; do
        [[ -f $f ]] && total=$(( total + $(file_size_bytes "$f") ))
    done
    shopt -u nullglob
    echo "$total"
}

# ── the sandbox ──────────────────────────────────────────────────────────────
#
# Every tool the suite runs is confined to a throwaway HOME/XDG/TMPDIR. This
# is not tidiness — it is the mechanism that makes the "no real Usenet account
# needed" guarantee real:
#
#   * pesto resolves its config from $XDG_CONFIG_HOME/pesto/config.toml, and
#     runs hook scripts from $XDG_CONFIG_HOME/pesto/hooks/. Pointing both at
#     an empty directory means a benchmark can never pick up the operator's
#     real server credentials, and can never fire their indexer hooks
#     (CLAUDE.md: a test that sends real data to an external system is a bug).
#   * ngPost reads ~/.ngPost by default, which on a real machine contains a
#     real provider. The suite always passes an explicit generated -c config.
#
# On top of that, `assert_local_target` refuses to run anything aimed at a
# host that is not loopback.

bench_sandbox_init() {
    local root=$1
    export BENCH_SANDBOX="$root"
    export HOME="$root/home"
    export XDG_CONFIG_HOME="$root/config"
    export XDG_DATA_HOME="$root/data"
    export XDG_CACHE_HOME="$root/cache"
    export TMPDIR="$root/tmp"
    mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$TMPDIR"
    # Keep locale-dependent number formatting out of every tool's output; the
    # parsers here expect '.' as the decimal separator.
    # C.UTF-8, not C: Qt (ngPost) warns loudly and switches locale on a
    # non-UTF-8 one, and the warning lands in the middle of the captured log.
    # Both keep "." as the decimal separator, which is what the parsers need.
    export LC_ALL=C.UTF-8
}

# assert_local_target <host> — hard stop before any network-facing run.
assert_local_target() {
    case "$1" in
        127.0.0.1|::1|localhost) return 0 ;;
        *) die "refusing to benchmark against non-loopback host '$1'. \
The suite only ever posts to its own mock NNTP server." ;;
    esac
}

# ── build ────────────────────────────────────────────────────────────────────

# ensure_built — build every binary the suite drives, once.
ensure_built() {
    local missing=()
    [[ -x $PESTO_BIN ]]      || missing+=("pesto")
    [[ -x $PARMESAN_BIN ]]   || missing+=("parmesan")
    [[ -x $BENCH_GEN_BIN ]]  || missing+=("bench-gen")
    [[ -x $YENC_BENCH_BIN ]] || missing+=("yenc-bench")
    [[ -x $MOCK_NNTP_BIN ]]  || missing+=("mock_nntp_server")
    [[ ${#missing[@]} -eq 0 ]] && return 0

    info "building missing binaries: ${missing[*]}"
    command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust from https://rustup.rs"
    ( cd "$REPO_ROOT" && cargo build --release \
        -p pesto-poster -p parmesan-par2 \
        --bins \
        --example bench-gen --example yenc-bench --example mock_nntp_server ) \
        || die "cargo build failed"
}
