#!/usr/bin/env bash
# bench/lib/measure.sh — how a single measurement is taken.
#
# One place decides what "a result" means for the whole suite, so every tool
# in every table was measured the same way:
#
#   wall_ms   nanosecond clock around the process. GNU time's own %e is only
#             10 ms granular, which is coarse enough to matter for the micro
#             cases, so wall time is taken here and only CPU/RSS come from it.
#   user/sys  CPU seconds. (user+sys)/wall is the parallel-efficiency figure
#             reported as cpu_ratio — the number that shows whether a tool is
#             actually using the cores it was given.
#   max_rss   peak resident set. PAR2 encoders trade memory for passes over
#             the input, so a throughput number without RSS next to it can be
#             bought simply by using more RAM.
#
# Repetitions are the unit of trust: each rep is recorded as its own row, and
# the summary reports the median plus a relative-standard-deviation column, so
# a noisy machine is visible in the output instead of being averaged away.

BENCH_REPS="${BENCH_REPS:-3}"
BENCH_WARMUP="${BENCH_WARMUP:-0}"
BENCH_DROP_CACHES="${BENCH_DROP_CACHES:-0}"

# Detected once; `time` the shell builtin cannot report RSS, so GNU time is
# strongly preferred but not required.
if /usr/bin/time -f '%e' true >/dev/null 2>&1; then
    BENCH_GNU_TIME=/usr/bin/time
elif command -v gtime >/dev/null 2>&1 && gtime -f '%e' true >/dev/null 2>&1; then
    BENCH_GNU_TIME=gtime
else
    BENCH_GNU_TIME=""
fi

# drop_caches — best effort, opt in via BENCH_DROP_CACHES=1.
#
# Deliberately off by default. It needs passwordless sudo, which most people
# running a benchmark do not have, and a run that silently succeeds on one
# machine and silently fails on another is worse than one that consistently
# measures warm-cache throughput. Whichever mode was used is recorded in
# system.json.
drop_caches() {
    sync
    [[ ${BENCH_DROP_CACHES} == 1 ]] || return 0
    echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null 2>&1 ||
        warn "drop_caches requested but sudo denied; continuing warm-cache"
}

# prime_cache <path>… — read files once so every tool in a comparison starts
# from the same page-cache state. The alternative is that whichever tool runs
# first pays for the cold read and looks slower for reasons that have nothing
# to do with it.
prime_cache() {
    local p
    for p in "$@"; do
        [[ -e $p ]] || continue
        if [[ -d $p ]]; then
            find "$p" -type f -exec cat {} + > /dev/null 2>&1 || true
        else
            cat "$p" > /dev/null 2>&1 || true
        fi
    done
}

# measure_once <cmd…> — run once, set M_* globals. Never aborts the suite on
# a non-zero exit; the exit code is recorded so a failed competitor shows up
# as a failure row rather than killing the run.
#
# stdout/stderr of the measured command go to $M_LOG (default: discarded).
measure_once() {
    local rc=0 t0 t1
    local timefile
    timefile=$(mktemp "${TMPDIR:-/tmp}/bench-time.XXXXXX")
    local log="${M_LOG:-/dev/null}"

    t0=$(date +%s%N)
    if [[ -n $BENCH_GNU_TIME ]]; then
        "$BENCH_GNU_TIME" -f '%U %S %M' -o "$timefile" "$@" >"$log" 2>&1 || rc=$?
    else
        "$@" >"$log" 2>&1 || rc=$?
        echo "0 0 0" > "$timefile"
    fi
    t1=$(date +%s%N)

    M_WALL_MS=$(( (t1 - t0) / 1000000 ))
    M_EXIT=$rc
    # GNU time appends its report; take the last line so a command that wrote
    # to the same file cannot corrupt the parse.
    read -r M_USER_S M_SYS_S M_RSS_KB <<< "$(tail -1 "$timefile")"
    M_USER_MS=$(awk -v s="${M_USER_S:-0}" 'BEGIN{printf "%d", s * 1000}')
    M_SYS_MS=$(awk -v s="${M_SYS_S:-0}" 'BEGIN{printf "%d", s * 1000}')
    M_RSS_KB=${M_RSS_KB:-0}
    rm -f "$timefile"
    return 0
}

# bench_case — run one benchmark case for BENCH_REPS repetitions and record
# a row per repetition.
#
#   bench_case --suite par2 --workload movie --tool parmesan \
#              --variant r10 --input-bytes 8589934592 \
#              --extra 'recovery_pct=10;slice_size=1048576' \
#              --setup 'rm -f "$OUT"/*.par2' \
#              --after 'CASE_OUTPUT_BYTES=$(dir_size_bytes "$OUT" "*.par2")' \
#              -- parmesan create ...
#
# --setup runs before every repetition (not timed) and is where output from
# the previous repetition gets cleared; --after runs after every repetition
# and may set CASE_OUTPUT_BYTES / CASE_ARTICLES, which land in the row.
bench_case() {
    local suite="" workload="" tool="" version="" variant="" extra=""
    local input_bytes=0 setup="" after="" reps="$BENCH_REPS" label=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --suite)       suite=$2; shift 2 ;;
            --workload)    workload=$2; shift 2 ;;
            --tool)        tool=$2; shift 2 ;;
            --version)     version=$2; shift 2 ;;
            --variant)     variant=$2; shift 2 ;;
            --extra)       extra=$2; shift 2 ;;
            --input-bytes) input_bytes=$2; shift 2 ;;
            --setup)       setup=$2; shift 2 ;;
            --after)       after=$2; shift 2 ;;
            --reps)        reps=$2; shift 2 ;;
            --label)       label=$2; shift 2 ;;
            --) shift; break ;;
            *) die "bench_case: unknown option $1" ;;
        esac
    done
    [[ $# -gt 0 ]] || die "bench_case: no command given"

    label=${label:-$tool${variant:+ ($variant)}}
    version=${version:-$(tool_version "$tool")}

    # Every case gets its own log. A benchmark that reports "FAILED (exit 1)"
    # and discards the tool's own message is a benchmark you cannot debug, and
    # the failure is usually a flag mismatch that needs fixing before the row
    # means anything.
    local logdir="$BENCH_RUN_DIR/logs"
    mkdir -p "$logdir"
    local slug="${suite}-${workload}-${tool}-${variant}"
    slug=${slug//[^A-Za-z0-9._+-]/_}
    M_LOG="$logdir/$slug.log"

    local rep wall_list=() failed=0 failing_exit=0
    for (( rep = 1 - BENCH_WARMUP; rep <= reps; rep++ )); do
        CASE_OUTPUT_BYTES=0
        CASE_ARTICLES=0
        [[ -n $setup ]] && eval "$setup"
        drop_caches
        measure_once "$@"
        [[ -n $after ]] && eval "$after"

        # Repetitions <= 0 are warmups: they populate caches and JIT state but
        # are deliberately not recorded.
        (( rep < 1 )) && continue

        if (( M_EXIT != 0 )); then
            failed=1
            failing_exit=$M_EXIT
        else
            wall_list+=("$M_WALL_MS")
        fi

        record_row \
            suite="$suite" workload="$workload" tool="$tool" \
            tool_version="$version" variant="$variant" rep="$rep" \
            input_bytes="$input_bytes" wall_ms="$M_WALL_MS" \
            user_ms="$M_USER_MS" sys_ms="$M_SYS_MS" max_rss_kb="$M_RSS_KB" \
            exit_code="$M_EXIT" articles="${CASE_ARTICLES:-0}" \
            output_bytes="${CASE_OUTPUT_BYTES:-0}" extra="$extra"
    done

    if (( failed )); then
        printf "  %-28s %s\n" "$label" "$(red "FAILED (exit $failing_exit)")"
        if [[ -s ${M_LOG:-} ]]; then
            printf '%s\n' "$(dim "$(tail -3 "$M_LOG" | sed 's/^/      /')")"
            printf '      %s\n' "$(dim "full log: $M_LOG")"
        fi
        return 0
    fi

    local median
    median=$(stats_median "${wall_list[@]}")
    local rate=""
    (( input_bytes > 0 )) && rate="$(throughput_mbps "$input_bytes" "$median") MiB/s"
    printf "  %-28s %8s  %-14s rss=%s\n" \
        "$label" "$(ms_to_str "$median")" "$rate" "$(human_bytes $(( M_RSS_KB * 1024 )))"
}
