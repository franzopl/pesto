#!/usr/bin/env bash
# bench/lib/report.sh — turning summary.csv into something a human reads.
#
# The report is generated from the summary, which is generated from the raw
# repetitions. Nothing here re-measures anything, so a report can be
# regenerated (or re-shaped) long after the run without touching the machine
# that produced it: `bench/run.sh --report-only <run-dir>`.
#
# Two conventions the tables follow throughout:
#
#   * the headline number is the median of the repetitions, and every table
#     carries the relative standard deviation next to it. A 3% difference on a
#     row with 8% noise is not a difference, and the table should say so
#     rather than leaving the reader to assume.
#   * speedups are expressed against an explicit baseline named in the header,
#     never against "the other one", so a three-tool table is unambiguous.

# report_build <run_dir>
report_build() {
    local run=$1
    local summary="$run/summary.csv" system="$run/system.json" out="$run/report.md"
    [[ -r $summary ]] || { warn "no summary.csv in $run"; return 1; }

    {
        report_head "$system"
        report_suite_yenc "$summary"
        report_suite_par2 "$summary"
        report_suite_stages "$summary"
        report_suite_e2e "$summary"
        report_suite_scaling "$summary"
        report_footer "$run"
    } > "$out"
    echo "$out"
}

json_get() { sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$1" | head -1; }
json_get_num() { sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\([0-9.]*\).*/\1/p" "$1" | head -1; }

report_head() {
    local sys=$1
    cat <<EOF
# pesto benchmark results

**$(json_get "$sys" host)** — $(json_get "$sys" generated_utc)

| | |
|---|---|
| CPU | $(json_get "$sys" cpu_model) |
| Cores | $(json_get_num "$sys" cpu_cores_logical) logical / $(json_get_num "$sys" cpu_cores_physical) physical |
| SIMD | \`$(json_get "$sys" simd_flags)\` |
| Kernel | $(json_get "$sys" os) $(json_get "$sys" kernel) ($(json_get "$sys" arch)) |
| Governor | $(json_get "$sys" cpu_governor), boost $(json_get "$sys" cpu_boost) |
| Corpus FS | $(json_get "$sys" data_filesystem) |
| Repetitions | $(json_get_num "$sys" reps) (median reported) |
| Page cache | $([[ $(json_get_num "$sys" drop_caches) == 1 ]] && echo "dropped between runs" || echo "warm, primed before each comparison") |

Tool versions: pesto \`$(json_get "$sys" pesto)\`, parmesan \`$(json_get "$sys" parmesan)\`,
parpar \`$(json_get "$sys" parpar)\`, par2cmdline \`$(json_get "$sys" par2cmdline)\`,
nyuu \`$(json_get "$sys" nyuu)\`, ngPost \`$(json_get "$sys" ngPost)\`.

EOF
}

# has_suite <summary> <suite>
has_suite() { awk -F, -v s="$2" 'NR>1 && $1 == s { found = 1 } END { exit !found }' "$1"; }

report_suite_yenc() {
    local summary=$1
    has_suite "$summary" yenc || return 0
    cat <<'EOF'
## yEnc microbenchmark

Pure encoder throughput: data already in memory, no articles, no I/O. This is
the number to watch for a regression in the SIMD kernels, and *not* a number
that predicts upload speed — see the pipeline stages below for that.

EOF
    awk -F, '
        NR == 1 { next }
        $1 != "yenc" { next }
        {
            # variant is op/simd/llN/sizeB
            split($5, v, "/")
            op = v[1]; simd = v[2]; ll = v[3]; sz = v[4]
            key = op "|" ll "|" sz
            mibps = ($10 > 0) ? ($9 / 1048576) / ($10 / 1000) : 0
            cell[key, simd] = sprintf("%.0f", mibps)
            rsd[key, simd] = $13
            simds[simd] = 1
            keys[key] = 1
        }
        END {
            n = 0
            for (s in simds) order[++n] = s
            # Stable, meaningful column order rather than hash order.
            split("scalar ssse3 avx2 neon auto node", pref, " ")
            cols = 0
            for (i = 1; i <= 6; i++) if (pref[i] in simds) col[++cols] = pref[i]
            for (s in simds) {
                seen = 0
                for (i = 1; i <= cols; i++) if (col[i] == s) seen = 1
                if (!seen) col[++cols] = s
            }

            printf "| op | line | size |"
            for (i = 1; i <= cols; i++) printf " %s |", col[i]
            printf "\n|---|---|---|"
            for (i = 1; i <= cols; i++) printf "---|"
            printf "\n"

            for (k in keys) {
                split(k, p, "|")
                printf "| %s | %s | %s |", p[1], p[2], p[3]
                for (i = 1; i <= cols; i++) {
                    val = cell[k, col[i]]
                    printf " %s |", (val == "" ? "–" : val " MiB/s")
                }
                printf "\n"
            }
        }
    ' "$summary" | table_sort_keeping_header -t'|' -k2,2 -k3,3 -k4,4n
    echo
}

report_suite_par2() {
    local summary=$1
    has_suite "$summary" par2 || return 0
    cat <<'EOF'
## PAR2

All tools were given the *same* geometry — identical slice size, input slice
count and recovery block count — so each row represents the same amount of
GF(2^16) arithmetic. See `bench/lib/tools.sh` for the flag mapping. `GF madd`
is `input_bytes × recovery_blocks / time`, the redundancy-independent rate.

EOF
    report_comparison_table "$summary" par2 parmesan
}

report_suite_stages() {
    local summary=$1
    has_suite "$summary" stages || return 0
    cat <<'EOF'
## Pipeline stages

The same corpus run through pesto with progressively more of the pipeline
enabled. The first table is the raw measurements; the second attributes cost
to individual stages by differencing pairs of runs that differ by exactly one
thing.

EOF
    awk -F, '
        NR == 1 { next }
        $1 != "stages" { next }
        {
            wl[$2] = 1
            t[$2, $5] = $10
            mib[$2, $5] = $14
            rsd[$2, $5] = $13
            rss[$2, $5] = $16
        }
        function have(w, s) { return ((w SUBSEP s) in t) }
        function secs(w, s) { return t[w, s] / 1000 }
        # Difference between two stages, as a signed duration, or "–" when
        # either stage did not run for this workload.
        function delta(w, a, b) {
            if (!have(w, a) || !have(w, b)) return "–"
            return sprintf("%+.2fs", secs(w, a) - secs(w, b))
        }
        END {
            split("read yenc yenc+par2 par2-only compress post post+par2 post+par2-pre post+check", ord, " ")
            for (w in wl) {
                printf "\n### %s\n\n", w
                print "| stage | wall | MiB/s | peak RSS | noise |"
                print "|---|---|---|---|---|"
                for (i = 1; i <= 9; i++) {
                    s = ord[i]
                    if (!have(w, s)) continue
                    printf "| %s | %.2fs | %.0f | %.0f MiB | %.1f%% |\n",
                        s, secs(w, s), mib[w, s], rss[w, s] / 1024, rsd[w, s]
                }

                # The attribution: each line is one stage difference chosen so
                # that the two runs differ by exactly one thing. A raw
                # row-to-row delta chain would be meaningless here, because the
                # stages are not a single increasing sequence — `post` does
                # less work than `par2-only`, not more.
                printf "\n| cost of | measured as | value |\n|---|---|---|\n"
                printf "| storage read | read | %.2fs |\n", secs(w, "read")
                printf "| yEnc + articles + NZB | yenc − read | %s |\n", delta(w, "yenc", "read")
                printf "| PAR2 generation | yenc+par2 − yenc | %s |\n", delta(w, "yenc+par2", "yenc")
                printf "| NNTP framing + sockets | post − yenc | %s |\n", delta(w, "post", "yenc")
                printf "| streaming PAR2 overlap | post+par2 − post+par2-pre | %s |\n", delta(w, "post+par2", "post+par2-pre")
                printf "| streaming STAT check | post+check − post+par2 | %s |\n", delta(w, "post+check", "post+par2")
                if (have(w, "compress"))
                    printf "| archiving | compress − yenc | %s |\n", delta(w, "compress", "yenc")
            }
        }
    ' "$summary"
    cat <<'EOF'

A negative "streaming PAR2 overlap" is the headline result: it is how much
time the default pipeline saves by computing recovery data *while* the data
articles are already going out, versus `--par2-before-upload`, which generates
everything first — the shape parpar+nyuu and ngPost both have. On a workload
where PAR2 dominates, that difference is the entire argument for the design.

Every other line is a single-variable difference between two real runs, so
each is attributable to exactly one stage. They do not sum to the total: the
default pipeline overlaps stages on purpose, which is the point.

EOF
}

report_suite_e2e() {
    local summary=$1
    has_suite "$summary" e2e || return 0
    cat <<'EOF'
## End-to-end uploads

Posted to the local mock NNTP server, so no network variance and no account.
`latency` is the artificial per-response delay the mock adds: 0 ms measures
encode-and-write throughput, 30 ms measures pipelining under a realistic
round-trip time.

Cases:

- `post-only-*` — no PAR2 anywhere. A pure poster comparison at identical
  article size, line length, connection count and group.
- `full-streaming-*` — pesto's default pipeline, recovery generated while the
  data articles are already going out. No competitor has this shape, so the
  row has one cell by design.
- `full-two-phase-*` — generate everything, then post: `pesto
  --par2-before-upload` against `parpar + nyuu` (both phases timed as one) and
  `ngPost --gen_par2`. This is the like-for-like comparison.

Article counts are cross-checked from each tool's own NZB. On `post-only`
rows they must match, and a ⚠ means the row is not comparable. On
`full-*` rows a difference is expected: implementations split recovery data
into volumes differently, so the same recovery block count lands in a
different number of files and articles. There the flag is a reminder that the
rows carry the same *payload*, not the same article count.

EOF
    report_comparison_table "$summary" e2e pesto
}

report_suite_scaling() {
    local summary=$1
    has_suite "$summary" scaling || return 0
    cat <<'EOF'
## Scaling

Throughput against connection count and PAR2 thread count. Raw points are in
`summary.csv` (filter `suite == scaling`); this table is the condensed form.

EOF
    report_comparison_table "$summary" scaling pesto
}

# report_comparison_table <summary> <suite> <baseline_tool>
#
# One row per (workload, variant), one column per tool, plus a speedup column
# against the named baseline.
report_comparison_table() {
    local summary=$1 suite=$2 baseline=$3
    awk -F, -v suite="$suite" -v base="$baseline" '
        NR == 1 { next }
        $1 != suite { next }
        {
            # Strip the tool-specific suffix so pesto/nyuu/ngPost rows for the
            # same scenario land on the same table row.
            key = $2 "|" $5
            ms[key, $3] = $10
            mib[key, $3] = $14
            rsd[key, $3] = $13
            arts[key, $3] = $17
            fails[key, $3] = $8
            tools[$3] = 1
            keys[key] = 1
        }
        END {
            # Baseline first, then the rest alphabetically. awk array order is
            # a hash order, so without this the columns would move between
            # runs and two reports could not be diffed against each other.
            n = 0
            for (t in tools) if (t == base) col[++n] = t
            for (t in tools) if (t != base) rest[++m] = t
            for (i = 2; i <= m; i++) {
                v = rest[i]
                for (j = i - 1; j >= 1 && rest[j] > v; j--) rest[j + 1] = rest[j]
                rest[j + 1] = v
            }
            for (i = 1; i <= m; i++) col[++n] = rest[i]

            printf "| workload | case |"
            for (i = 1; i <= n; i++) printf " %s |", col[i]
            printf " best vs %s | noise |\n", base
            printf "|---|---|"
            for (i = 1; i <= n; i++) printf "---|"
            printf "---|---|\n"

            for (k in keys) {
                split(k, p, "|")
                printf "| %s | %s |", p[1], p[2]
                best = ""; best_ms = 0; maxrsd = 0
                counts = ""; ncounts = 0; failed = 0
                for (i = 1; i <= n; i++) {
                    t = col[i]
                    if (!((k SUBSEP t) in ms)) { printf " – |"; continue }
                    printf " %.0f MiB/s |", mib[k, t]
                    if (rsd[k, t] > maxrsd) maxrsd = rsd[k, t]
                    failed += fails[k, t]
                    if (best_ms == 0 || ms[k, t] < best_ms) { best_ms = ms[k, t]; best = t }
                    if (arts[k, t] > 0) {
                        if (index(counts, "|" arts[k, t] "|") == 0) {
                            counts = counts "|" arts[k, t] "|"
                            ncounts++
                        }
                    }
                }
                if ((k SUBSEP base) in ms && best != "") {
                    pct = (ms[k, base] / best_ms - 1) * 100
                    if (best == base) printf " %s fastest |", base
                    else printf " %s %+.1f%% |", best, pct
                } else printf " – |"
                # Two caveats belong next to the numbers, not in a footnote:
                # rows whose tools posted different article counts are not
                # comparable at all, and a row whose median is drawn from a
                # partly-failing tool is drawn from its lucky runs.
                warn = (ncounts > 1) ? " ⚠ article counts differ" : ""
                if (failed > 0) warn = warn sprintf(" ⚠ %d failed rep(s)", failed)
                printf " %.1f%%%s |\n", maxrsd, warn
            }
        }
    ' "$summary" | table_sort_keeping_header
    echo
}

# table_sort_keeping_header — sort a Markdown table's body, keeping its header
# and separator rows first. Uses `read` rather than `head -2` for the same
# reason as sort_keeping_header: `head` over-reads a pipe and drops rows.
#
# Sorts with `-V` when available. Case labels embed numbers (`conn1`, `conn2`,
# `conn16`, `conn32`), and a plain lexicographic sort interleaves them —
# conn1, conn16, conn2, conn32 — which makes a scaling curve unreadable in
# exactly the table whose whole purpose is to show one.
BENCH_SORT_VERSION=$(printf 'a1\na2\n' | sort -V >/dev/null 2>&1 && echo "-V" || echo "")

table_sort_keeping_header() {
    local header separator
    IFS= read -r header || return 0
    IFS= read -r separator || { printf '%s\n' "$header"; return 0; }
    printf '%s\n%s\n' "$header" "$separator"
    sort ${BENCH_SORT_VERSION:+$BENCH_SORT_VERSION} "$@"
}

report_footer() {
    local run=$1
    cat <<EOF

---

## Reproducing this

\`\`\`bash
git clone https://github.com/franzopl/pesto && cd pesto
cargo build --release
./bench/run.sh --scale ${BENCH_SCALE:-1.0} --reps ${BENCH_REPS:-3}
\`\`\`

The corpus is generated from fixed seeds, so the input bytes are identical on
any machine at the same \`--scale\`. Verify with \`./bench/run.sh --verify-data\`.

Raw per-repetition data: \`$(basename "$run")/raw.csv\`.
Machine-readable summary: \`$(basename "$run")/results.json\`.

**Limitations.** Read \`bench/README.md\` before quoting any of these numbers —
in particular, the end-to-end figures are against a local mock server, which
removes real-provider behaviour (per-account concurrency caps, propagation
delay, TLS) from the measurement by design.
EOF
}
