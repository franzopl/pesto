#!/usr/bin/env bash
# bench/lib/sysinfo.sh — the machine and toolchain fingerprint.
#
# A throughput number without the machine it came from is not a result, it is
# a rumour. Everything here goes into system.json next to the measurements so
# two people comparing tables can see whether they are comparing the same
# thing (SIMD tier and core count in particular decide most PAR2 and yEnc
# numbers), and so a regression check can refuse to compare across CPUs.

cpu_model() {
    if [[ -r /proc/cpuinfo ]]; then
        grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs
    elif command -v sysctl >/dev/null 2>&1; then
        sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown
    else
        echo unknown
    fi
}

cpu_cores_logical()  { nproc 2>/dev/null || sysctl -n hw.logicalcpu 2>/dev/null || echo 0; }

cpu_cores_physical() {
    if [[ -r /proc/cpuinfo ]]; then
        # Sum distinct (physical id, core id) pairs — the only way to get this
        # right on a multi-socket box.
        awk -F: '/^physical id/{p=$2} /^core id/{print p"/"$2}' /proc/cpuinfo |
            sort -u | wc -l
    else
        sysctl -n hw.physicalcpu 2>/dev/null || echo 0
    fi
}

# SIMD tier that actually decides which PAR2/yEnc kernel runs.
simd_flags() {
    local out=()
    if [[ -r /proc/cpuinfo ]]; then
        local flags
        flags=$(grep -m1 -E '^(flags|Features)' /proc/cpuinfo | cut -d: -f2-)
        local f
        for f in ssse3 avx2 avx512f avx512bw gfni vpclmulqdq asimd neon pmull; do
            [[ " $flags " == *" $f "* ]] && out+=("$f")
        done
    elif command -v sysctl >/dev/null 2>&1; then
        sysctl -n machdep.cpu.features machdep.cpu.leaf7_features 2>/dev/null |
            tr 'A-Z ' 'a-z\n' | sort -u | tr '\n' ' '
        return
    fi
    echo "${out[*]:-unknown}"
}

mem_total_kb() {
    if [[ -r /proc/meminfo ]]; then
        awk '/^MemTotal:/{print $2}' /proc/meminfo
    else
        echo 0
    fi
}

# Frequency scaling turns an otherwise clean benchmark into noise, and a
# "powersave" governor can halve a single-threaded result. Recorded, and
# warned about, rather than silently changed.
cpu_governor() {
    local g=/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor
    [[ -r $g ]] && cat "$g" || echo unknown
}

cpu_boost_state() {
    if [[ -r /sys/devices/system/cpu/intel_pstate/no_turbo ]]; then
        [[ $(cat /sys/devices/system/cpu/intel_pstate/no_turbo) == 0 ]] && echo on || echo off
    elif [[ -r /sys/devices/system/cpu/cpufreq/boost ]]; then
        [[ $(cat /sys/devices/system/cpu/cpufreq/boost) == 1 ]] && echo on || echo off
    else
        echo unknown
    fi
}

# Which filesystem the corpus lives on. tmpfs vs a spinning disk changes every
# end-to-end number, so it belongs in the fingerprint.
data_filesystem() {
    local fs
    fs=$(df -PT "${BENCH_DATA_DIR:-.}" 2>/dev/null | awk 'NR==2 {print $2}') || true
    echo "${fs:-unknown}"
}

tool_version() {
    local tool=$1
    command -v "$tool" >/dev/null 2>&1 || { echo "absent"; return; }
    case "$tool" in
        par2)   par2 --version 2>&1 | head -1 ;;
        parpar) parpar --version 2>&1 | head -1 ;;
        nyuu)   nyuu --version 2>&1 | head -1 ;;
        ngPost) ngPost --version 2>&1 | grep -oE 'v[0-9]+\.[0-9]+(\.[0-9]+)?' | head -1 ;;
        node)   node --version 2>&1 | head -1 ;;
        *)      "$tool" --version 2>&1 | head -1 ;;
    esac
}

json_escape() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }

# write_system_json <path>
write_system_json() {
    local out=$1
    local pesto_ver parmesan_ver rustc_ver
    pesto_ver=$([[ -x $PESTO_BIN ]] && "$PESTO_BIN" --version 2>/dev/null | head -1 || echo absent)
    parmesan_ver=$([[ -x $PARMESAN_BIN ]] && "$PARMESAN_BIN" --version 2>/dev/null | head -1 || echo absent)
    rustc_ver=$(rustc --version 2>/dev/null || echo absent)

    cat > "$out" <<EOF
{
  "generated_utc": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')",
  "host": "$(json_escape "$(hostname 2>/dev/null || uname -n)")",
  "os": "$(json_escape "$(uname -s)")",
  "kernel": "$(json_escape "$(uname -r)")",
  "arch": "$(json_escape "$(uname -m)")",
  "cpu_model": "$(json_escape "$(cpu_model)")",
  "cpu_cores_logical": $(cpu_cores_logical),
  "cpu_cores_physical": $(cpu_cores_physical),
  "cpu_governor": "$(cpu_governor)",
  "cpu_boost": "$(cpu_boost_state)",
  "simd_flags": "$(json_escape "$(simd_flags)")",
  "mem_total_kb": $(mem_total_kb),
  "data_filesystem": "$(data_filesystem)",
  "drop_caches": ${BENCH_DROP_CACHES:-0},
  "reps": ${BENCH_REPS:-3},
  "toolchain": {
    "rustc": "$(json_escape "$rustc_ver")",
    "cargo_profile": "release (lto=true, codegen-units=1, panic=abort)",
    "rustflags": "$(json_escape "${RUSTFLAGS:-}")"
  },
  "tools": {
    "pesto": "$(json_escape "$pesto_ver")",
    "parmesan": "$(json_escape "$parmesan_ver")",
    "par2cmdline": "$(json_escape "$(tool_version par2)")",
    "parpar": "$(json_escape "$(tool_version parpar)")",
    "nyuu": "$(json_escape "$(tool_version nyuu)")",
    "ngPost": "$(json_escape "$(tool_version ngPost)")",
    "node": "$(json_escape "$(tool_version node)")"
  }
}
EOF
}

print_system_info() {
    printf "  CPU        : %s\n" "$(cpu_model)"
    printf "  Cores      : %s logical / %s physical\n" "$(cpu_cores_logical)" "$(cpu_cores_physical)"
    printf "  SIMD       : %s\n" "$(simd_flags)"
    printf "  Memory     : %s\n" "$(human_bytes $(( $(mem_total_kb) * 1024 )))"
    printf "  Kernel     : %s %s\n" "$(uname -s)" "$(uname -r)"
    printf "  Governor   : %s (boost: %s)\n" "$(cpu_governor)" "$(cpu_boost_state)"
    printf "  Corpus FS  : %s\n" "$(data_filesystem)"
    printf "  Date       : %s\n" "$(date -u '+%Y-%m-%d %H:%M UTC')"

    if [[ $(cpu_governor) == powersave ]]; then
        warn "CPU governor is 'powersave' — single-threaded results will be low and noisy."
        warn "Consider: sudo cpupower frequency-set -g performance"
    fi
}
