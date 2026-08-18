#!/usr/bin/env bash
# bench/run.sh — the benchmark suite's single entry point.
#
#   ./bench/run.sh                      quick + standard tiers, all suites
#   ./bench/run.sh micro                yEnc and PAR2 microbenchmarks only
#   ./bench/run.sh e2e --workload movie-1080p
#   ./bench/run.sh --scale 0.1          shrink every corpus to a tenth
#   ./bench/run.sh --report-only DIR    regenerate a report from an old run
#
# Everything is written under bench/results/<host>/<UTC timestamp>/, and
# nothing outside that directory and bench/data/ is touched.

set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$BENCH_DIR/lib.sh"

SUITES_ALL=(yenc par2 stages e2e scaling correctness heterogeneous)
SUITES_MICRO=(yenc par2)
SUITES_REQUESTED=()
WORKLOADS_REQUESTED=()
TIERS="quick standard"
ASSUME_YES=0
VERIFY_DATA=0
REPORT_ONLY=""

usage() {
    cat <<'EOF'
usage: bench/run.sh [suite…] [options]

Suites (default: all)
  yenc          yEnc encoder/decoder microbenchmark (in-memory, no I/O)
  par2          PAR2 create/verify/repair vs parpar and par2cmdline
  stages        pipeline stage isolation and cost attribution
  e2e           full uploads vs nyuu, ngPost and parpar+nyuu
  scaling       connection-count and thread-count curves
  correctness   cross-tool PAR2 interop and a wire-level yEnc round-trip
  heterogeneous two mock servers; one can be slow (round-robin vs degraded peer)
  micro         shorthand for: yenc par2
  all           every suite

Options
  --workload NAME     run only this workload (repeatable)
  --tier LIST         corpus tiers to include: quick, standard, heavy
                      (default: "quick standard"; heavy is opt-in, ~40 GiB)
  --scale F           multiply every corpus size by F (default 1.0)
  --reps N            repetitions per case (default 3)
  --warmup N          unrecorded warmup repetitions (default 0)
  --drop-caches       drop the page cache before each repetition (needs sudo)
  --simd-sweep        also measure every parmesan SIMD path individually
  --latencies LIST    mock-server latencies in ms for e2e (default "0,30")
  --verify-data       re-checksum the corpus against its manifest and exit
  --report-only DIR   regenerate report.md from an existing run directory
  --list              list workloads and detected tools, then exit
  --yes               do not prompt before generating a large corpus
  -h, --help          this text

Environment overrides: BENCH_REPS, BENCH_SCALE, BENCH_PAR2_MEMORY,
BENCH_PAR2_THREADS, BENCH_PAR2_SLICES, BENCH_E2E_LATENCIES.
EOF
}

parse_cli() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            yenc|par2|stages|e2e|scaling|correctness|heterogeneous)
                SUITES_REQUESTED+=("$1"); shift ;;
            micro) SUITES_REQUESTED+=("${SUITES_MICRO[@]}"); shift ;;
            all)   SUITES_REQUESTED+=("${SUITES_ALL[@]}"); shift ;;
            --workload)   WORKLOADS_REQUESTED+=("$2"); shift 2 ;;
            --tier)       TIERS=$2; shift 2 ;;
            --scale)      export BENCH_SCALE=$2; shift 2 ;;
            --reps)       export BENCH_REPS=$2; shift 2 ;;
            --warmup)     export BENCH_WARMUP=$2; shift 2 ;;
            --drop-caches) export BENCH_DROP_CACHES=1; shift ;;
            --simd-sweep) export BENCH_SIMD_SWEEP=1; shift ;;
            --latencies)  export BENCH_E2E_LATENCIES=$2; shift 2 ;;
            --verify-data) VERIFY_DATA=1; shift ;;
            --report-only) REPORT_ONLY=$2; shift 2 ;;
            --list)       LIST_ONLY=1; shift ;;
            --yes|-y)     ASSUME_YES=1; shift ;;
            -h|--help)    usage; exit 0 ;;
            *) die "unknown argument '$1' (try --help)" ;;
        esac
    done
    [[ ${#SUITES_REQUESTED[@]} -gt 0 ]] || SUITES_REQUESTED=("${SUITES_ALL[@]}")
}

# Workloads selected by tier, unless named explicitly.
select_workloads() {
    if [[ ${#WORKLOADS_REQUESTED[@]} -gt 0 ]]; then
        printf '%s\n' "${WORKLOADS_REQUESTED[@]}"
        return
    fi
    local wl
    while read -r wl; do
        workload_load "$wl"
        # An `if`, not `[[ … ]] && echo`: a trailing AND-list becomes the
        # loop's — and then the function's — exit status, so a run whose last
        # workload does not match the tier would return 1. Harmless where this
        # is called today, and a `set -e` abort the moment someone assigns it
        # directly. The same shape already cost two debugging rounds elsewhere
        # in this suite.
        if [[ " $TIERS " == *" $WL_TIER "* ]]; then
            echo "$wl"
        fi
    done < <(workload_list)
}

# Total corpus size, before anything is written. Generating 40 GiB because a
# flag was mistyped is exactly the kind of surprise a benchmark should not
# spring on someone.
estimate_corpus_bytes() {
    local total=0 wl entry size count
    for wl in "$@"; do
        workload_load "$wl"
        for entry in "${WL_FILES[@]}"; do
            IFS='|' read -r _ size _ <<< "$entry"
            total=$(( total + $(scaled_size "$size") ))
        done
        for entry in "${WL_GENSETS[@]}"; do
            IFS='|' read -r _ _ count size _ <<< "$entry"
            total=$(( total + count * $(scaled_size "$size") ))
        done
    done
    echo "$total"
}

confirm_corpus() {
    local need=$1 have
    have=$(df -PB1 "$BENCH_DATA_DIR" 2>/dev/null | awk 'NR==2 {print $4}') || true
    have=${have:-0}

    printf "  corpus needed : %s\n" "$(human_bytes "$need")"
    printf "  free on disk  : %s\n" "$(human_bytes "$have")"

    if (( have > 0 && need > have )); then
        die "not enough free space for the corpus — lower --scale or pick fewer workloads"
    fi
    (( ASSUME_YES == 1 )) && return 0
    [[ -t 0 ]] || return 0            # non-interactive (CI): proceed
    (( need < 5 * 1024 * 1024 * 1024 )) && return 0

    local answer
    read -r -p "  generate corpus and continue? [y/N] " answer
    [[ ${answer,,} == y* ]] || die "aborted"
}

main() {
    parse_cli "$@"
    ensure_built
    detect_tools

    if [[ -n $REPORT_ONLY ]]; then
        local path; path=$(report_build "$REPORT_ONLY")
        info "report regenerated: $path"
        exit 0
    fi

    mapfile -t workloads < <(select_workloads)

    if [[ ${LIST_ONLY:-0} == 1 ]]; then
        step "workloads"
        local wl
        for wl in $(workload_list); do
            workload_load "$wl"
            printf "  %-16s %-9s %s\n" "$WL_NAME" "[$WL_TIER]" "$WL_DESC"
        done
        step "tools"
        print_tool_table
        exit 0
    fi

    if [[ $VERIFY_DATA == 1 ]]; then
        local rc=0 wl
        for wl in "${workloads[@]}"; do workload_verify "$wl" || rc=1; done
        exit $rc
    fi

    BENCH_RUN_DIR="$BENCH_RESULTS_DIR/$(hostname 2>/dev/null || uname -n)/$(date -u '+%Y%m%dT%H%M%SZ')"
    mkdir -p "$BENCH_RUN_DIR"
    bench_sandbox_init "$BENCH_RUN_DIR/sandbox"
    record_init

    echo
    bold "pesto benchmark suite"; echo
    hr
    print_system_info
    echo
    print_tool_table
    echo
    info "suites    : ${SUITES_REQUESTED[*]}"
    info "workloads : ${workloads[*]:-none}"
    info "scale     : ${BENCH_SCALE} · reps: ${BENCH_REPS} · results: $BENCH_RUN_DIR"
    hr

    write_system_json "$BENCH_RUN_DIR/system.json"

    if [[ ${#workloads[@]} -gt 0 ]] && suites_need_corpus; then
        confirm_corpus "$(estimate_corpus_bytes "${workloads[@]}")"
        step "materialising corpora"
        local wl
        for wl in "${workloads[@]}"; do
            workload_materialise "$wl" > /dev/null
            info "$(describe_workload "$wl")"
        done
    fi

    run_suites "${workloads[@]}"

    step "aggregating"
    summarise "$BENCH_RAW_CSV" "$BENCH_RUN_DIR/summary.csv"
    summary_to_json "$BENCH_RUN_DIR/summary.csv" "$BENCH_RUN_DIR/system.json" \
        "$BENCH_RUN_DIR/results.json"
    report_build "$BENCH_RUN_DIR" > /dev/null

    # A stable path for "the last run", which is what scripts and humans both
    # reach for. Relative, so the results tree stays movable.
    ln -sfn "$(basename "$BENCH_RUN_DIR")" "$(dirname "$BENCH_RUN_DIR")/latest"

    echo
    hr
    info "$(green 'done')"
    info "report : $BENCH_RUN_DIR/report.md"
    info "raw    : $BENCH_RUN_DIR/raw.csv"
    info "json   : $BENCH_RUN_DIR/results.json"
    echo
}

# Which suites need the workload corpora materialised.
#
# `yenc` generates its data in memory and `correctness` builds its own small
# flat corpus, so a run of either alone must not spend minutes — and tens of
# gigabytes — writing corpora it will never open. Getting this wrong is not
# subtle: `./bench/run.sh correctness` used to materialise the entire standard
# tier first.
suites_need_corpus() {
    local s
    for s in "${SUITES_REQUESTED[@]}"; do
        case "$s" in
            yenc|correctness) ;;
            *) return 0 ;;
        esac
    done
    return 1
}

run_suites() {
    local workloads=("$@") suite
    for suite in "${SUITES_REQUESTED[@]}"; do
        case "$suite" in
            yenc)
                source "$BENCH_DIR/suites/10-yenc.sh"
                suite_yenc ;;
            par2)
                source "$BENCH_DIR/suites/20-par2.sh"
                suite_par2 "${workloads[@]}" ;;
            stages)
                source "$BENCH_DIR/suites/30-stages.sh"
                suite_stages $(filter_layer stages "${workloads[@]}") ;;
            e2e)
                source "$BENCH_DIR/suites/40-e2e.sh"
                suite_e2e $(filter_layer e2e "${workloads[@]}") ;;
            scaling)
                source "$BENCH_DIR/suites/50-scaling.sh"
                # One workload only: this suite sweeps six connection counts at
                # three latencies, so a second workload multiplies an already
                # long run for very little extra information.
                local scaling_wl
                scaling_wl=$(filter_layer scaling "${workloads[@]}" | head -1)
                if [[ -z $scaling_wl ]]; then
                    info "$(dim 'no selected workload opts into the scaling layer — skipped')"
                else
                    suite_scaling "$scaling_wl"
                fi ;;
            correctness)
                source "$BENCH_DIR/suites/60-correctness.sh"
                # Takes no workload: it builds its own small flat corpus, for
                # the reasons given at the top of that file.
                suite_correctness || true ;;
            heterogeneous)
                source "$BENCH_DIR/suites/70-heterogeneous.sh"
                local hetero_wl
                hetero_wl=$(filter_layer e2e "${workloads[@]}" | head -1)
                if [[ -z $hetero_wl ]]; then
                    info "$(dim 'no selected workload opts into e2e — heterogeneous skipped')"
                else
                    suite_heterogeneous "$hetero_wl"
                fi ;;
        esac
    done
}

# A workload opts into layers via WL_LAYERS — a 40 GiB remux is worth an
# end-to-end row but not a connection-scaling sweep at six connection counts.
filter_layer() {
    local layer=$1; shift
    local wl
    for wl in "$@"; do
        workload_load "$wl"
        if [[ " $WL_LAYERS " == *" $layer "* ]]; then
            echo "$wl"
        fi
    done
}

main "$@"
