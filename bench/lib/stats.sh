#!/usr/bin/env bash
# bench/lib/stats.sh — summary statistics over repetitions.
#
# The headline figure everywhere in this suite is the *median*, not the mean:
# benchmark noise is one-sided (something else on the machine steals time, it
# never gives time back), so a single unlucky repetition drags a mean down
# while leaving the median alone. Mean, standard deviation and relative
# standard deviation are computed too — in one awk pass over raw.csv inside
# `summarise` — precisely so their disagreement with the median is visible.
# That is the signal that a run was noisy.
#
# This file holds only what shell needs live, while a suite is still running.

# stats_median <values…>
stats_median() {
    printf '%s\n' "$@" | sort -n | awk '
        { v[NR] = $1 }
        END {
            if (NR == 0) { print 0; exit }
            if (NR % 2) print v[(NR + 1) / 2]
            else        printf "%.0f", (v[NR/2] + v[NR/2 + 1]) / 2
        }'
}

# Rows noisier than this are flagged in report.md rather than silently trusted.
BENCH_NOISE_THRESHOLD="${BENCH_NOISE_THRESHOLD:-5}"
