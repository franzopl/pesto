#!/usr/bin/env bash
# bench/compare.sh — regression gate.
#
#   ./bench/compare.sh <baseline.json> <candidate.json> [--threshold PCT]
#
# Compares two `results.json` files case by case and exits non-zero when any
# case got slower by more than the threshold. Intended for CI, but equally
# useful locally before and after a change:
#
#   ./bench/run.sh micro --reps 5 --yes
#   cp bench/results/*/latest/results.json /tmp/before.json
#   …make the change, rebuild…
#   ./bench/run.sh micro --reps 5 --yes
#   ./bench/compare.sh /tmp/before.json bench/results/*/latest/results.json
#
# Two deliberate restrictions:
#
#   * It refuses to compare across different CPU models. A "regression" that is
#     really a different runner is worse than no gate at all, because it trains
#     people to ignore the gate.
#   * The default threshold is 10%, not 2%. Shared CI runners drift by several
#     percent between runs; a tighter bound produces false alarms. Catching
#     10% regressions automatically and the small ones by running on real
#     hardware is the right split.

set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$BENCH_DIR/lib.sh"

THRESHOLD="${BENCH_REGRESSION_THRESHOLD:-10}"
ALLOW_CPU_MISMATCH=0
BASELINE=""
CANDIDATE=""

usage() {
    cat <<'EOF'
usage: bench/compare.sh <baseline.json> <candidate.json> [options]

  --threshold PCT       fail when a case is slower by more than PCT (default 10)
  --allow-cpu-mismatch  compare anyway when the two runs used different CPUs
  -h, --help            this text
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --threshold) THRESHOLD=$2; shift 2 ;;
        --allow-cpu-mismatch) ALLOW_CPU_MISMATCH=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *)
            if [[ -z $BASELINE ]]; then BASELINE=$1
            elif [[ -z $CANDIDATE ]]; then CANDIDATE=$1
            else die "unexpected argument '$1'"
            fi
            shift ;;
    esac
done

[[ -r ${BASELINE:-} ]]  || die "baseline not readable: ${BASELINE:-<missing>}"
[[ -r ${CANDIDATE:-} ]] || die "candidate not readable: ${CANDIDATE:-<missing>}"

json_field() { sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$1" | head -1; }

base_cpu=$(json_field "$BASELINE" cpu_model)
cand_cpu=$(json_field "$CANDIDATE" cpu_model)
if [[ $base_cpu != "$cand_cpu" && $ALLOW_CPU_MISMATCH == 0 ]]; then
    die "CPU mismatch — baseline '$base_cpu' vs candidate '$cand_cpu'.
A comparison across machines measures the machines, not the change.
Refresh the baseline on this runner, or pass --allow-cpu-mismatch."
fi

echo
bold "regression check"; echo
hr
info "baseline  : $BASELINE"
info "candidate : $CANDIDATE"
info "cpu       : $base_cpu"
info "threshold : ${THRESHOLD}% slower"
hr
echo

# Both files are emitted by summary_to_json: a flat array of one-line objects.
# Extracting the three fields that matter with sed keeps this dependency-free —
# jq is not installed everywhere a benchmark needs to run.
extract() {
    grep -o '{[^{}]*}' "$1" |
        grep '"suite"' |
        sed -n 's/.*"suite":"\([^"]*\)".*"workload":"\([^"]*\)".*"tool":"\([^"]*\)".*"variant":"\([^"]*\)".*"wall_median_ms":\([0-9.]*\).*/\1|\2|\3|\4 \5/p'
}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/bench-compare.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

extract "$BASELINE"  > "$scratch/baseline.tsv"
extract "$CANDIDATE" > "$scratch/candidate.tsv"

[[ -s "$scratch/baseline.tsv" ]]  || die "no comparable cases in the baseline"
[[ -s "$scratch/candidate.tsv" ]] || die "no comparable cases in the candidate"

awk -v threshold="$THRESHOLD" '
    NR == FNR { base[$1] = $2; next }
    {
        if (!($1 in base)) { added++; next }
        b = base[$1]; c = $2
        if (b <= 0 || c <= 0) next
        seen[$1] = 1
        # Positive = slower than baseline.
        delta = (c / b - 1) * 100
        rows[++n] = sprintf("%-58s %9.2f %9.2f %+8.1f%%", $1, b, c, delta)
        deltas[n] = delta
        if (delta > threshold) { bad++; badrows[bad] = rows[n] }
        else if (delta < -threshold) { good++; goodrows[good] = rows[n] }
    }
    END {
        for (k in base) if (!(k in seen)) missing++

        printf "%-58s %9s %9s %9s\n", "CASE", "BASE ms", "NEW ms", "CHANGE"
        for (i = 1; i <= n; i++) print rows[i]

        printf "\n%d case(s) compared", n
        if (added)   printf ", %d new", added
        if (missing) printf ", %d missing from candidate", missing
        printf "\n"

        if (good) {
            printf "\n%d case(s) improved by more than %g%%:\n", good, threshold
            for (i = 1; i <= good; i++) print "  " goodrows[i]
        }
        if (bad) {
            printf "\n%d case(s) REGRESSED by more than %g%%:\n", bad, threshold
            for (i = 1; i <= bad; i++) print "  " badrows[i]
            exit 1
        }
        printf "\nno regression beyond %g%%\n", threshold
    }
' "$scratch/baseline.tsv" "$scratch/candidate.tsv"
