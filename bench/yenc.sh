#!/usr/bin/env bash
# bench/yenc.sh — deprecated. Forwards to the yEnc suite.
#
# The old script measured one SIMD path against node-yencode on a file read
# from disk, with no repetitions and no statistics. `bench/suites/10-yenc.sh`
# measures every SIMD path available on the CPU (so the `auto` dispatch choice
# is visible), in memory, over repetitions, and records the results.
#
# Kept as a shim so existing links, notes and muscle memory keep working.

set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "bench/yenc.sh is deprecated — running ./bench/run.sh yenc instead." >&2
echo "See bench/README.md for the current suite." >&2
echo >&2

if [[ $# -gt 0 ]]; then
    # Old form took sizes in MB; the suite takes them in bytes.
    sizes=""
    for mb in "$@"; do sizes+="$(( mb * 1048576 )),"; done
    export BENCH_YENC_SIZES="${sizes%,}"
fi

exec "$BENCH_DIR/run.sh" yenc
