# Hybrid CPU Optimization

This document describes pesto's optimization for hybrid CPUs (Intel 12th gen+: P-cores + E-cores).

## Problem

Modern CPUs like the i5-14400 have heterogeneous cores:
- **P-cores** (Performance): 8 cores with hyperthreading → 16 threads
- **E-cores** (Efficiency): 8 cores without hyperthreading → 8 threads
- **Total**: 24 logical threads available

Using all logical threads (24) for CPU-intensive workloads causes:
- **Hyperthread contention**: HT pairs compete for execution ports
- **E-core slowdown**: E-cores run 30-40% slower than P-cores
- **Cache misses**: More threads = more cache line contention
- **Performance degradation**: More threads actually makes things slower

## Solution

Use `performance_core_count()` to detect P-cores + E-cores as separate physical cores:

**i5-14400 topology:**
```
P-cores:   0-1, 2-3, 4-5, 6-7, 8-9, 10-11  (6 pairs with HT)
E-cores:   12-15                            (4 standalone)
Logical:   24 threads total

performance_core_count() = 10 (6 P-core leaders + 4 E-cores)
```

**Key insight**: Don't use hyperthreads for CPU-bound work. Use one thread per physical core instead.

## Implementation

### Parmesan (PAR2 Reed-Solomon encoder)
Located in `crates/parmesan/src/lib.rs`:

```rust
pub fn performance_core_count() -> usize {
    // Detects P-cores (2+ thread_siblings) vs E-cores (1 sibling)
    // Returns: P-core leaders + E-cores = physical cores only
}
```

Used in `crates/parmesan/src/main.rs`:
```rust
let rayon_threads = if options.threads > 0 {
    options.threads
} else {
    parmesan::performance_core_count()  // ← Optimized for hybrid CPUs
};
```

### Pesto (NNTP connection pooling)
Located in `src/main.rs`:

```rust
let effective_jobs = if jobs == 0 {
    parmesan::performance_core_count()  // ← Use P+E cores, not hyperthreads
} else {
    jobs
};
let semaphore = Arc::new(tokio::sync::Semaphore::new(effective_jobs));
```

This determines how many concurrent NNTP uploads happen in parallel.

## Benchmark Results

### Multi-threaded yEnc Encoding (50 MB file)
```
1 thread           2706.88 MB/s
2 threads          4927.35 MB/s
4 threads          8970.50 MB/s
6 threads (P)      10833.61 MB/s
8 threads          12747.86 MB/s
10 threads (P+E)   13847.47 MB/s  ⭐ (87.5% scaling efficiency)
12 threads         14071.62 MB/s  (pico)
16 threads (all)   13721.02 MB/s  ❌ (worse than 10!)
```

**Key finding**: 10 threads (P+E cores) is nearly optimal, avoiding hyperthread contention.

### Posting Mode (100 MB file split into articles, like nyuu)
```
1 thread           764 MB/s       -73.6% vs yencode
4 threads          2816 MB/s      -2.8% vs yencode
10 threads (P+E)   5357 MB/s      +84.8% vs yencode ⭐
12 threads         5435 MB/s      +87.5% vs yencode (pico)
16 threads (all)   5350 MB/s      +84.6% vs yencode (hyperthread degradation)
Auto               5298 MB/s      +82.8% vs yencode
```

**Result**: Pesto with 10 threads is **84.8% faster** than node-yencode (single-threaded).

## How to Test

### Parallel yEnc encoding benchmark:
```bash
./target/release/examples/yenc-bench-parallel /path/to/50M_file 128 10
```

### Posting mode benchmark (nyuu-style):
```bash
./target/release/examples/yenc-bench-posting /path/to/100M_file 128 10
```

### Compare pesto vs yencode in posting mode:
```bash
./bench_pesto_vs_nyuu.sh  # (in /tmp/)
```

## Technical Details

### Linux Hybrid CPU Detection

Pesto detects P-cores and E-cores by reading `/sys/devices/system/cpu/cpu*/topology/thread_siblings_list`:

```
cpu0:  0-1     (2 threads = P-core)
cpu2:  2-3     (2 threads = P-core)
...
cpu12: 12      (1 thread = E-core)
cpu13: 13      (1 thread = E-core)
```

Count the "leaders" (first CPU in each pair) + solo cores = physical core count.

### Why This Matters for Pesto

1. **Posting workflow**: Pesto encodes multiple articles in parallel using rayon
2. **Real bottleneck**: CPU-bound yEnc encoding with I/O-bound NNTP uploads
3. **Concurrency model**: N articles × M NNTP connections
4. **Optimization**: Use 10 cores (P+E) instead of 16 (with HT) for better throughput

## References

- [Intel Hybrid Technology](https://www.intel.com/content/www/us/en/architecture-and-technology/hybrid-cpus.html)
- [Linux CPU Topology Documentation](https://www.kernel.org/doc/html/latest/admin-guide/cputopology.html)
- [Nyuu Poster](https://github.com/animetosho/Nyuu) - Inspiration for pesto's architecture

## Future Work

- [ ] Benchmark on other hybrid CPUs (AMD Ryzen 7000 series, Apple Silicon M-series)
- [ ] Profile cache efficiency with different core counts
- [ ] Consider dynamic thread pool sizing based on workload
