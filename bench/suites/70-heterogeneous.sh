#!/usr/bin/env bash
# bench/suites/70-heterogeneous.sh — #145: round-robin dispatch vs a slow peer.
#
# #129 gave each worker its own channel and round-robins articles onto them.
# That removed the shared-queue lock, but it also removed implicit
# work-stealing: an article is pinned to a worker (and therefore a server)
# up front. This suite measures that trade-off with two mock NNTP servers
# and three cases, all post-only, same connection count (4+4 = 8):
#
#   both-0      both servers 0 ms     — healthy dual-server baseline
#   both-50     both servers 50 ms    — every connection equally slow
#   hetero-0-50 one 0 ms, one 50 ms   — the realistic degraded backup
#
# If pinned dispatch cannot skip a backed-up worker, hetero-0-50 collapses
# toward both-50 (or worse: the producer blocks on the slow channel of
# depth 4 and starves the fast workers). A shared queue or shortest-queue
# send would keep the fast half busy.
#
# Small corpus on purpose: the shape is about article scheduling, not GB.

set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

SUITE=heterogeneous
HETERO_CONNS_EACH="${BENCH_HETERO_CONNS_EACH:-4}"
HETERO_SLOW_MS="${BENCH_HETERO_SLOW_MS:-50}"

suite_heterogeneous() {
    local wl=${1:-mixed-folder}
    local corpus bytes
    workload_materialise "$wl" > /dev/null
    workload_clean "$wl"
    corpus=$(workload_root "$wl")
    bytes=$(workload_bytes "$wl")
    workload_load "$wl"

    step "Heterogeneous servers — '$wl' ($(human_bytes "$bytes")), ${HETERO_CONNS_EACH}+${HETERO_CONNS_EACH} connections"

    local work="$BENCH_SANDBOX/heterogeneous/$wl"
    rm -rf "$work"; mkdir -p "$work"
    prime_cache "$corpus"

    hetero_case "$wl" "$bytes" "$work" "$corpus" both-0 0 0
    hetero_case "$wl" "$bytes" "$work" "$corpus" both-${HETERO_SLOW_MS} "$HETERO_SLOW_MS" "$HETERO_SLOW_MS"
    hetero_case "$wl" "$bytes" "$work" "$corpus" "hetero-0-${HETERO_SLOW_MS}" 0 "$HETERO_SLOW_MS"
}

hetero_case() {
    local wl=$1 bytes=$2 work=$3 corpus=$4 variant=$5 lat1=$6 lat2=$7
    local extra="scenario=hetero;lat1=$lat1;lat2=$lat2;conns_each=$HETERO_CONNS_EACH"

    mock_stop
    mock_start --latency-ms "$lat1"
    mock_start_secondary --latency-ms "$lat2"
    write_pesto_dual_config "$work/pesto.toml" 127.0.0.1 "$MOCK_PORT" "$MOCK_PORT2" \
        "$HETERO_CONNS_EACH"

    echo
    info "servers ${lat1} ms + ${lat2} ms ($variant)"

    bench_case --suite "$SUITE" --workload "$wl" --tool pesto \
        --version "$("$PESTO_BIN" --version 2>/dev/null | head -1)" \
        --variant "$variant" --input-bytes "$bytes" --extra "$extra" \
        --setup "rm -f '$work/pesto.nzb'" \
        --after "CASE_ARTICLES=\$(nzb_segment_count '$work/pesto.nzb')" \
        -- "$PESTO_BIN" --config "$work/pesto.toml" \
           --par2 0 --no-check --obfuscate=none \
           --article-size "$WL_ARTICLE_SIZE" --line-length 128 \
           --no-hooks --no-history --no-notify --no-session-log \
           --output-format json -o "$work/pesto.nzb" "$corpus"

    mock_stop
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    bench_standalone_init "$SUITE"
    suite_heterogeneous "${@:-mixed-folder}"
    bench_standalone_finish
fi
