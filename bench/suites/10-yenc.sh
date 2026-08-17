#!/usr/bin/env bash
# bench/suites/10-yenc.sh — Layer A: the yEnc kernel, and nothing else.
#
# No disk, no network, no article assembly: data is generated in memory and
# handed straight to the encoder. Whatever this reports is attributable to the
# SIMD kernel alone, which is what makes it the right place to catch a
# regression in `crates/pesto/src/yenc/` — and the wrong place to draw any
# conclusion about how fast pesto uploads.
#
# Every available SIMD path is measured on the same machine in the same run,
# so the dispatch decision itself ("is `auto` picking the fastest one?") is
# visible rather than assumed.
#
# Comparison: node-yencode, the C++ addon nyuu uses. It is the only other
# implementation in this class, and both are driven by their own internal
# timers after a warmup, so process startup is excluded on both sides.

set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

SUITE=yenc
SIZES="${BENCH_YENC_SIZES:-4096,131072,768000,8388608}"
LINE_LENS="${BENCH_YENC_LINE_LENS:-128,256}"
MIN_TIME="${BENCH_YENC_MIN_TIME:-1.0}"

suite_yenc() {
    step "yEnc encode/decode microbenchmark"
    info "sizes=${SIZES} line_lens=${LINE_LENS} min_time=${MIN_TIME}s"
    info "$(dim 'in-memory only — no file I/O, no articles, no network')"
    echo

    local json line op path ll size mibps ns

    # The driver runs its own warmup-and-repeat loop internally, so it is
    # invoked once per repetition of the whole sweep rather than measured with
    # bench_case's process-level timer (which would add ~2 ms of startup to a
    # measurement whose smallest case takes microseconds).
    local rep
    for (( rep = 1; rep <= BENCH_REPS; rep++ )); do
        json=$("$YENC_BENCH_BIN" --json \
            --sizes "$SIZES" --line-lens "$LINE_LENS" \
            --min-time "$MIN_TIME" --decode) || die "yenc-bench failed"

        while IFS= read -r line; do
            [[ -n $line ]] || continue
            op=$(json_str "$line" op)
            path=$(json_str "$line" simd_path)
            ll=$(json_num "$line" line_len)
            size=$(json_num "$line" size)
            mibps=$(json_num "$line" mibps)
            ns=$(json_num "$line" ns_per_iter)

            # wall_ms holds the per-iteration time so the standard aggregation
            # (median over reps, rsd) applies unchanged; the derived MiB/s in
            # summary.csv is recomputed from input_bytes and that time.
            #
            # The measured rate deliberately does NOT go into `extra`: that
            # column is part of the grouping key, so a value that changes
            # between repetitions would put every repetition in its own group
            # and there would be nothing left to take a median over.
            record_row suite="$SUITE" workload="micro" tool="pesto" \
                tool_version="$("$PESTO_BIN" --version 2>/dev/null | head -1)" \
                variant="$op/$path/ll$ll/${size}B" rep="$rep" \
                input_bytes="$size" \
                wall_ms="$(awk -v n="$ns" 'BEGIN{printf "%.6f", n / 1000000}')" \
                user_ms=0 sys_ms=0 max_rss_kb=0 exit_code=0 \
                articles=0 output_bytes=0 \
                extra="op=$op;simd=$path;line_len=$ll"

            # An `if`, not `(( rep == 1 )) && printf`: when that AND-list is
            # the last statement in a loop body it becomes the enclosing
            # function's exit status, so every repetition after the first
            # would return 1 and `set -e` would abort the whole run.
            if (( rep == 1 )); then
                printf "  %-8s %-8s ll=%-4s %9s B  %8s MiB/s\n" \
                    "$op" "$path" "$ll" "$size" "$mibps"
            fi
        done <<< "$json"
    done

    suite_yenc_node
}

# node-yencode, driven by bench/yencode.js — the same shape of harness
# (warmup, then N iterations under an internal timer) so the two numbers mean
# the same thing.
suite_yenc_node() {
    echo
    if ! tool_present node-yencode; then
        info "$(dim 'node-yencode not installed — comparison skipped')"
        info "$(dim '  install with: npm install yencode')"
        return 0
    fi

    local size ll mibps rep
    for size in ${SIZES//,/ }; do
        for ll in ${LINE_LENS//,/ }; do
            for (( rep = 1; rep <= BENCH_REPS; rep++ )); do
                mibps=$(node "$BENCH_DIR/yencode.js" --size "$size" --line-len "$ll" \
                        --min-time "$MIN_TIME" 2>/dev/null) || continue
                local ns
                ns=$(awk -v m="$mibps" -v s="$size" 'BEGIN{
                    if (m <= 0) { print 0; exit }
                    printf "%.6f", (s / 1048576) / m * 1000
                }')
                record_row suite="$SUITE" workload="micro" tool="node-yencode" \
                    tool_version="$(tool_version node)" \
                    variant="encode/simd/ll$ll/${size}B" rep="$rep" \
                    input_bytes="$size" wall_ms="$ns" \
                    user_ms=0 sys_ms=0 max_rss_kb=0 exit_code=0 \
                    articles=0 output_bytes=0 \
                    extra="op=encode;simd=node;line_len=$ll"
                if (( rep == 1 )); then
                    printf "  %-8s %-8s ll=%-4s %9s B  %8s MiB/s\n" \
                        "encode" "node" "$ll" "$size" "$mibps"
                fi
            done
        done
    done
    return 0
}

# Minimal JSON field readers — the driver emits flat, one-line objects with no
# nesting and no escapes, so a dependency on jq would buy nothing.
json_str() { sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p" <<< "$1"; }
json_num() { sed -n "s/.*\"$2\":\([-0-9.]*\).*/\1/p" <<< "$1"; }

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    bench_standalone_init "$SUITE"
    suite_yenc
fi
