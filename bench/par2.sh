#!/usr/bin/env bash
# bench/par2.sh — deprecated. Forwards to the PAR2 suite.
#
# The old script gave each tool a target slice *count* and let it pick its own
# slice size. They round differently, so the three tools were doing measurably
# different amounts of GF(2^16) arithmetic and the comparison was not fair.
# `bench/suites/20-par2.sh` computes one geometry — explicit slice size and
# explicit recovery block count — and pushes it into all three, adds verify and
# repair, and reports peak RSS alongside throughput.
#
# Kept as a shim so existing links, notes and muscle memory keep working.

set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "bench/par2.sh is deprecated — running ./bench/run.sh par2 instead." >&2
echo "See bench/README.md for the current suite and its flag mapping." >&2
echo >&2

exec "$BENCH_DIR/run.sh" par2 --workload movie-1080p "$@"
