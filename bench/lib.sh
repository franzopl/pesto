#!/usr/bin/env bash
# bench/lib.sh — the single include for every benchmark script.
#
#   source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"   # suites/
#   source "$(dirname "$0")/lib.sh"                                    # bench/
#
# Composes the modules under bench/lib/. Sourcing it twice is harmless, which
# matters because each suite includes it so it stays runnable on its own, and
# run.sh has already included it by the time it sources a suite.

set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$BENCH_DIR/.." && pwd)"

# shellcheck source=bench/lib/core.sh
source "$BENCH_DIR/lib/core.sh"
# shellcheck source=bench/lib/sysinfo.sh
source "$BENCH_DIR/lib/sysinfo.sh"
# shellcheck source=bench/lib/stats.sh
source "$BENCH_DIR/lib/stats.sh"
# shellcheck source=bench/lib/record.sh
source "$BENCH_DIR/lib/record.sh"
# shellcheck source=bench/lib/measure.sh
source "$BENCH_DIR/lib/measure.sh"
# shellcheck source=bench/lib/data.sh
source "$BENCH_DIR/lib/data.sh"
# shellcheck source=bench/lib/tools.sh
source "$BENCH_DIR/lib/tools.sh"
# shellcheck source=bench/lib/nntp.sh
source "$BENCH_DIR/lib/nntp.sh"
# shellcheck source=bench/lib/report.sh
source "$BENCH_DIR/lib/report.sh"

# bench_standalone_init <suite> — set up a run directory when a suite script is
# executed directly instead of through run.sh. Keeps every suite independently
# runnable (`./bench/suites/20-par2.sh`), which is how you iterate on one
# without waiting for the others.
bench_standalone_init() {
    local suite=$1
    if [[ -n ${BENCH_RUN_DIR:-} ]]; then
        return 0   # already inside a run.sh invocation
    fi
    ensure_built
    detect_tools
    BENCH_RUN_DIR="$BENCH_RESULTS_DIR/$(hostname 2>/dev/null || uname -n)/$(date -u '+%Y%m%dT%H%M%SZ')-$suite"
    mkdir -p "$BENCH_RUN_DIR"
    bench_sandbox_init "$BENCH_RUN_DIR/sandbox"
    record_init
    write_system_json "$BENCH_RUN_DIR/system.json"
    BENCH_STANDALONE=1
    bench_arm_exit_trap
}

bench_standalone_finish() {
    [[ ${BENCH_STANDALONE:-0} == 1 ]] || return 0
    BENCH_STANDALONE=0          # idempotent: the trap can fire more than once
    summarise "$BENCH_RAW_CSV" "$BENCH_RUN_DIR/summary.csv"
    summary_to_json "$BENCH_RUN_DIR/summary.csv" "$BENCH_RUN_DIR/system.json" \
        "$BENCH_RUN_DIR/results.json"
    report_build "$BENCH_RUN_DIR" > /dev/null || true
    # `latest` is the path humans and scripts both reach for (it is what
    # bench/compare.sh examples use), so a suite run on its own has to
    # maintain it too — not just a full run.sh invocation.
    ln -sfn "$(basename "$BENCH_RUN_DIR")" "$(dirname "$BENCH_RUN_DIR")/latest"
    echo
    info "results: $BENCH_RUN_DIR"
}

# One exit handler for the whole suite.
#
# There used to be two — `mock_start` installed its own, and so did
# `bench_standalone_init`. `trap` replaces rather than appends, so whichever
# ran second silently disabled the other: running a posting suite standalone
# started a mock server and then never wrote its summary, because the mock's
# trap had overwritten the one that does that.
bench_exit_handler() {
    mock_stop || true
    bench_standalone_finish || true
}

bench_arm_exit_trap() {
    trap bench_exit_handler EXIT INT TERM
}
