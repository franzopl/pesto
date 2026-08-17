#!/usr/bin/env bash
# bench/suites/50-scaling.sh — how throughput responds to the knobs.
#
# Two curves, both of which answer a question a single number cannot:
#
#   connections × latency   Throughput against connection count, at several
#                           simulated round-trip times. On a 0 ms link more
#                           connections buy almost nothing and the curve is
#                           flat; at 50 ms the curve is the whole story,
#                           because each connection can only have so many
#                           articles in flight. This is where a poster's
#                           pipelining strategy shows up, and it is the
#                           setting real users get wrong most often.
#
#   PAR2 threads            parmesan's scaling across thread counts. Reed-
#                           Solomon encoding is embarrassingly parallel in
#                           principle; where the curve flattens says whether
#                           the limit is arithmetic, memory bandwidth, or the
#                           read pass.
#
# Output is a plain CSV of points, deliberately: this suite exists to be
# plotted, not to be read as a table.
#
# Run it on a SMALL workload. The cost of one cell is roughly
# `articles x latency / connections`, so the 1-connection/50 ms cell of a
# 1.5 GiB corpus is ~5 minutes *per repetition* — half an hour for one point
# that says nothing the 10 ms curve did not already show. What this suite
# measures is the shape of the curve, and the shape does not depend on corpus
# size. `WL_LAYERS` is how a workload opts in; keep it on the quick tier.

set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

SUITE=scaling
SCALING_CONNECTIONS="${BENCH_SCALING_CONNECTIONS:-1,2,4,8,16,32}"
SCALING_LATENCIES="${BENCH_SCALING_LATENCIES:-0,10,50}"

suite_scaling() {
    local wl=${1:-mixed-folder}
    scaling_connections "$wl"
    scaling_par2_threads "$wl"
}

scaling_connections() {
    local wl=$1 corpus bytes
    workload_materialise "$wl" > /dev/null
    workload_clean "$wl"
    corpus=$(workload_root "$wl")
    bytes=$(workload_bytes "$wl")
    workload_load "$wl"

    step "Connection scaling — '$wl' ($(human_bytes "$bytes"))"

    local work="$BENCH_SANDBOX/scaling/$wl"
    rm -rf "$work"; mkdir -p "$work"
    prime_cache "$corpus"

    local latency conns
    for latency in ${SCALING_LATENCIES//,/ }; do
        mock_start --latency-ms "$latency"
        write_pesto_config "$work/pesto.toml" 127.0.0.1 "$MOCK_PORT" 8
        echo
        info "server latency ${latency} ms"

        for conns in ${SCALING_CONNECTIONS//,/ }; do
            local extra="latency_ms=$latency;connections=$conns"

            bench_case --suite "$SUITE" --workload "$wl" --tool pesto \
                --version "$("$PESTO_BIN" --version 2>/dev/null | head -1)" \
                --variant "conn$conns-l$latency" --input-bytes "$bytes" \
                --extra "$extra" \
                --setup "rm -f '$work/pesto.nzb'" \
                -- "$PESTO_BIN" --config "$work/pesto.toml" \
                   --par2 0 --no-check --connections "$conns" \
                   --article-size "$WL_ARTICLE_SIZE" --line-length 128 \
                   --no-hooks --no-history --no-notify --no-session-log \
                   --output-format json -o "$work/pesto.nzb" "$corpus"

            if tool_present nyuu; then
                bench_case --suite "$SUITE" --workload "$wl" --tool nyuu \
                    --version "$(tool_version nyuu)" \
                    --variant "conn$conns-l$latency" --input-bytes "$bytes" \
                    --extra "$extra" \
                    --setup "rm -f '$work/nyuu.nzb'" \
                    -- nyuu -h 127.0.0.1 -P "$MOCK_PORT" -u bench -p bench \
                       -n "$conns" -a "$WL_ARTICLE_SIZE" --article-line-size 128 \
                       --check-tries 0 -g alt.binaries.test \
                       -f 'bench <bench@localhost>' -r keep \
                       -o "$work/nyuu.nzb" -O -q "$corpus"
            fi
        done
        mock_stop
    done
}

scaling_par2_threads() {
    local wl=$1 corpus bytes
    workload_materialise "$wl" > /dev/null
    workload_clean "$wl"
    corpus=$(workload_root "$wl")
    bytes=$(workload_bytes "$wl")

    step "PAR2 thread scaling — '$wl'"

    local out="$BENCH_SANDBOX/scaling-par2"
    rm -rf "$out"; mkdir -p "$out"
    mapfile -t inputs < <(find "$corpus" -type f ! -name '.manifest.jsonl' | sort)
    par2_geometry "$corpus" "${BENCH_PAR2_SLICES:-2000}" 10

    # 1 → all physical cores, doubling. Past the physical core count the
    # curve usually turns down rather than flat, which is itself the result.
    local max threads=1 list=()
    max=$(cpu_cores_physical)
    while (( threads <= max )); do
        list+=("$threads")
        threads=$(( threads * 2 ))
    done
    [[ " ${list[*]} " == *" $max "* ]] || list+=("$max")

    prime_cache "$corpus"
    local t saved=$BENCH_PAR2_THREADS
    for t in "${list[@]}"; do
        BENCH_PAR2_THREADS=$t
        par2_cmd_parmesan "$out" scaling "${inputs[@]}"
        bench_case --suite "$SUITE" --workload "$wl" --tool parmesan \
            --version "$("$PARMESAN_BIN" --version 2>/dev/null | head -1)" \
            --variant "par2-t$t" --input-bytes "$bytes" \
            --extra "threads=$t;$(par2_geometry_note)" \
            --setup "rm -f '$out'/*.par2" \
            --after "CASE_OUTPUT_BYTES=\$(dir_size_bytes '$out' '*.par2')" \
            -- "${P2_CMD[@]}"
    done
    BENCH_PAR2_THREADS=$saved
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    bench_standalone_init "$SUITE"
    suite_scaling "${@:-many-small}"
fi
