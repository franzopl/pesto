#!/usr/bin/env bash
# bench/posting.sh — deprecated. Forwards to the pipeline-stage suite.
#
# The old script timed `pesto --dry-run` at a few connection counts, which
# measures almost nothing about connections: --dry-run never opens one.
# `bench/suites/30-stages.sh` isolates each pipeline stage against a real local
# NNTP endpoint and attributes cost to it; `bench/suites/50-scaling.sh` is the
# one that actually sweeps connection counts, and does it at several simulated
# round-trip times, where the count starts to matter.
#
# Kept as a shim so existing links, notes and muscle memory keep working.

set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "bench/posting.sh is deprecated — running ./bench/run.sh stages instead." >&2
echo "For connection scaling use: ./bench/run.sh scaling" >&2
echo "See bench/README.md." >&2
echo >&2

exec "$BENCH_DIR/run.sh" stages "$@"
