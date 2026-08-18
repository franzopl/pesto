#!/usr/bin/env bash
# bench/lib/tools.sh — competitor discovery, and the flag mapping that makes
# a comparison fair.
#
# This file is where "we beat X by N%" either becomes a defensible claim or
# stays marketing. Two tools only produce comparable numbers when they are
# doing comparable work, and for PAR2 that means the *same geometry*: same
# slice size, same input slice count, same recovery block count, same thread
# count, same memory ceiling. Any one of those left to a tool's own default
# silently changes the amount of GF(2^16) arithmetic performed and invalidates
# the row.
#
# The mapping is stated once here, in one place, and every suite uses it.

# ── discovery ────────────────────────────────────────────────────────────────

have_tool() { command -v "$1" >/dev/null 2>&1; }

# Competitors are optional by design: the suite must be runnable by someone
# who has none of them installed, and must say so rather than silently
# reporting a one-tool table as a comparison.
#
# Declared only once: every suite re-sources lib.sh so it stays independently
# runnable, and an unconditional `declare -A … =()` here would wipe the
# detection results mid-run — turning every competitor into "not installed"
# from the first suite onwards.
if ! declare -p BENCH_TOOL_PRESENT >/dev/null 2>&1; then
    declare -A BENCH_TOOL_PRESENT=()
fi

detect_tools() {
    local t
    for t in par2 parpar nyuu ngPost node 7z rar; do
        if have_tool "$t"; then
            BENCH_TOOL_PRESENT[$t]=1
        else
            BENCH_TOOL_PRESENT[$t]=0
        fi
    done
    # node-yencode is a module, not a binary: having node is not enough.
    BENCH_TOOL_PRESENT[node-yencode]=0
    if [[ ${BENCH_TOOL_PRESENT[node]} == 1 ]] &&
       node -e 'require("yencode")' >/dev/null 2>&1; then
        BENCH_TOOL_PRESENT[node-yencode]=1
    fi
}

tool_present() { [[ ${BENCH_TOOL_PRESENT[$1]:-0} == 1 ]]; }

skip_missing() {
    local tool=$1
    tool_present "$tool" && return 1
    printf "  %-28s %s\n" "$tool" "$(dim '(not installed — skipped)')"
    return 0
}

print_tool_table() {
    local t
    printf "  %-14s %-10s %s\n" "TOOL" "STATUS" "VERSION"
    for t in par2 parpar nyuu ngPost node-yencode 7z rar; do
        if tool_present "$t"; then
            printf "  %-14s %-10s %s\n" "$t" "$(green present)" \
                "$(tool_version "${t/node-yencode/node}")"
        else
            printf "  %-14s %-10s %s\n" "$t" "$(dim absent)" "-"
        fi
    done
}

# ── PAR2 geometry ────────────────────────────────────────────────────────────

# par2_geometry <corpus_dir> <target_slices> <recovery_pct>
#
# Computes one geometry and pushes it into every tool, instead of letting each
# pick its own from a target slice count. They do not agree: parmesan, parpar
# and par2cmdline all round differently, so `--slice-count 2000` / `-s2000` /
# `-b2000` produce three different slice sizes and therefore three different
# amounts of GF(2^16) arithmetic. Passing an explicit slice size in bytes and
# an explicit recovery block count removes that whole class of unfairness.
#
# The input slice count is the sum of per-file ceilings, not one division of
# the total: PAR2 slices each file independently and pads its last slice, so a
# corpus of 2 000 small files has ~2 000 slices no matter how large the total
# is. Getting this wrong would size the recovery set against a slice count
# that does not exist.
#
# Sets: P2_SLICE_SIZE, P2_SLICE_COUNT, P2_RECOVERY_COUNT.
par2_geometry() {
    local dir=$1 target=$2 pct=$3

    local sizes
    sizes=$(find "$dir" -type f ! -name '.manifest.jsonl' ! -name '*.par2' \
        -printf '%s\n' 2>/dev/null | sort -n)
    [[ -n $sizes ]] || die "par2_geometry: no input files under $dir"

    # Pick the smallest 4 KiB-aligned slice size whose resulting count fits the
    # spec's 32 768-slice ceiling, starting from the size the target count
    # implies. Doubling rather than failing mirrors what every real tool does
    # for large inputs, and keeps the huge workloads runnable.
    read -r P2_SLICE_SIZE P2_SLICE_COUNT <<< "$(awk -v target="$target" '
        { size[NR] = $1; total += $1 }
        END {
            s = int(total / target)
            if (s < 4096) s = 4096
            s = int((s + 4095) / 4096) * 4096
            for (;;) {
                count = 0
                for (i = 1; i <= NR; i++) {
                    c = int((size[i] + s - 1) / s)
                    count += (c < 1 ? 1 : c)
                }
                if (count <= 32768) break
                s *= 2
            }
            print s, count
        }' <<< "$sizes")"

    P2_RECOVERY_COUNT=$(awk -v c="$P2_SLICE_COUNT" -v p="$pct" 'BEGIN {
        n = int(c * p / 100 + 0.5)
        printf "%d", (n < 1 ? 1 : n)
    }')

    # PAR2 spec caps recovery blocks at 65 535. Unlike the input-slice ceiling
    # this cannot be fixed by resizing slices, so it is a hard stop.
    if (( P2_RECOVERY_COUNT > 65535 )); then
        die "geometry needs $P2_RECOVERY_COUNT recovery blocks (spec max 65535) — \
lower --tier/--scale or the recovery percentage"
    fi
}

# Shared compute budget. Left to their defaults, parpar takes up to 75% of
# free RAM while parmesan takes 1 GiB — so parpar would make one pass over the
# input where parmesan makes several, and the resulting table would be
# measuring memory policy, not arithmetic throughput.
BENCH_PAR2_MEMORY="${BENCH_PAR2_MEMORY:-1024}"   # MiB
BENCH_PAR2_THREADS="${BENCH_PAR2_THREADS:-$(cpu_cores_physical)}"

par2_geometry_note() {
    printf 'slice_size=%s;slice_count=%s;recovery_count=%s;threads=%s;mem_mib=%s' \
        "$P2_SLICE_SIZE" "$P2_SLICE_COUNT" "$P2_RECOVERY_COUNT" \
        "$BENCH_PAR2_THREADS" "$BENCH_PAR2_MEMORY"
}

# ── matched PAR2 create command lines ────────────────────────────────────────
#
#                   parmesan            parpar              par2cmdline
#   slice size      -s <bytes>          -s <bytes>B         -s<bytes>
#   recovery blocks --recovery-count N  -r <N>              -c<N>
#   threads         -t N                -t N                -t<N>
#   memory          -m <MiB>MiB         -m <MiB>M           -m<MiB>
#   output base     -o DIR -b NAME      -o DIR/NAME.par2    -a DIR/NAME.par2
#   path handling   (basenames)         -f basename         -B DIR
#   quiet           -q                  -q                  -q -q
#
# All three end up storing bare file names in the File Description packets,
# which is what makes the recovery sets interchangeable in 60-correctness.sh.
#
# parpar has no verify or repair mode, so it appears only in create tables.
#
# These builders set the global array P2_CMD rather than running anything.
# The measurement wrapper hands that array straight to /usr/bin/time, which
# can only exec a real binary — routing a shell function through `bash -c`
# instead would add an interpreter startup to every timing.

par2_cmd_parmesan() {
    local out_dir=$1 base=$2; shift 2
    P2_CMD=("$PARMESAN_BIN" create
        -s "$P2_SLICE_SIZE"
        --recovery-count "$P2_RECOVERY_COUNT"
        -t "$BENCH_PAR2_THREADS"
        -m "${BENCH_PAR2_MEMORY}MiB"
        --simd "${BENCH_SIMD:-auto}"
        -o "$out_dir" -b "$base" -O -q "$@")
}

par2_cmd_parpar() {
    local out_dir=$1 base=$2; shift 2
    # `-f basename` matches what the other two do with paths: store the bare
    # file name in the File Description packet. Left at parpar's default
    # (`common`) the recovery sets would not be interchangeable, and
    # 60-correctness.sh's cross-verification would fail for a reason that has
    # nothing to do with either implementation.
    P2_CMD=(parpar
        -s "${P2_SLICE_SIZE}B"
        -r "$P2_RECOVERY_COUNT"
        -t "$BENCH_PAR2_THREADS"
        -m "${BENCH_PAR2_MEMORY}M"
        -f basename
        -o "$out_dir/$base.par2" -O -q -- "$@")
}

par2_cmd_par2cmdline() {
    local out_dir=$1 base=$2; shift 2
    # -B is not optional here. par2cmdline refuses ("Ignoring out of basepath
    # source file") any input that does not live under the basepath, which
    # defaults to the archive's own directory — so without it a run whose
    # recovery set and sources are in different places silently protects
    # nothing and still exits 0.
    P2_CMD=(par2 create
        -B "$out_dir"
        -s"$P2_SLICE_SIZE"
        -c"$P2_RECOVERY_COUNT"
        -t"$BENCH_PAR2_THREADS"
        -m"$BENCH_PAR2_MEMORY"
        -q -q
        -a "$out_dir/$base.par2" -- "$@")
}

# par2_create_<tool> — run the matched command line once, outside a
# measurement (used to prepare a recovery set for the verify/repair cases and
# for the correctness suite).
par2_create_parmesan()   { par2_cmd_parmesan   "$@"; "${P2_CMD[@]}"; }
par2_create_parpar()     { par2_cmd_parpar     "$@"; "${P2_CMD[@]}"; }
par2_create_par2cmdline() { par2_cmd_par2cmdline "$@"; "${P2_CMD[@]}"; }

# ── generated configs ────────────────────────────────────────────────────────

# write_pesto_config <path> <host> <port> <connections>
#
# Always generated, never the operator's own: the suite must not be able to
# reach a real provider even by accident, and a config in the sandbox is the
# only thing pesto will find (see bench_sandbox_init).
write_pesto_config() {
    local path=$1 host=$2 port=$3 conns=$4
    assert_local_target "$host"
    mkdir -p "$(dirname "$path")"
    cat > "$path" <<EOF
# Generated by bench/ — points at the local mock NNTP server only.
[server]
host = "$host"
port = $port
ssl = false
connections = $conns

[auth]
username = "bench"
password = "bench"

[posting]
from = "bench <bench@localhost>"
groups = ["alt.binaries.test"]

[output]
history = false
session_log = false
EOF
}

# write_pesto_dual_config <path> <host> <port1> <port2> <conns_each>
#
# Two [[servers]] entries, same credentials, equal connection quotas. Used by
# the heterogeneous suite so half the workers land on each mock.
write_pesto_dual_config() {
    local path=$1 host=$2 port1=$3 port2=$4 conns=$5
    assert_local_target "$host"
    mkdir -p "$(dirname "$path")"
    cat > "$path" <<EOF
# Generated by bench/ — two local mock NNTP servers only.
[[servers]]
host = "$host"
port = $port1
ssl = false
connections = $conns
username = "bench"
password = "bench"

[[servers]]
host = "$host"
port = $port2
ssl = false
connections = $conns
username = "bench"
password = "bench"

[posting]
from = "bench <bench@localhost>"
groups = ["alt.binaries.test"]

[output]
history = false
session_log = false
EOF
}

# write_ngpost_config <path> <host> <port> <connections> <article_size> <nzb_dir>
write_ngpost_config() {
    local path=$1 host=$2 port=$3 conns=$4 art=$5 nzb_dir=$6
    assert_local_target "$host"
    mkdir -p "$(dirname "$path")" "$nzb_dir"
    cat > "$path" <<EOF
lang = EN
nzbPath = $nzb_dir
GROUPS = alt.binaries.test
FROM = bench@localhost
ARTICLE_SIZE = $art
NB_RETRY = 1
DISP_PROGRESS = NONE
GROUP_POLICY = ALL

[server]
host = $host
port = $port
ssl = false
user = bench
pass = bench
connection = $conns
enabled = true
nzbCheck = false
EOF
}
