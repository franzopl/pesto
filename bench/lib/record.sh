#!/usr/bin/env bash
# bench/lib/record.sh — the result schema, and everything that writes it.
#
# One wide CSV of raw repetitions is the single source of truth; summary.csv,
# results.json and report.md are all derived from it. That ordering matters:
# it means a result can be re-aggregated (differently, later, by someone
# else's script) without re-running an eight-hour benchmark.
#
# Columns are fixed so the file stays loadable by anything (pandas, awk, a
# spreadsheet). Suite-specific dimensions — SIMD path, connection count,
# recovery percentage, mock-server latency — go into the free-form `extra`
# column as `k=v;k=v`. That is what makes a new workload or a new competitor
# a pure addition, with no schema migration.

BENCH_CSV_COLUMNS=(
    timestamp host suite workload tool tool_version variant rep
    input_bytes wall_ms user_ms sys_ms max_rss_kb exit_code
    articles output_bytes extra
)

record_init() {
    BENCH_RAW_CSV="$BENCH_RUN_DIR/raw.csv"
    (IFS=,; echo "${BENCH_CSV_COLUMNS[*]}") > "$BENCH_RAW_CSV"
    BENCH_HOST=$(hostname 2>/dev/null || uname -n)
}

# csv_field — normalise one field so the file needs no quoting at all.
#
# Commas become semicolons and newlines become spaces. Quoting would be more
# faithful, but every consumer downstream (the awk aggregator here, and
# whatever one-liner someone writes against raw.csv later) would then need a
# real CSV parser. No field this suite records has any use for a literal
# comma, so the trade is free: `extra` already uses `;` as its own separator.
csv_field() {
    local v=${1//,/;}
    v=${v//$'\n'/ }
    printf '%s' "${v//\"/}"
}

# record_row key=value… — order-independent; missing keys default to empty.
record_row() {
    declare -A f=()
    local kv
    for kv in "$@"; do
        f[${kv%%=*}]=${kv#*=}
    done
    f[timestamp]=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    f[host]=$BENCH_HOST

    local out="" col
    for col in "${BENCH_CSV_COLUMNS[@]}"; do
        out+="$(csv_field "${f[$col]:-}"),"
    done
    printf '%s\n' "${out%,}" >> "$BENCH_RAW_CSV"
}

# ── aggregation ──────────────────────────────────────────────────────────────

# summarise <raw.csv> <summary.csv>
#
# Groups repetitions by everything except `rep`, and reports median / mean /
# stddev / rsd along with the derived rates. Failed repetitions (non-zero
# exit) are excluded from the statistics but counted, so a tool that crashes
# half the time cannot post a good median.
summarise() {
    local raw=$1 out=$2
    awk -F, -v OFS=, '
        NR == 1 { next }
        {
            # No field can contain a comma (see csv_field), so a plain split
            # on "," is exact and every column index below is stable.
            key = $3 SUBSEP $4 SUBSEP $5 SUBSEP $6 SUBSEP $7 SUBSEP $17
            if ($14 != 0) { fail[key]++; next }
            n[key]++
            wall[key, n[key]] = $10
            input[key] = $9
            usr[key] += $11
            sys[key] += $12
            if ($13 + 0 > rss[key]) rss[key] = $13
            arts[key] = $15
            outb[key] = $16
        }
        END {
            print "suite","workload","tool","tool_version","variant","extra", \
                  "runs","failures","input_bytes","wall_median_ms","wall_mean_ms", \
                  "wall_stddev_ms","rsd_pct","mibps_median","cpu_ratio", \
                  "max_rss_kb","articles","output_bytes"
            for (k in n) {
                cnt = n[k]
                for (i = 1; i <= cnt; i++) v[i] = wall[k, i]
                # Insertion sort: cnt is the repetition count, never large.
                for (i = 2; i <= cnt; i++) {
                    t = v[i]
                    for (j = i - 1; j >= 1 && v[j] > t; j--) v[j + 1] = v[j]
                    v[j + 1] = t
                }
                med = (cnt % 2) ? v[(cnt + 1) / 2] : (v[cnt/2] + v[cnt/2 + 1]) / 2

                s = 0; for (i = 1; i <= cnt; i++) s += v[i]
                mean = s / cnt
                d = 0; for (i = 1; i <= cnt; i++) d += (v[i] - mean) ^ 2
                sd  = (cnt > 1) ? sqrt(d / (cnt - 1)) : 0
                rsd = (mean > 0) ? 100 * sd / mean : 0

                mibps = (med > 0) ? (input[k] / 1048576) / (med / 1000) : 0
                cpu   = (med > 0) ? (usr[k] + sys[k]) / cnt / med : 0

                split(k, p, SUBSEP)
                # Nanosecond resolution, expressed in milliseconds. The
                # smallest yEnc case runs in well under a microsecond per
                # iteration; at %.3f it rounded to 0.001 ms — or to 0.000,
                # which then reported 0 MiB/s — and three different SIMD paths
                # collapsed onto one quantised value. Every other suite is
                # orders of magnitude slower and does not care, so the extra
                # digits cost nothing.
                #
                # (No apostrophes in this block: the whole awk program is a
                # single-quoted shell string, and one would end it.)
                printf "%s,%s,%s,%s,%s,%s,%d,%d,%d,%.6f,%.6f,%.6f,%.1f,%.1f,%.2f,%d,%d,%d\n",
                    p[1], p[2], p[3], p[4], p[5], p[6],
                    cnt, fail[k] + 0, input[k], med, mean, sd, rsd, mibps, cpu,
                    rss[k], arts[k], outb[k]
            }
        }
    ' "$raw" | sort_keeping_header -t, -k1,1 -k2,2 -k5,5 -k3,3 > "$out"
}

# sort_keeping_header <sort args…> — sort stdin, leaving line 1 in place.
#
# The obvious `{ head -1; tail -n +2 | sort; }` silently loses rows: `head`
# reads a whole buffer from the pipe and discards everything past the first
# line, so `tail` sees an already-drained stream. `read` consumes exactly one
# line, which is the only version of this that is correct on a pipe.
sort_keeping_header() {
    IFS= read -r header || return 0
    printf '%s\n' "$header"
    sort "$@"
}

# summary_to_json <summary.csv> <system.json> <out.json>
summary_to_json() {
    local summary=$1 system=$2 out=$3
    {
        printf '{\n  "system": '
        cat "$system"
        printf ',\n  "results": [\n'
        awk -F, '
            NR == 1 { for (i = 1; i <= NF; i++) col[i] = $i; ncol = NF; next }
            {
                printf "%s    {", (NR > 2 ? ",\n" : "")
                for (i = 1; i <= ncol; i++) {
                    # Numeric columns stay unquoted so the JSON is directly
                    # usable for plotting without a cast on every field.
                    numeric = ($i ~ /^-?[0-9]+(\.[0-9]+)?$/)
                    printf "%s\"%s\":%s", (i > 1 ? "," : ""), col[i],
                           (numeric ? $i : "\"" $i "\"")
                }
                printf "}"
            }
            END { printf "\n" }
        ' "$summary"
        printf '  ]\n}\n'
    } > "$out"
}
