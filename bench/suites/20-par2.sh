#!/usr/bin/env bash
# bench/suites/20-par2.sh — Layer A/B: PAR2 create, verify and repair.
#
# parmesan vs parpar vs par2cmdline, on identical geometry. See
# `lib/tools.sh:par2_geometry` for why the geometry is computed once here and
# pushed into all three rather than letting each tool derive its own: a target
# slice *count* is rounded differently by each implementation, and a 3%
# difference in slice size is a 3% difference in the amount of GF(2^16)
# arithmetic performed, which is exactly the quantity being measured.
#
# Two throughput figures are reported per row:
#
#   input MiB/s   source bytes per second. The number that bounds a real
#                 upload, and the one everybody quotes.
#   GF madd GiB/s input_bytes × recovery_count / time. The implementation-level
#                 rate, independent of the redundancy chosen. Two tools run at
#                 5% and 10% recovery are not comparable on input MiB/s but
#                 are directly comparable on this.
#
# parpar has no verify or repair mode, so it appears in the create table only.

set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

SUITE=par2
PAR2_PCTS="${BENCH_PAR2_PCTS:-10}"
PAR2_TARGET_SLICES="${BENCH_PAR2_SLICES:-2000}"
# Fraction of recovery blocks to destroy before the repair measurement. 50%
# of the available recovery is the interesting point: enough missing data that
# the decoder must actually invert a sizeable matrix, not so much that the
# repair is impossible.
PAR2_DAMAGE_FRAC="${BENCH_PAR2_DAMAGE_FRAC:-50}"

suite_par2() {
    local workloads=("$@")
    for wl in "${workloads[@]}"; do
        par2_workload "$wl"
    done
}

par2_workload() {
    local wl=$1 corpus bytes files pct
    corpus=$(workload_materialise "$wl")
    # Before measuring anything: a previous (or interrupted) run may have left
    # recovery sets in the corpus, and they would otherwise be counted as
    # input and change the geometry.
    workload_clean "$wl"
    bytes=$(workload_bytes "$wl")
    files=$(find "$corpus" -type f ! -name '.manifest.jsonl' ! -name '*.par2' | wc -l)

    step "PAR2 — workload '$wl' ($(human_bytes "$bytes"), $files file(s))"

    # Every tool reads the same paths; passing the file list explicitly (not a
    # directory) keeps recursion behaviour — which differs between the three —
    # out of the comparison.
    mapfile -t inputs < <(find "$corpus" -type f ! -name '.manifest.jsonl' \
        ! -name '*.par2' | sort)

    # The recovery set is written next to the data, exactly as an operator
    # would. Not tidiness: par2cmdline refuses sources outside its basepath,
    # and both parmesan and par2cmdline resolve a set's files relative to the
    # index when verifying and repairing. Put the .par2 files anywhere else
    # and every verify/repair row fails for a reason that has nothing to do
    # with the tools' performance.
    local out
    out=$(common_parent_dir "${inputs[@]}")

    for pct in ${PAR2_PCTS//,/ }; do
        par2_geometry "$corpus" "$PAR2_TARGET_SLICES" "$pct"
        info "geometry: slice=$(human_bytes "$P2_SLICE_SIZE") × $P2_SLICE_COUNT input, \
$P2_RECOVERY_COUNT recovery blocks, ${BENCH_PAR2_THREADS} threads, ${BENCH_PAR2_MEMORY} MiB"
        echo
        par2_create_row "$wl" "$bytes" "$pct" "$out" "${inputs[@]}"
    done

    par2_verify_repair "$wl" "$bytes" "$out" "${inputs[@]}"

    # Leave the shared corpus exactly as it was found: it is cached between
    # runs and reused by every other suite, which walks it as *input*.
    workload_clean "$wl"
}

par2_create_row() {
    local wl=$1 bytes=$2 pct=$3 out=$4; shift 4
    local geom; geom=$(par2_geometry_note)
    local clean="rm -f '$out'/*.par2"
    local collect="CASE_OUTPUT_BYTES=\$(dir_size_bytes '$out' '*.par2')"
    prime_cache "$@"

    par2_cmd_parmesan "$out" "$wl" "$@"
    bench_case --suite "$SUITE" --workload "$wl" --tool parmesan \
        --version "$("$PARMESAN_BIN" --version 2>/dev/null | head -1)" \
        --variant "create-r${pct}" --input-bytes "$bytes" \
        --extra "op=create;recovery_pct=$pct;$geom" \
        --setup "$clean" --after "$collect" -- "${P2_CMD[@]}"

    if ! skip_missing parpar; then
        par2_cmd_parpar "$out" "$wl" "$@"
        bench_case --suite "$SUITE" --workload "$wl" --tool parpar \
            --variant "create-r${pct}" --input-bytes "$bytes" \
            --extra "op=create;recovery_pct=$pct;$geom" \
            --setup "$clean" --after "$collect" -- "${P2_CMD[@]}"
    fi

    if ! skip_missing par2; then
        par2_cmd_par2cmdline "$out" "$wl" "$@"
        bench_case --suite "$SUITE" --workload "$wl" --tool par2cmdline \
            --version "$(tool_version par2)" \
            --variant "create-r${pct}" --input-bytes "$bytes" \
            --extra "op=create;recovery_pct=$pct;$geom" \
            --setup "$clean" --after "$collect" -- "${P2_CMD[@]}"
    fi

    # SIMD path sweep, parmesan only: neither competitor exposes the choice.
    # This is the row that says *why* a machine is fast, not just that it is,
    # and it is what a GFNI-vs-AVX2 claim has to rest on.
    if [[ ${BENCH_SIMD_SWEEP:-0} == 1 ]]; then
        local path
        for path in scalar ssse3 avx2 avx2-gfni avx512-gfni; do
            BENCH_SIMD=$path
            par2_cmd_parmesan "$out" "$wl" "$@"
            # A path the CPU does not support makes parmesan exit non-zero;
            # bench_case records that as a failure row and moves on, which is
            # the right outcome — an unsupported path is a fact about the
            # machine, not an error in the run.
            bench_case --suite "$SUITE" --workload "$wl" --tool parmesan \
                --variant "create-r${pct}-$path" --input-bytes "$bytes" \
                --extra "op=create;recovery_pct=$pct;simd=$path;$geom" \
                --setup "$clean" --after "$collect" -- "${P2_CMD[@]}"
        done
        BENCH_SIMD=auto
    fi
}

# Verify and repair share one prepared recovery set, built by parmesan and
# then re-verified by every tool. Cross-tool verification is a correctness
# result as much as a performance one; 60-correctness.sh asserts it, this
# suite times it.
par2_verify_repair() {
    local wl=$1 bytes=$2 out=$3; shift 3
    local index="$out/$wl.par2"

    # All three tools store bare file names in their File Description packets
    # (that is what makes their sets interchangeable), which means verify and
    # repair can only find the data when every protected file sits directly
    # beside the index. A nested corpus is a legitimate posting workload but
    # not a legitimate verify/repair comparison, so it is skipped explicitly
    # rather than reported as four tool failures.
    local f
    for f in "$@"; do
        if [[ $(dirname "$f") != "$out" ]]; then
            info "$(dim "verify/repair skipped for '$wl': corpus has subdirectories")"
            return 0
        fi
    done

    rm -f "$out"/*.par2
    par2_create_parmesan "$out" "$wl" "$@" >/dev/null 2>&1 || {
        warn "could not build a recovery set for '$wl'; skipping verify/repair"
        return 0
    }
    [[ -r $index ]] || { warn "no index at $index; skipping verify/repair"; return 0; }

    echo
    info "verify (intact corpus)"
    prime_cache "$@"

    bench_case --suite "$SUITE" --workload "$wl" --tool parmesan \
        --variant verify --input-bytes "$bytes" --extra "op=verify" \
        -- "$PARMESAN_BIN" verify -q "$index"

    if ! skip_missing par2; then
        bench_case --suite "$SUITE" --workload "$wl" --tool par2cmdline \
            --version "$(tool_version par2)" \
            --variant verify --input-bytes "$bytes" --extra "op=verify" \
            -- par2 verify -q -q -t"$BENCH_PAR2_THREADS" "$index"
    fi

    # Repair needs damaged input. Overwriting slice-aligned regions in place
    # (rather than deleting files) is the harder and more realistic case: the
    # decoder has to work out *which* slices are bad before it can rebuild
    # them, instead of being handed a list of obviously missing files.
    local damage_count
    damage_count=$(awk -v r="$P2_RECOVERY_COUNT" -v f="$PAR2_DAMAGE_FRAC" \
        'BEGIN { n = int(r * f / 100); print (n < 1 ? 1 : n) }')

    # The damage is spread across files rather than concentrated in the first
    # one. A corpus of 2 000 sub-slice files has exactly one slice per file, so
    # "damage 100 slices" has to mean 100 files; writing 100 slices' worth into
    # a single 12 KiB file would just extend it and turn the experiment into a
    # file-size mismatch instead of a decode.
    mapfile -t victims < <(damage_targets "$P2_SLICE_SIZE" "$damage_count" "$@")
    if [[ ${#victims[@]} -eq 0 ]]; then
        info "$(dim "repair skipped for '$wl': no file large enough to damage")"
        return 0
    fi

    local backup="$BENCH_SANDBOX/par2-backup"
    rm -rf "$backup"; mkdir -p "$backup"
    local v
    for v in "${victims[@]}"; do cp "$v" "$backup/$(basename "$v")"; done

    echo
    info "repair ($damage_count damaged slices of $P2_RECOVERY_COUNT recovery blocks, \
${#victims[@]} file(s) affected)"

    local prep="restore_from '$backup' \"\${victims[@]}\"; \
damage_slices_in '$P2_SLICE_SIZE' '$damage_count' \"\${victims[@]}\""

    bench_case --suite "$SUITE" --workload "$wl" --tool parmesan \
        --variant repair --input-bytes "$bytes" \
        --extra "op=repair;damaged_slices=$damage_count" \
        --setup "$prep" \
        -- "$PARMESAN_BIN" repair -q "$index"

    if ! skip_missing par2; then
        bench_case --suite "$SUITE" --workload "$wl" --tool par2cmdline \
            --version "$(tool_version par2)" \
            --variant repair --input-bytes "$bytes" \
            --extra "op=repair;damaged_slices=$damage_count" \
            --setup "$prep" \
            -- par2 repair -q -q -t"$BENCH_PAR2_THREADS" "$index"
    fi

    restore_from "$backup" "${victims[@]}"
    rm -rf "$backup"
}

# damage_targets <slice_size> <slices> <files…> — the files that between them
# hold `slices` whole slices, walked in order until the budget is spent.
damage_targets() {
    local slice=$1 budget=$2; shift 2
    local f size have
    for f in "$@"; do
        (( budget > 0 )) || break
        size=$(file_size_bytes "$f")
        have=$(( size / slice ))
        (( have < 1 )) && continue
        echo "$f"
        budget=$(( budget - have ))
    done
}

# damage_slices_in <slice_size> <slices> <files…> — zero slice-aligned regions
# in place, strictly within each file's existing length. Deterministic: the
# same regions are hit on every repetition and on every machine, so repair
# timings stay comparable across runs.
damage_slices_in() {
    local slice=$1 budget=$2; shift 2
    local f size have n i
    for f in "$@"; do
        (( budget > 0 )) || break
        size=$(file_size_bytes "$f")
        have=$(( size / slice ))
        (( have < 1 )) && continue
        n=$(( have < budget ? have : budget ))
        for (( i = 0; i < n; i++ )); do
            dd if=/dev/zero of="$f" bs="$slice" count=1 seek="$i" \
                conv=notrunc status=none 2>/dev/null || true
        done
        budget=$(( budget - n ))
    done
}

# restore_from <backup_dir> <files…> — put the originals back, and remove the
# numbered backups par2cmdline leaves behind.
#
# par2cmdline renames each damaged file to `<name>.1`, `<name>.2`, … before
# writing the repaired version, and keeps them. Left in place they accumulate
# in the shared corpus, where the *next* suite would walk them as input files:
# a repair benchmark would silently grow the corpus every time it ran.
# `par2 repair -p` would purge them, but it also deletes the recovery set,
# which the following repetition still needs.
restore_from() {
    local backup=$1; shift
    local f leftover
    for f in "$@"; do
        cp "$backup/$(basename "$f")" "$f"
        shopt -s nullglob
        for leftover in "$f".[0-9]*; do rm -f "$leftover"; done
        shopt -u nullglob
    done
}

# common_parent_dir <paths…> — the deepest directory containing all of them.
common_parent_dir() {
    local prefix
    prefix=$(dirname "$1"); shift
    local d
    for d in "$@"; do
        d=$(dirname "$d")
        while [[ $d != "$prefix" ]]; do
            [[ $d == "$prefix"/* ]] && break
            prefix=$(dirname "$prefix")
            [[ $prefix == / || $prefix == . ]] && break
        done
    done
    printf '%s' "$prefix"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    bench_standalone_init "$SUITE"
    suite_par2 "${@:-many-small}"
fi
