#!/usr/bin/env bash
# bench/suites/30-stages.sh — Layer B: attributing cost to pipeline stages.
#
# The same corpus, run through pesto with progressively more of the pipeline
# switched on. Each stage is a real invocation, not an instrumented one, so
# nothing here depends on internal timers that could drift from what the
# binary actually does:
#
#   read        cat > /dev/null. The floor: what the storage can deliver.
#   yenc        --dry-run --par2 0. Read, yEnc-encode, build article headers,
#               write the NZB. Everything except PAR2 and the network.
#   yenc+par2   --dry-run. Adds recovery generation, overlapped with encoding
#               exactly as it is during a real post.
#   par2-only   --par2-only. Recovery generation with no article pipeline at
#               all, written next to the sources.
#   compress    --dry-run --compress. Adds archiving.
#   post        against the mock server, --par2 0. Adds NNTP framing, the
#               connection pool and the socket writes.
#   post+par2   the default streaming pipeline: recovery computed while the
#               data articles are already going out.
#   post+par2-pre  --par2-before-upload: generate everything, then post. The
#               two-phase shape that parpar+nyuu and ngPost both have.
#   post+check  adds the streaming STAT confirmation pass.
#
# Differences between adjacent stages are the attribution, and the report
# computes them. The one to watch is (post+par2-pre) − (post+par2): that
# difference is the entire value of overlapping PAR2 with the upload, and it
# is the reason pesto's default is not the two-phase shape.

set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

SUITE=stages

suite_stages() {
    local wl
    for wl in "$@"; do
        stages_workload "$wl"
    done
}

stages_workload() {
    local wl=$1 corpus bytes
    workload_materialise "$wl" > /dev/null
    workload_clean "$wl"
    corpus=$(workload_root "$wl")
    bytes=$(workload_bytes "$wl")
    workload_load "$wl"

    step "Pipeline stages — workload '$wl' ($(human_bytes "$bytes"))"

    local work="$BENCH_SANDBOX/stages/$wl"
    rm -rf "$work"; mkdir -p "$work"
    local cfg="$work/pesto.toml"

    mock_start --latency-ms 0
    write_pesto_config "$cfg" 127.0.0.1 "$MOCK_PORT" "$WL_CONNECTIONS"
    info "mock NNTP server on 127.0.0.1:$MOCK_PORT"
    echo

    prime_cache "$corpus"

    # Baseline: the storage floor. Every stage below includes this cost, so a
    # pipeline number that looks poor is only meaningful next to it.
    bench_case --suite "$SUITE" --workload "$wl" --tool "storage" \
        --version "-" --variant "read" --input-bytes "$bytes" \
        --extra "stage=read" \
        -- bash -c 'find "$1" -type f ! -name ".manifest.jsonl" -exec cat {} + > /dev/null' _ "$corpus"

    stage_case "$wl" "$bytes" "$work" yenc \
        --dry-run --par2 0

    stage_case "$wl" "$bytes" "$work" yenc+par2 \
        --dry-run --par2 "$WL_PAR2_PCT"

    stage_case "$wl" "$bytes" "$work" par2-only \
        --par2-only --par2 "$WL_PAR2_PCT"

    # Archiving, measured against the `yenc` stage above (same flags plus
    # --compress), so the difference is the archiver and nothing else. The
    # workload's format name is also its binary name for both supported
    # formats: `7z` and `rar`.
    if [[ -n $WL_COMPRESS ]] && tool_present "$WL_COMPRESS"; then
        local compress_flags=(--compress="$WL_COMPRESS" --compress-temp-dir "$work/compress")
        [[ -n $WL_COMPRESS_VOLUME ]] &&
            compress_flags+=(--compress-volume-size "$WL_COMPRESS_VOLUME")
        stage_case "$wl" "$bytes" "$work" compress \
            --dry-run --par2 0 "${compress_flags[@]}"
    fi

    stage_case "$wl" "$bytes" "$work" post \
        --config "$cfg" --par2 0 --no-check

    stage_case "$wl" "$bytes" "$work" post+par2 \
        --config "$cfg" --par2 "$WL_PAR2_PCT" --no-check

    stage_case "$wl" "$bytes" "$work" post+par2-pre \
        --config "$cfg" --par2 "$WL_PAR2_PCT" --no-check --par2-before-upload

    stage_case "$wl" "$bytes" "$work" post+check \
        --config "$cfg" --par2 "$WL_PAR2_PCT" --check --check-delay 0

    mock_stop
}

# stage_case <workload> <bytes> <workdir> <stage> <pesto flags…>
#
# Common flags for every stage. --no-hooks/--no-history/--no-notify keep the
# measurement free of side effects (and keep a benchmark from firing the
# operator's indexer hooks, which CLAUDE.md forbids outright); the sandbox
# already guarantees there are none to find, this is belt and braces.
stage_case() {
    local wl=$1 bytes=$2 work=$3 stage=$4; shift 4
    local corpus; corpus=$(workload_root "$wl")
    local nzb="$work/$stage.nzb"

    # The setup sweep is not housekeeping: --par2-only writes its recovery set
    # beside the sources by design, so without this the *next* stage would walk
    # the corpus, find the previous stage's .par2 files and post them as if
    # they were input — silently inflating both stages' byte counts.
    # `workload_clean` also covers the set --par2-only leaves one level *above*
    # a directory input, which a sweep of the corpus alone would miss.
    bench_case --suite "$SUITE" --workload "$wl" --tool pesto \
        --version "$("$PESTO_BIN" --version 2>/dev/null | head -1)" \
        --variant "$stage" --input-bytes "$bytes" \
        --extra "stage=$stage;connections=$WL_CONNECTIONS;par2_pct=$WL_PAR2_PCT" \
        --setup "rm -f '$nzb'; workload_clean '$wl'" \
        --after "CASE_ARTICLES=\$(nzb_segment_count '$nzb')" \
        -- "$PESTO_BIN" "$@" \
           --groups alt.binaries.test --from "bench <bench@localhost>" \
           --article-size "$WL_ARTICLE_SIZE" \
           --connections "$WL_CONNECTIONS" \
           --no-hooks --no-history --no-notify --no-session-log \
           --output-format json -o "$nzb" "$corpus"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    bench_standalone_init "$SUITE"
    suite_stages "${@:-many-small}"
fi
