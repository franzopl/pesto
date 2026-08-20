### Memory Fragmentation & PAR2 `many-small` Bottleneck

Following up on the `many-small` workload investigation, further profiling has revealed why `pesto` consumes **~710 MiB RSS** compared to `parpar`'s **~168 MiB** during the PAR2 generation phase, contributing significantly to the slow speeds.

The memory bloat stems from how `pesto`'s ingestion loop interacts with `parmesan`'s `Par2Worker` channels and Tokio's buffer pool:

1. **Slice Padding Explosion**: In `pesto`'s `producer`, each file is fed manually via `feed_par2_slice` with `is_last_of_file = true`. For 250 KiB files, this pads each file up to the `par2_slice_size` (e.g., 750 KiB) with zeros, inflating the 500 MiB payload into ~1.5 GiB of RAM pressure passed to the mathematical encoder.
2. **Buffer Pool Starvation & Drops**: The `RecoveryEncoder` processes and flushes 128 slices at a time back to the `Par2Worker`'s `free_tx` return channel. However, `free_tx` is a `sync_channel` with a fixed depth of `64`. As a result, half of the recycled buffers (64 slices) are silently dropped on every flush.
3. **RSS Fragmentation**: Because the buffers are constantly dropped by the worker, `pesto`'s producer (`try_take_buffer`) finds the pool empty and is forced to continuously allocate new memory from the OS. This rapid "allocate/drop" cycle of giant blocks severely fragments the `glibc malloc` heap, spiking RSS up to 700+ MiB even though the theoretical working set is under 200 MiB.
4. **Dropped Global Buffers**: In `full-two-phase` mode (`defer_posting = true`), the main article buffers (`buf`) acquired from the `Shared::pool` are bypassed and dropped without ever hitting the network worker's `release_buffer`. This means the global Tokio pool also suffers continuous allocations without re-use.

#### Recommendation
We need to abandon the custom manual file-by-file ingestion loop in `pesto`'s `producer` for PAR2 generation. Instead, `pesto` should delegate to `parmesan`'s optimized `ops::ingest_files` routine (introduced in `parmesan` PR #131), which packs small files together, skips the async overhead, and correctly avoids exploding memory boundaries.
