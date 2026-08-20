## Context

In Issue #144, we introduced a fast-path for the ingestion of small files (`<= 8 MiB`) in `pesto`'s `poster` to bypass the overhead of `spawn_blocking` and channel handoffs. While this significantly improved throughput for the `many-small` workload, bringing `pesto` back into the same order of magnitude as competitors, we still observe a performance gap in 0ms-latency scenarios.

## Benchmark Results (many-small, 500.0 MiB, 2000 files)

When running the end-to-end suite with 0ms and 30ms latency, the results are as follows:

### Latency: 0 ms (CPU/Processing Bound)
| Tool | Time | Speed | Memory |
|------|------|-------|--------|
| `ngPost` (C++) | 0.75s | 663.1 MiB/s | 58.0 MiB |
| `nyuu` (Node) | 0.99s | 505.1 MiB/s | 52.9 MiB |
| `pesto` (Rust) | 1.26s | 396.2 MiB/s | 32.6 MiB |

### Latency: 30 ms (Network/Pipeline Bound)
| Tool | Time | Speed | Memory |
|------|------|-------|--------|
| `nyuu` (Node) | 16.37s | 30.5 MiB/s | 54.8 MiB |
| `pesto` (Rust) | 16.23s | 30.8 MiB/s | 36.4 MiB |
| `ngPost` (C++) | FAILED | - | - |

## Analysis

In real-world conditions (30ms latency), the network becomes the bottleneck and `pesto` achieves parity with the competition. However, in raw throughput (0ms latency), `pesto` is bottlenecked by single-thread CPU execution.

The primary reason for this gap is that `pesto` computes the CRC32 checksum and slices the articles sequentially inside the `producer`'s main thread loop. 

## Path to Optimization

To close the remaining gap and match or exceed `ngPost`/`nyuu` in 0ms scenarios, we need to offload the CPU-bound work from the producer loop:
1. **Parallelize CRC32 computation:** Offload checksum calculation and article partitioning to the async worker threads or a dedicated CPU pool, rather than blocking the main `producer` loop.
2. **Buffer management:** Evaluate if the zero-copy/buffer pooling strategy can be further tightened for small files without breaking the architecture built for multi-GB files.
