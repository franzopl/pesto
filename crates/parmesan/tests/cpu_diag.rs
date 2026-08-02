// Throwaway diagnostic: print which SIMD dispatch path the CI runner's CPU
// actually takes, to debug the flaky decoder proptest failure. Not meant to
// be merged.
#[test]
#[cfg(target_arch = "x86_64")]
fn print_cpu_features() {
    panic!(
        "CPU_DIAG avx2={} ssse3={} avx512f={} avx512bw={} gfni={} avx512vbmi={} nproc={}",
        std::is_x86_feature_detected!("avx2"),
        std::is_x86_feature_detected!("ssse3"),
        std::is_x86_feature_detected!("avx512f"),
        std::is_x86_feature_detected!("avx512bw"),
        std::is_x86_feature_detected!("gfni"),
        std::is_x86_feature_detected!("avx512vbmi"),
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
    );
}
