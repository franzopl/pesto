#!/usr/bin/env bash
# bench/suites/60-correctness.sh — the checks that make the numbers mean
# something.
#
# A PAR2 encoder can be made arbitrarily fast by producing recovery data that
# does not recover anything, and a yEnc encoder by producing output nobody can
# decode. Every performance claim in this suite therefore has to sit next to
# evidence that the output is correct *and interoperable* — not just
# self-consistent.
#
# What is asserted:
#
#   1. parmesan creates    → par2cmdline verifies, repairs, byte-exact restore
#   2. par2cmdline creates → parmesan verifies, repairs, byte-exact restore
#   3. parpar creates      → parmesan verifies (parpar has no verify of its own)
#   4. every tool's recovery set holds at least the payload that was requested,
#      with its packet overhead reported rather than treated as an error
#   5. pesto posts         → an independent decoder reassembles the source
#                            byte-for-byte from the articles on the wire
#
# (5) is the one worth the effort: it reads what actually went over the socket
# (captured by the mock server), decodes it with a yEnc implementation that is
# not pesto's, and compares a checksum with the input. It is the only check
# here that covers the encoder, the segmenter and the article builder at once.

set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

SUITE=correctness
CHECKS_PASSED=0
CHECKS_FAILED=0

check_ok()   { CHECKS_PASSED=$(( CHECKS_PASSED + 1 )); printf "  %s %s\n" "$(green ✓)" "$1"; }
check_fail() { CHECKS_FAILED=$(( CHECKS_FAILED + 1 )); printf "  %s %s\n" "$(red ✗)" "$1"; }
check_skip() { printf "  %s %s\n" "$(dim '–')" "$(dim "$1")"; }

suite_correctness() {
    step "Cross-tool correctness"

    local work="$BENCH_SANDBOX/correctness"
    rm -rf "$work"; mkdir -p "$work/src"

    # This suite builds its own corpus rather than reusing a workload, for two
    # reasons. It must be *flat*: all three tools store bare file names in
    # their File Description packets, so a set can only be verified and
    # repaired when every protected file sits beside the index — a nested
    # release folder would fail these checks for a reason that says nothing
    # about interoperability. And it must be *small*: correctness does not get
    # more true at 40 GiB, and a check suite people skip because it takes an
    # hour protects nothing.
    #
    # Several files of differing sizes, not one: multi-file sets are where the
    # PAR2 spec's File-ID block ordering matters, and a mismatch there produces
    # a "successful" repair that writes the wrong bytes.
    local sizes=(4194304 8388608 1048576 12582912 2097152 6291456)
    local i
    for i in "${!sizes[@]}"; do
        "$BENCH_GEN_BIN" --out "$work/src/part-$i.bin" \
            --size "${sizes[$i]}" --seed "$(( 7000 + i ))" --entropy 100 > /dev/null
    done
    # One file whose size is an exact multiple of the slice size, which is the
    # boundary that used to crash parmesan outright (see
    # crates/parmesan/tests/exact_multiple_slice_size.rs).
    "$BENCH_GEN_BIN" --out "$work/src/aligned.bin" --size 8388608 --seed 7100 \
        --entropy 100 > /dev/null

    mapfile -t inputs < <(find "$work/src" -type f | sort)
    par2_geometry "$work/src" 512 10
    info "corpus: ${#inputs[@]} files, $(human_bytes "$(dir_size_bytes "$work/src")")"
    info "geometry: slice=$P2_SLICE_SIZE, $P2_SLICE_COUNT input, $P2_RECOVERY_COUNT recovery"
    echo

    check_parmesan_to_par2cmdline "$work"
    check_par2cmdline_to_parmesan "$work"
    check_parpar_to_parmesan "$work"
    check_recovery_size "$work"
    check_wire_roundtrip "$work"

    echo
    if (( CHECKS_FAILED > 0 )); then
        printf "  %s\n" "$(red "$CHECKS_FAILED check(s) failed, $CHECKS_PASSED passed")"
        return 1
    fi
    printf "  %s\n" "$(green "all $CHECKS_PASSED checks passed")"
}

# tree_digest <dir> — order-independent checksum of the protected files, the
# comparison used to prove a repair restored the original bytes.
#
# Excludes the recovery set, and par2cmdline's numbered pre-repair backups
# (`part-0.bin.1`, …). Counting those would make every successful
# par2cmdline repair look like a mismatch: the data files come back correct,
# but the directory has gained a file the "before" digest never saw.
tree_digest() {
    find "$1" -type f ! -name '*.par2' ! -regex '.*\.[0-9]+$' -print0 |
        sort -z |
        xargs -0 -r sha256sum 2>/dev/null |
        awk '{print $1}' |
        sha256sum |
        awk '{print $1}'
}

# corrupt_first <dir> <blocks> <block_size> — deterministic damage.
corrupt_first() {
    local dir=$1 blocks=$2 size=$3
    local victim
    victim=$(find "$dir" -type f ! -name '*.par2' | sort | head -1)
    local i
    for (( i = 0; i < blocks; i++ )); do
        dd if=/dev/zero of="$victim" bs="$size" count=1 seek="$(( i * 2 ))" \
            conv=notrunc status=none 2>/dev/null || break
    done
}

check_parmesan_to_par2cmdline() {
    local work=$1
    if ! tool_present par2; then
        check_skip "parmesan → par2cmdline (par2cmdline not installed)"
        return 0
    fi
    local dir="$work/a"; rm -rf "$dir"; mkdir -p "$dir"
    cp -r "$work/src"/. "$dir/"
    local before; before=$(tree_digest "$dir")
    mapfile -t files < <(find "$dir" -type f | sort)

    par2_create_parmesan "$dir" set "${files[@]}" >/dev/null 2>&1 ||
        { check_fail "parmesan create failed"; return 0; }

    if par2 verify -q -q -B "$dir" "$dir/set.par2" >/dev/null 2>&1; then
        check_ok "par2cmdline verifies a parmesan recovery set"
    else
        check_fail "par2cmdline rejects a parmesan recovery set"
        return 0
    fi

    corrupt_first "$dir" "$(( P2_RECOVERY_COUNT / 2 + 1 ))" "$P2_SLICE_SIZE"
    if par2 repair -q -q -B "$dir" "$dir/set.par2" >/dev/null 2>&1 &&
       [[ $(tree_digest "$dir") == "$before" ]]; then
        check_ok "par2cmdline repairs from a parmesan recovery set (byte-exact)"
    else
        check_fail "par2cmdline could not repair from a parmesan recovery set"
    fi
}

check_par2cmdline_to_parmesan() {
    local work=$1
    if ! tool_present par2; then
        check_skip "par2cmdline → parmesan (par2cmdline not installed)"
        return 0
    fi
    local dir="$work/b"; rm -rf "$dir"; mkdir -p "$dir"
    cp -r "$work/src"/. "$dir/"
    local before; before=$(tree_digest "$dir")
    mapfile -t files < <(find "$dir" -type f | sort)

    par2_create_par2cmdline "$dir" set "${files[@]}" >/dev/null 2>&1 ||
        { check_fail "par2cmdline create failed"; return 0; }

    if "$PARMESAN_BIN" verify -q "$dir/set.par2" >/dev/null 2>&1; then
        check_ok "parmesan verifies a par2cmdline recovery set"
    else
        check_fail "parmesan rejects a par2cmdline recovery set"
        return 0
    fi

    corrupt_first "$dir" "$(( P2_RECOVERY_COUNT / 2 + 1 ))" "$P2_SLICE_SIZE"
    if "$PARMESAN_BIN" repair -q "$dir/set.par2" >/dev/null 2>&1 &&
       [[ $(tree_digest "$dir") == "$before" ]]; then
        check_ok "parmesan repairs from a par2cmdline recovery set (byte-exact)"
    else
        check_fail "parmesan could not repair from a par2cmdline recovery set"
    fi
}

check_parpar_to_parmesan() {
    local work=$1
    if ! tool_present parpar; then
        check_skip "parpar → parmesan (parpar not installed)"
        return 0
    fi
    local dir="$work/c"; rm -rf "$dir"; mkdir -p "$dir"
    cp -r "$work/src"/. "$dir/"
    mapfile -t files < <(find "$dir" -type f | sort)

    par2_create_parpar "$dir" set "${files[@]}" >/dev/null 2>&1 ||
        { check_fail "parpar create failed"; return 0; }

    if "$PARMESAN_BIN" verify -q "$dir/set.par2" >/dev/null 2>&1; then
        check_ok "parmesan verifies a parpar recovery set"
    else
        check_fail "parmesan rejects a parpar recovery set"
    fi
}

# Recovery payload accounting.
#
# Every tool was asked for exactly P2_RECOVERY_COUNT blocks of P2_SLICE_SIZE
# bytes. What lands on disk is always larger, and legitimately so: the PAR2
# format repeats the critical packets (Main, File Description, Input File Slice
# Checksum, Creator) in *every* volume so any single volume is enough to
# identify the set. On a 2 000-file corpus those packets are hundreds of
# kilobytes and the repetition dominates the recovery data itself.
#
# So the assertion is one-directional — the payload must be at least what was
# requested, because a tool that quietly produced fewer blocks would otherwise
# post an excellent throughput number — and the overhead ratio is reported as
# a result in its own right, since it differs several-fold between tools and
# is exactly what an uploader pays for in extra articles.
check_recovery_size() {
    local work=$1
    local expected=$(( P2_RECOVERY_COUNT * P2_SLICE_SIZE ))
    local tool dir actual ratio
    for tool in parmesan par2cmdline parpar; do
        case $tool in
            parmesan)    dir="$work/a" ;;
            par2cmdline) dir="$work/b" ;;
            parpar)      dir="$work/c" ;;
        esac
        [[ -d $dir ]] || { check_skip "recovery size: $tool (not run)"; continue; }
        actual=$(dir_size_bytes "$dir" '*.par2')
        (( actual > 0 )) || { check_skip "recovery size: $tool (no output)"; continue; }
        ratio=$(awk -v a="$actual" -v e="$expected" 'BEGIN{printf "%.2f", a / e}')
        record_row suite="$SUITE" workload="recovery-size" tool="$tool" \
            variant="size-accuracy" rep=1 input_bytes="$expected" \
            wall_ms=0 user_ms=0 sys_ms=0 max_rss_kb=0 exit_code=0 \
            articles=0 output_bytes="$actual" \
            extra="expected_bytes=$expected;ratio=$ratio"
        if (( actual >= expected )); then
            check_ok "$tool recovery set holds the requested payload \
($(human_bytes "$actual") on disk for $(human_bytes "$expected") of blocks; ${ratio}x with packet overhead)"
        else
            check_fail "$tool produced less recovery data than requested \
($(human_bytes "$actual") vs $(human_bytes "$expected"))"
        fi
    done
}

# The wire round-trip: post a small file to the mock server with --save-dir,
# then reassemble it from the captured articles with an independent decoder
# and compare checksums.
check_wire_roundtrip() {
    local work=$1
    local dir="$work/wire"
    rm -rf "$dir"; mkdir -p "$dir/articles" "$dir/src"

    # Deliberately small and deliberately its own file: this check writes
    # every article to disk, and doing that for a multi-gigabyte workload
    # would cost more than the rest of the suite combined.
    "$BENCH_GEN_BIN" --out "$dir/src/payload.bin" --size 8388608 --seed 424242 \
        --entropy 100 > /dev/null

    mock_start --save-dir "$dir/articles"
    write_pesto_config "$dir/pesto.toml" 127.0.0.1 "$MOCK_PORT" 4

    "$PESTO_BIN" --config "$dir/pesto.toml" --par2 0 --no-check \
        --article-size 768000 --line-length 128 --connections 4 \
        --no-hooks --no-history --no-notify --no-session-log \
        --output-format json -o "$dir/out.nzb" "$dir/src/payload.bin" \
        >/dev/null 2>&1 || { mock_stop; check_fail "pesto post to mock server failed"; return 0; }
    mock_stop

    local decoded="$dir/decoded.bin"
    if ! python3 "$BENCH_DIR/tools/yenc_decode.py" "$dir/articles" "$decoded" \
         >"$dir/decode.log" 2>&1; then
        check_fail "independent yEnc decode failed ($(tail -1 "$dir/decode.log"))"
        return 0
    fi

    local want got
    want=$(sha256sum "$dir/src/payload.bin" | awk '{print $1}')
    got=$(sha256sum "$decoded" | awk '{print $1}')
    if [[ $want == "$got" ]]; then
        check_ok "posted articles decode back to the source byte-for-byte"
    else
        check_fail "wire round-trip mismatch (source $want, decoded $got)"
    fi
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    bench_standalone_init "$SUITE"
    suite_correctness
fi
