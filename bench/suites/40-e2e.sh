#!/usr/bin/env bash
# bench/suites/40-e2e.sh — Layer C: complete uploads, against competitors.
#
# This is the table people actually care about, and the one easiest to get
# wrong. Two scenarios, because "which poster is faster" has two different
# honest answers depending on what you are comparing:
#
#   post-only     No PAR2 anywhere. pesto vs nyuu vs ngPost, same article
#                 size, same line length, same connection count, same group,
#                 post checking off on all three. A pure poster comparison.
#
#   full-release  What an uploader actually runs: data plus a 10% recovery
#                 set. Here the tools have genuinely different shapes and the
#                 comparison has to respect that:
#                   * pesto (default) overlaps PAR2 generation with the
#                     upload — recovery articles start going out while the
#                     encoder is still working.
#                   * parpar + nyuu is two-phase: generate everything, then
#                     post. Both phases are timed together, because that is
#                     the wall time the operator waits.
#                   * ngPost is also two-phase (par2 before post).
#                   * pesto --par2-before-upload is included as the
#                     like-for-like two-phase row, so the overlap advantage
#                     can be separated from raw throughput.
#
# Latency sweep: the mock server can delay every response. At 0 ms the
# measurement is encode-and-write throughput. At 30 ms — an ordinary
# transatlantic RTT — it becomes a measurement of pipelining and connection
# scheduling, where posters differ far more. Reporting only the 0 ms number
# would flatter whichever tool has the tightest local loop and say nothing
# about real uploads.

set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

SUITE=e2e
E2E_LATENCIES="${BENCH_E2E_LATENCIES:-0,30}"

suite_e2e() {
    local wl
    for wl in "$@"; do
        local lat
        for lat in ${E2E_LATENCIES//,/ }; do
            e2e_workload "$wl" "$lat"
        done
    done
}

e2e_workload() {
    local wl=$1 latency=$2 corpus bytes
    workload_materialise "$wl" > /dev/null
    workload_clean "$wl"
    # The release folder, not the corpus container — see workload_root.
    corpus=$(workload_root "$wl")
    bytes=$(workload_bytes "$wl")
    workload_load "$wl"

    step "End-to-end — '$wl' ($(human_bytes "$bytes")), server latency ${latency} ms"

    local work="$BENCH_SANDBOX/e2e/$wl-$latency"
    rm -rf "$work"; mkdir -p "$work/nzb"

    mock_start --latency-ms "$latency"
    write_pesto_config "$work/pesto.toml" 127.0.0.1 "$MOCK_PORT" "$WL_CONNECTIONS"
    write_ngpost_config "$work/ngpost.conf" 127.0.0.1 "$MOCK_PORT" \
        "$WL_CONNECTIONS" "$WL_ARTICLE_SIZE" "$work/nzb"
    prime_cache "$corpus"

    info "post-only (no PAR2)"
    e2e_post_only "$wl" "$bytes" "$latency" "$work" "$corpus"

    echo
    info "full-release (${WL_PAR2_PCT}% PAR2)"
    e2e_full_release "$wl" "$bytes" "$latency" "$work" "$corpus"

    mock_stop
}

# ── scenario 1: posting only ─────────────────────────────────────────────────
#
# Matched settings, tool by tool:
#
#                     pesto              nyuu                  ngPost
#   article size      --article-size N   -a N                  -a N (config)
#   line length       --line-length 128  --article-line-size 128  fixed 128
#   connections       --connections N    -n N                  -n N / -t N
#   post check        --no-check         --check-tries 0       nzbCheck=false
#   group             --groups G         -g G                  GROUPS=G
#   NZB out           -o FILE            -o FILE -O            -o FILE
#
# nyuu defaults to 700 K articles and pesto to 768 000; both are overridden to
# the workload's value so the article count is identical. ngPost takes it from
# the generated config.
e2e_post_only() {
    local wl=$1 bytes=$2 latency=$3 work=$4 corpus=$5
    local extra="scenario=post-only;latency_ms=$latency;connections=$WL_CONNECTIONS"

    bench_case --suite "$SUITE" --workload "$wl" --tool pesto \
        --version "$("$PESTO_BIN" --version 2>/dev/null | head -1)" \
        --variant "post-only-l$latency" --input-bytes "$bytes" --extra "$extra" \
        --setup "rm -f '$work/pesto.nzb'" \
        --after "CASE_ARTICLES=\$(nzb_segment_count '$work/pesto.nzb')" \
        -- "$PESTO_BIN" --config "$work/pesto.toml" \
           --par2 0 --no-check --obfuscate=none \
           --article-size "$WL_ARTICLE_SIZE" --line-length 128 \
           --connections "$WL_CONNECTIONS" \
           --no-hooks --no-history --no-notify --no-session-log \
           --output-format json -o "$work/pesto.nzb" "$corpus"

    if ! skip_missing nyuu; then
        bench_case --suite "$SUITE" --workload "$wl" --tool nyuu \
            --version "$(tool_version nyuu)" \
            --variant "post-only-l$latency" --input-bytes "$bytes" --extra "$extra" \
            --setup "rm -f '$work/nyuu.nzb'" \
            --after "CASE_ARTICLES=\$(nzb_segment_count '$work/nyuu.nzb')" \
            -- nyuu -h 127.0.0.1 -P "$MOCK_PORT" -u bench -p bench \
               -n "$WL_CONNECTIONS" -a "$WL_ARTICLE_SIZE" --article-line-size 128 \
               --check-tries 0 -g alt.binaries.test -f 'bench <bench@localhost>' \
               -r keep -o "$work/nyuu.nzb" -O -q "$corpus"
    fi

    if ngpost_usable "$wl"; then
        bench_case --suite "$SUITE" --workload "$wl" --tool ngPost \
            --version "$(tool_version ngPost)" \
            --variant "post-only-l$latency" --input-bytes "$bytes" --extra "$extra" \
            --setup "rm -f '$work/nzb'/*.nzb" \
            --after "CASE_ARTICLES=\$(ngpost_segments '$work/nzb')" \
            -- ngPost -c "$work/ngpost.conf" -q -i "$corpus"
    fi
}

# ngPost only walks subdirectories when it is also compressing, so on a nested
# release it would post a subset of the files and produce a throughput number
# that is not comparable with the others. Reported as a capability gap rather
# than run and quietly under-counted.
ngpost_usable() {
    skip_missing ngPost && return 1
    # --compress makes ngPost walk subdirectories, so a nested workload that
    # asks for compression is fine; only an uncompressed nested one is not.
    [[ -n ${WL_COMPRESS:-} ]] && return 0
    if workload_is_nested "$1"; then
        printf "  %-28s %s\n" "ngPost" \
            "$(dim '(cannot recurse without --compress — skipped)')"
        return 1
    fi
    return 0
}

# ── scenario 2: the full release ─────────────────────────────────────────────
e2e_full_release() {
    local wl=$1 bytes=$2 latency=$3 work=$4 corpus=$5
    local extra="scenario=full-release;latency_ms=$latency;par2_pct=$WL_PAR2_PCT;connections=$WL_CONNECTIONS"

    # A workload that asks for compression gets it, because for the scene-style
    # release the archiving step is part of the upload, not a preliminary. Only
    # pesto and ngPost can do it themselves; nyuu has no archiving stage at
    # all, so on those workloads it appears in the post-only scenario only.
    local pesto_compress=()
    local ngpost_compress=()
    if [[ -n $WL_COMPRESS ]] && tool_present "$WL_COMPRESS"; then
        pesto_compress=(--compress="$WL_COMPRESS" --compress-temp-dir "$work/ctmp")
        ngpost_compress=(--compress)
        extra="$extra;compress=$WL_COMPRESS"
        if [[ -n $WL_COMPRESS_VOLUME ]]; then
            pesto_compress+=(--compress-volume-size "$WL_COMPRESS_VOLUME")
            # ngPost takes volume size in MB; the workload states it as e.g.
            # "500m", so strip the unit.
            ngpost_compress+=(--rar_size "${WL_COMPRESS_VOLUME%[mM]}")
            extra="$extra;volume=$WL_COMPRESS_VOLUME"
        fi
        mkdir -p "$work/ctmp"
    elif [[ -n $WL_COMPRESS ]]; then
        warn "workload '$wl' wants --compress=$WL_COMPRESS but $WL_COMPRESS is not installed"
    fi

    # pesto, default pipeline: PAR2 overlapped with the upload.
    bench_case --suite "$SUITE" --workload "$wl" --tool pesto \
        --version "$("$PESTO_BIN" --version 2>/dev/null | head -1)" \
        --variant "full-streaming-l$latency" --input-bytes "$bytes" --extra "$extra" \
        --setup "rm -f '$work/pesto-full.nzb'; find '$corpus' -name '*.par2' -delete" \
        --after "CASE_ARTICLES=\$(nzb_segment_count '$work/pesto-full.nzb')" \
        -- "$PESTO_BIN" --config "$work/pesto.toml" \
           --par2 "$WL_PAR2_PCT" --no-check --obfuscate="$WL_OBFUSCATE" \
           "${pesto_compress[@]}" \
           --article-size "$WL_ARTICLE_SIZE" --line-length 128 \
           --connections "$WL_CONNECTIONS" \
           --no-hooks --no-history --no-notify --no-session-log \
           --output-format json -o "$work/pesto-full.nzb" "$corpus"

    # pesto, two-phase: the same shape as parpar+nyuu and ngPost. The gap
    # between this row and the one above is the value of the overlap, measured
    # rather than asserted.
    bench_case --suite "$SUITE" --workload "$wl" --tool pesto \
        --version "$("$PESTO_BIN" --version 2>/dev/null | head -1)" \
        --variant "full-two-phase-l$latency" --input-bytes "$bytes" \
        --extra "$extra;mode=par2-before-upload" \
        --setup "rm -f '$work/pesto-2p.nzb'; find '$corpus' -name '*.par2' -delete" \
        --after "CASE_ARTICLES=\$(nzb_segment_count '$work/pesto-2p.nzb')" \
        -- "$PESTO_BIN" --config "$work/pesto.toml" \
           --par2 "$WL_PAR2_PCT" --no-check --par2-before-upload \
           --obfuscate="$WL_OBFUSCATE" "${pesto_compress[@]}" \
           --article-size "$WL_ARTICLE_SIZE" --line-length 128 \
           --connections "$WL_CONNECTIONS" \
           --no-hooks --no-history --no-notify --no-session-log \
           --output-format json -o "$work/pesto-2p.nzb" "$corpus"

    # parpar + nyuu, timed as one operation — that is the wall time an
    # operator using this combination actually waits.
    if tool_present parpar && tool_present nyuu; then
        par2_geometry "$corpus" "${BENCH_PAR2_SLICES:-2000}" "$WL_PAR2_PCT"
        local script="$work/parpar-nyuu.sh"
        write_parpar_nyuu_script "$script" "$work" "$corpus" "$MOCK_PORT"
        bench_case --suite "$SUITE" --workload "$wl" --tool "parpar+nyuu" \
            --version "$(tool_version parpar) / $(tool_version nyuu)" \
            --variant "full-two-phase-l$latency" --input-bytes "$bytes" \
            --extra "$extra;mode=two-phase" \
            --setup "rm -rf '$work/pp'; mkdir -p '$work/pp'; rm -f '$work/parpar-nyuu.nzb'" \
            --after "CASE_ARTICLES=\$(nzb_segment_count '$work/parpar-nyuu.nzb')" \
            -- bash "$script"
    else
        printf "  %-28s %s\n" "parpar+nyuu" "$(dim '(needs both parpar and nyuu — skipped)')"
    fi

    if ngpost_usable "$wl"; then
        bench_case --suite "$SUITE" --workload "$wl" --tool ngPost \
            --version "$(tool_version ngPost)" \
            --variant "full-two-phase-l$latency" --input-bytes "$bytes" \
            --extra "$extra;mode=two-phase" \
            --setup "rm -f '$work/nzb'/*.nzb; rm -rf '$work/ngtmp'; mkdir -p '$work/ngtmp'" \
            --after "CASE_ARTICLES=\$(ngpost_segments '$work/nzb')" \
            -- ngPost -c "$work/ngpost.conf" -q \
               --par2_pct "$WL_PAR2_PCT" --tmp_dir "$work/ngtmp" \
               "${ngpost_compress[@]}" -i "$corpus"
    fi
}

# The parpar+nyuu pipeline as a script, so /usr/bin/time measures both phases
# as a single process tree and the row is directly comparable with the
# single-binary rows.
write_parpar_nyuu_script() {
    local path=$1 work=$2 corpus=$3 port=$4
    cat > "$path" <<EOF
#!/usr/bin/env bash
set -euo pipefail
# Same input set every other row in this table sees: source files only,
# never a recovery set another run happened to leave behind.
mapfile -t inputs < <(find "$corpus" -type f ! -name '.manifest.jsonl' \
    ! -name '*.par2' | sort)

# Phase 1 — recovery generation, same geometry as every other PAR2 row here.
parpar -s "${P2_SLICE_SIZE}B" -r "$P2_RECOVERY_COUNT" \\
    -t "$BENCH_PAR2_THREADS" -m "${BENCH_PAR2_MEMORY}M" -f basename \\
    -o "$work/pp/$(basename "$corpus").par2" -O -q -- "\${inputs[@]}"

# Phase 2 — post the data files and the recovery set together.
nyuu -h 127.0.0.1 -P "$port" -u bench -p bench \\
    -n "$WL_CONNECTIONS" -a "$WL_ARTICLE_SIZE" --article-line-size 128 \\
    --check-tries 0 -g alt.binaries.test -f 'bench <bench@localhost>' \\
    -r keep -o "$work/parpar-nyuu.nzb" -O -q "\${inputs[@]}" "$work/pp"/*.par2
EOF
    chmod +x "$path"
}

# ngPost names its own NZB (appending _1, _2 on collision), so the article
# count is read from whatever it just wrote rather than from a known path.
ngpost_segments() {
    local dir=$1 newest
    newest=$(ls -t "$dir"/*.nzb 2>/dev/null | head -1) || true
    [[ -n ${newest:-} ]] && nzb_segment_count "$newest" || echo 0
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    bench_standalone_init "$SUITE"
    suite_e2e "${@:-many-small}"
fi
