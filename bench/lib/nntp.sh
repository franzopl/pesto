#!/usr/bin/env bash
# bench/lib/nntp.sh — the local mock NNTP server's lifecycle.
#
# End-to-end posting numbers are only reproducible if the server is not part
# of the experiment. A real provider adds route latency, per-account
# concurrency caps, and time-of-day variance that swamp the differences
# between posters — and requires an account, which requirement 4 rules out.
#
# So every posting benchmark runs against `examples/mock_nntp_server`, which
# ACKs POST/STAT as fast as the kernel will carry them, and can be told to add
# a fixed per-response latency. That knob is the interesting one: at 0 ms a
# poster is measured on raw encode-and-write throughput, while at 30–50 ms
# (a realistic transatlantic RTT) the measurement becomes about pipelining and
# connection scheduling instead. Tools differ far more on the second than the
# first, and only the mock lets both be measured on the same machine.

MOCK_PID=""
MOCK_PORT=""
MOCK_STATS_FILE=""

# mock_start [--latency-ms N] [--save-dir DIR] [--drop-pct N] [--miss-pct N]
#
# Binds port 0 and reads back the port the kernel assigned, so concurrent runs
# (and a leftover server from an interrupted run) cannot collide on a
# hard-coded port.
mock_start() {
    local log="$BENCH_RUN_DIR/mock.log"
    MOCK_STATS_FILE="$BENCH_SANDBOX/mock-stats.json"
    rm -f "$MOCK_STATS_FILE"

    "$MOCK_NNTP_BIN" --port 0 --quiet --stats-file "$MOCK_STATS_FILE" "$@" \
        > "$log" 2>&1 &
    MOCK_PID=$!
    # Armed before the readiness wait, not after: if the wait itself fails the
    # server is already running, and a trap installed later would never fire —
    # leaving an orphaned listener behind for every aborted run. The shared
    # handler (see bench/lib.sh) also finishes a standalone run's summary,
    # which a `trap mock_stop` here would silently replace.
    bench_arm_exit_trap

    # Wait for the "listening on 127.0.0.1:<port>" line rather than sleeping a
    # fixed amount: a fixed sleep is either flaky on a loaded machine or wasted
    # time on an idle one.
    local waited=0
    while (( waited < 100 )); do
        MOCK_PORT=$(sed -n 's/^listening on 127\.0\.0\.1:\([0-9]*\).*/\1/p' "$log" 2>/dev/null | head -1)
        [[ -n $MOCK_PORT ]] && break
        kill -0 "$MOCK_PID" 2>/dev/null || die "mock NNTP server died on startup: $(cat "$log")"
        sleep 0.05
        # `waited=$(( … ))`, not `(( waited++ ))`: post-increment evaluates to
        # the *old* value, so the first iteration would make the arithmetic
        # command exit 1 and `set -e` would kill the run before the server had
        # a chance to come up.
        waited=$(( waited + 1 ))
    done
    [[ -n $MOCK_PORT ]] || die "mock NNTP server never reported a port"
}

mock_stop() {
    [[ -n $MOCK_PID ]] || return 0
    # SIGTERM, not SIGKILL: the server writes its JSON stats summary from the
    # signal handler, and that summary is how the suite verifies every tool
    # actually posted the same number of articles.
    kill -TERM "$MOCK_PID" 2>/dev/null || true
    wait "$MOCK_PID" 2>/dev/null || true
    MOCK_PID=""
}

# mock_final_stats — the JSON summary the server writes on SIGTERM:
# connections, articles, bytes, STAT commands, and the rates it saw.
#
# Only available after `mock_stop`, so per-case article counts come from each
# tool's own NZB instead (see `nzb_segment_count`). This is the whole-run
# cross-check: if the server counted far fewer articles than the NZBs claim,
# something was not actually posted.
mock_final_stats() {
    [[ -r ${MOCK_STATS_FILE:-} ]] && cat "$MOCK_STATS_FILE" || echo '{}'
}

# nzb_segment_count <nzb> — how many articles a poster says it wrote.
#
# Every tool here emits an NZB with one <segment> per posted article, so this
# is a tool-independent cross-check of the work each one did, available even
# for the tools that report nothing useful on stdout.
nzb_segment_count() {
    [[ -r $1 ]] || { echo 0; return; }
    grep -c '<segment' "$1" 2>/dev/null || echo 0
}
