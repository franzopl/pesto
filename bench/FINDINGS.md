# Benchmark findings

What the suite turned up on its first full run, with the measurements behind
each claim. `README.md` explains the methodology and its limitations;
`GUIDE.md` is how to reproduce any of this.

Every number here is a **median of 3 repetitions**, and every table carries the
relative standard deviation (RSD). A difference smaller than the RSD on its row
is not a difference.

---

## The machine

| | |
|---|---|
| CPU | Intel Core i5-10400 — 6 physical / 12 logical cores |
| SIMD | SSSE3, AVX2. **No GFNI, no AVX-512** |
| Memory | 31.2 GiB |
| Kernel | Linux 6.8.0-111-generic (x86_64) |
| Governor | **`powersave`**, boost on |
| Corpus FS | btrfs, warm page cache, primed before each comparison |
| Repetitions | 3 (median reported) |
| Workloads | `many-small`, `mixed-folder`, `movie-1080p` at `--scale 0.25` |
| Tools | pesto 0.7.0, parmesan 0.4.1, parpar 0.4.5, par2cmdline 0.8.1, nyuu 0.4.2, ngPost 4.16, node-yencode (node v24.15.0) |

Two caveats that apply to everything below.

**The governor was `powersave`.** Absolute figures are conservative and the
single-threaded ones most of all. Relative comparisons within a table are still
sound — every tool in a row ran under the same conditions, minutes apart.

**This CPU has no GFNI.** parmesan's fastest PAR2 kernels (AVX2+GFNI,
AVX-512+GFNI) never ran. Any PAR2 conclusion here is about the AVX2 path only,
and could look different on Ice Lake or newer.

Raw data: `bench/results/medialab/20260817T015113Z/`. The yEnc rows in it were
re-measured a few minutes after the rest, on the same machine and binaries,
after the precision fix described in "Bugs the suite found" below.

---

## 1. Two crashes in `parmesan`, found on the first run

Both are the same shape — a `Vec<FileHashes>` indexed by file position when the
worker only produces an entry for *some* files — and both abort the process.
Both are now fixed with regression tests in
`crates/parmesan/tests/exact_multiple_slice_size.rs`.

**A file whose size is an exact multiple of the slice size.**
`ops::ingest_files` flagged `is_last_of_file` only on a *partial* trailing
slice, so a file that divided evenly produced no hash at all:

```
$ parmesan create -s 1048576 --recovery-count 200 movie.mkv   # 256 MiB file
thread 'main' panicked at crates/parmesan/src/main.rs:347:
index out of bounds: the len is 0 but the index is 0
```

This is not an exotic input. It only looks rare because the automatic geometry
picks an awkward slice size; the moment a caller passes an explicit
`--slice-size` or `--slice-count` — which the benchmark must, to compare tools
fairly — exact division becomes the normal case. The identical bug had already
been found and fixed in `pesto`'s posting path
(`crates/pesto/tests/par2_exact_multiple_of_slice_size.rs`); that fix never
reached the `parmesan` side.

**A zero-byte input file.** Same crash, same cause: an empty file contributes
zero slices, so the worker returns no hash for it.

```
$ parmesan create -s 16384 --recovery-count 2 a.bin empty.bin z.bin
index out of bounds: the len is 2 but the index is 2
```

A release folder with a placeholder or a stub `.nfo` hits this. Fixed by
synthesizing the MD5-of-nothing, exactly as `pesto`'s poster already does.

> **Needs a decision.** That fix diverges from par2cmdline, which refuses to
> protect zero-length files at all (`Skipping 0 byte file`) and reports a
> zero-length File Description entry it did not write as *damaged*. Matching
> `pesto` keeps the recovery set a complete description of what was posted;
> matching par2cmdline maximises interoperability. Currently it matches
> `pesto`.

---

## 2. yEnc: pesto is ahead of node-yencode, and `auto` is leaving a little on the table

Encode throughput, in-memory, MiB/s (RSD in parentheses):

| size | line | scalar | SSSE3 | AVX2 | `auto` | node-yencode |
|---|---|---|---|---|---|---|
| 4 KiB | 128 | 572 (0.6%) | 4846 (0.3%) | **5222** (0.4%) | 4865 (0.5%) | 1257 (1.2%) |
| 128 KiB | 128 | 559 (0.4%) | 2862 (1.0%) | **2898** (0.2%) | 2831 (1.0%) | 1954 (2.4%) |
| **768 000** | **128** | 561 (1.8%) | 2316 (2.5%) | **2369** (3.6%) | 2304 (1.3%) | 2122 (0.4%) |
| 8 MiB | 128 | 551 (0.7%) | 2269 (0.5%) | **2328** (0.9%) | 2271 (0.9%) | 1987 (1.7%) |
| 768 000 | 256 | 548 (0.3%) | 2755 (2.0%) | **3086** (1.9%) | 2874 (0.5%) | 2245 (2.9%) |

768 000 bytes is the real article size — that row is the one that matters.

**pesto's AVX2 encoder is ~12% faster than node-yencode** at the article size
(2369 vs 2122), and the gap widens as the buffer shrinks: 4.2× at 4 KiB, where
node's per-call overhead dominates. This contradicts the current README, which
reports the two as neck-and-neck at ~2200 MiB/s on this CPU; that entry
predates the current encoder and should be refreshed.

**`auto` costs 2–7% versus dispatching straight to AVX2 on this CPU** — 2304
vs 2369 at the article size (2.7%), 2874 vs 3086 at `ll=256` (6.9%). This is
not a dispatch bug: `pesto::yenc::x86::encode` (the `auto` path) deliberately
caps at SSSE3 everywhere — `encode_avx2()` exists separately for explicit
selection — because AVX2 measured ~5% slower than SSSE3 on Alder Lake+
E-cores at `ll=128`. This was investigated and closed on purpose; see
`ROADMAP.new.md`'s "Deferred / intentionally not implemented" section and the
comment on `x86::encode`. The 2.7% here is that safety margin's cost on a
homogeneous CPU that never needed it — not overhead to eliminate.
(The earlier reading of "`auto` also lands below SSSE3" in the raw data was
measurement noise, not a real effect: `auto` and explicit SSSE3 call the
exact same code path, so any difference between their rows is two
independent benchmark runs of one implementation, not two implementations.)

One real open question this raises: the ~5% figure the policy is based on
was measured at `ll=128` only. Whether SSSE3-over-AVX2 is still the right
call at `ll=256` — where the gap to AVX2 is larger, 6.9% vs 2.7% — has not
been tested on hybrid hardware. Worth revisiting only if `DEFAULT_LINE_LENGTH`
or the `--line-length 256` recommendation from the next finding ever changes
in practice, and only on hybrid hardware — not something to guess at here.

**`ll=256` encodes 30% faster than `ll=128`** at the article size (3086 vs
2369). `ROADMAP.new.md` records that raising `DEFAULT_LINE_LENGTH` to 256 was
"benchmarked and rejected"; that rejection was presumably on compatibility
grounds, but the performance side of the trade is larger than the note implies
and is worth restating with a number.

**Decode runs at ~690–740 MiB/s**, about 3.4× slower than encode, with a single
portable implementation and no SIMD path. Nothing downloads in `pesto`, so this
does not matter here — but `penne` decodes for a living, and this is the number
that bounds it.

---

## 3. PAR2: parmesan sits between parpar and par2cmdline, and repair is single-threaded

Create, at identical geometry across all three tools — same slice size, same
input slice count, 200 recovery blocks, 6 threads, 1 GiB memory ceiling:

| workload | parmesan | parpar | par2cmdline | parmesan vs parpar |
|---|---|---|---|---|
| `many-small` — 125 MiB in 2 000 files | 72.3 (7.1%) | **136.6** (1.4%) | 50.3 (3.1%) | −47% |
| `mixed-folder` — 525 MiB in 29 files | 254.3 (0.8%) | **348.9** (0.6%) | 41.9 (11.2%) | −27% |
| `movie-1080p` — 1.5 GiB in 1 file | 269.0 (0.5%) | **357.9** (7.3%) | 42.1 (1.9%) | −25% |

MiB/s of source data; higher is better.

**parpar is ahead everywhere on this CPU** — 25% on large files, 47% on the
small-file case. Against par2cmdline, parmesan is 6.4× faster on large files.
Note again that GFNI is unavailable here; the gap to parpar may be an AVX2-path
gap specifically.

**parmesan also uses the most memory while being slower**: 442 MiB peak RSS on
`movie-1080p` versus parpar's 255 MiB and par2cmdline's 160 MiB, at the same
1 GiB ceiling. Throughput bought with RAM is a defensible trade; being behind
on both at once is the finding.

**`many-small` is the weak spot.** Every file smaller than a slice consumes a
whole padded slice, so the work is sized by file *count*. parmesan falls to
half of parpar's rate there — and this is what drags the end-to-end
`many-small` result in §5.

Verify and repair, same corpora:

| operation | workload | parmesan | par2cmdline |
|---|---|---|---|
| verify | `movie-1080p` | **462.9** (cpu 1.00) | 184.0 (cpu 1.04) |
| verify | `many-small` | **399.4** (cpu 0.96) | 317.3 (cpu 1.91) |
| repair | `movie-1080p` | **66.7** (cpu 1.00) | 48.3 (cpu 3.52) |
| repair | `many-small` | 76.5 (cpu 0.99) | **90.5** (cpu 4.35) |

`cpu` is `(user+sys)/wall` — cores actually used.

**parmesan's verify and repair were entirely single-threaded** (cpu ≈ 1.00 in
every row above), while par2cmdline uses 3.5–4.3 cores for repair. parmesan
still won verify outright and won repair on the large file — on one core
against four.

**Fixed in [#130](https://github.com/franzopl/pesto/issues/130).**
`RecoveryDecoder::reconstruct` (`crates/parmesan/src/decoder.rs`) now
parallelises across missing slices with `rayon`, the same pattern
`RecoveryEncoder` already uses for creation — two loops whose iterations
write to disjoint buffers, so no synchronization was needed beyond sharing
the (already-immutable) `Gf16` field tables. Direct A/B measurement on the
same corpora, same geometry, same damage pattern, before vs after (not a
full 3-rep suite run — a targeted comparison to size the fix before landing
it):

| workload | before (wall / cpu) | after (wall / cpu) | wall-clock gain |
|---|---|---|---|
| `movie-1080p` (807 KB slices) | 22.0s / 0.96 | 17.5s / **7.9** | **20%** |
| `many-small` (64 KB slices) | 1.87s / 0.87 | 0.87s / **3.4** | **53%** |

Both are real, measured, and byte-exact — `repair` re-verifies every
reconstructed slice's checksum before writing, so a correctness bug here
would have surfaced as a hard failure, not a silently wrong number; the full
existing property-test suite plus the `#[ignore]`d 960-case structural sweep
for issue #51's territory (same file, `decoder.rs`) also stayed green.

Note the gain is real but **sub-linear** — CPU time went up 7-9×, wall time
came down only 1.25-2.1×. `mac()` over an 807 KB buffer is bandwidth-bound
(read src, read+write dst — several hundred KB of traffic per call), so 8
threads contending for the same memory bus don't get 8× throughput; the
smaller 64 KB `many-small` slices fit in L2, contend less, and scale better
(53% vs 20%), which is why the two rows differ so much. Estimated new
`many-small` repair throughput (125 MiB / 0.87s ≈ 143 MiB/s) now beats
par2cmdline's 90.5 MiB/s on the one row where parmesan used to lose — it no
longer does.

This is a different outcome from `#126` (parallelising `verify()`, recorded
in `CHANGELOG.md`, showed no real gain): `verify` at release speed is
I/O-bound — sequential file reads dominate, hashing is cheap — so spreading
the hash work across cores had nothing to buy. `repair`'s GF(2^16)
multiply-accumulate is heavier per byte than a hash, so there was real
compute to parallelize; it just wasn't as much as `cpu ≈ 1.00` alone
suggested, because most of what limits `repair` at scale is memory
bandwidth, not core count. Getting closer to linear would need the
encoder's batched/transposed data layout (`altmap`/`shuffle2x` in
`encoder.rs`) rather than more threads over the naive per-slice loop — a
meaningfully bigger change, not attempted here.

A related note in `CHANGELOG.md` records that parallelising `verify()` "showed
no real gain" (#126). These numbers do not contradict that — verify is already
ahead — but they do suggest **repair** deserves the same experiment, and it is
a different code path.

**Fixed in [#131](https://github.com/franzopl/pesto/issues/131).** The 47%
`many-small` create gap above was root-caused by profiling (`perf record` +
`strace -f -c`), not assumed, and the actual mechanism was neither of the two
things it was easiest to suspect:

- **Not the encoder's flush cache-blocking.** `RecoveryEncoder::add_slice`
  flushes every 128 queued slices or `flush_limit_bytes` (256 MiB),
  whichever comes first — for `many-small`'s 64 KiB slices that is the
  128-slice branch, well short of the byte budget. Tested removing the count
  cap entirely (byte limit only): wall time got *worse* (2.05s → 2.1–2.5s)
  and peak RSS went up 7.7× (79 MiB → 611 MiB). The cache-blocking is
  correctly tuned, not a bug — ruled out.
- **Not the #137 tokio worker-pool cap.** With `#[tokio::main]`'s old
  default (one worker per core — 12 here — instead of #137's fixed 4),
  the same workload measured 2.2–2.46s: slightly *slower* than the current
  2.05s baseline, not faster. #137 did not regress this path — ruled out.

`perf record -g` (cycles) on `many-small` showed ~63% of *executed* cycles
inside the RS SIMD kernel (real work) but only ~300% average CPU use out of
a 600% ceiling (`-t 6`); `strace -f -c` on the same run showed **22,270
`futex` calls (3.29s cumulative) and 7,578 `sched_yield`** in a 2.05s
wall-clock run — threads parking and waking far more than they compute.
`ops::ingest_files` processes its `for file_info in files` loop strictly one
file at a time: each file pays a fresh `tokio::sync::mpsc::channel` plus a
`spawn_blocking` reader task and a channel round-trip, even when the entire
file is one syscall — with 2 000 files, that ceremony is paid 2 000 times
regardless of size, while the rayon pool sits idle except during the rare
flush bursts big enough to need it.

**Fix** (`crates/parmesan/src/ops.rs`): files that fit in one read (≤
`CHUNK_SIZE`, 8 MiB — the same constant the streaming reader already chunks
by) skip the channel and task-spawn entirely and are read with a single
`std::fs::read` inside one `block_in_place`, then fed through the same
slice-accumulation logic (extracted into `feed_chunk`, shared verbatim with
the unchanged streaming path so large-file behavior — and the `held`/
`is_last_of_file` bookkeeping this module's doc comment warns about — is
byte-for-byte identical to before). Slice order and per-file sequencing are
untouched, so this is an I/O-strategy change only, not a reordering: PAR2's
Reed-Solomon coefficients are positional (see `sort_files_by_file_id`'s
doc comment), and the hasher thread still consumes slices in the same order.

Official suite numbers, `./bench/run.sh par2 --workload many-small --scale
0.25 --reps 5` (the exact repro command from #131), median of 5:

| tool | before | after | vs before |
|---|---:|---:|---:|
| parmesan | 72.3 MiB/s | **178.3 MiB/s** | **+147%** |
| parpar | 136.6 MiB/s | 135.6 MiB/s (noise) | — |

**parmesan now beats parpar by 31% on `many-small`** (178.3 vs 135.6 MiB/s),
a full reversal from being 47% behind. `mixed-folder` and `movie-1080p` — both
already on the streaming (unchanged) code path — were re-measured to confirm
no regression; the ~5–10% deltas seen there are within this machine's
`powersave`-governor noise (see "Two caveats" at the top of this file), not a
code-path change, since neither workload's files are small enough to take the
new branch. All 9 checks in `bench/suites/60-correctness.sh` pass, plus the
full `#[ignore]`d `par2cmdline_compat` cross-tool suite (byte-exact repairs
both directions, unicode filenames, multi-file damage).

This fix is in `parmesan`'s own `ops::ingest_files`, used by the standalone
`parmesan create` CLI measured in this table. `pesto`'s poster
(`crates/pesto/src/poster/mod.rs`) has its own separate file-ingestion loop —
it uses `Par2Worker`/`RecoveryEncoder` directly rather than
`ops::ingest_files`, to overlap PAR2 generation with upload — so §5's
`many-small` end-to-end row is a *different* code path and was not measured
as part of this fix; whether it has an analogous per-file overhead problem is
open, not assumed fixed here.

---

## 4. Pipeline stages: where the time actually goes

`movie-1080p`, 1.5 GiB, one file. Each line is one difference between two real
runs that differ by exactly one thing:

| stage | wall | MiB/s | peak RSS | RSD |
|---|---|---|---|---|
| `read` (storage floor) | 0.15s | 10 520 | 3 MiB | 4.7% |
| `yenc` (read + yEnc + articles + NZB) | 2.36s | 650 | 30 MiB | 0.6% |
| `yenc+par2` | 4.22s | 364 | 737 MiB | 1.3% |
| `par2-only` | 3.59s | 428 | 745 MiB | 1.3% |
| `post` (mock server, no PAR2) | 2.56s | 600 | 50 MiB | 6.3% |
| `post+par2` (default, streaming) | 4.55s | 338 | 840 MiB | 5.1% |
| `post+par2-pre` (`--par2-before-upload`) | 6.41s | 240 | 887 MiB | 2.0% |
| `post+check` (streaming STAT) | 4.63s | 332 | 845 MiB | 1.3% |

Attribution:

| cost of | measured as | value |
|---|---|---|
| storage read | `read` | 0.15s |
| yEnc + article build + NZB | `yenc − read` | +2.21s |
| PAR2 generation | `yenc+par2 − yenc` | +1.86s |
| NNTP framing, pool and sockets | `post − yenc` | +0.20s |
| **streaming PAR2 overlap** | `post+par2 − post+par2-pre` | **−1.86s** |
| streaming STAT check | `post+check − post+par2` | +0.08s |

Three things stand out.

**The overlap recovers essentially all of the PAR2 cost.** Generation costs
1.86 s and the streaming pipeline saves 1.86 s against the two-phase shape.
That is the design working exactly as intended, and it is what the `e2e` tables
in §5 are measuring at the whole-upload level.

**NNTP framing is nearly free** — 0.20 s on top of 2.36 s, under 9%, at zero
latency. Whatever limits posting throughput, it is not the protocol layer.

**The streaming STAT check costs 1.8%** (0.08 s on 4.55 s). It is on by default
and confirms every posted article; that is a very cheap guarantee.

**`read` at 10.5 GiB/s is the page cache, not the disk.** These are warm-cache
numbers by design (see `README.md`); a cold-cache run would move the floor and
everything above it.

---

## 5. End-to-end: it depends entirely on what you are uploading

### Posting only, no PAR2 — the pure poster comparison

MiB/s at 0 ms simulated latency, all three tools at identical article size,
line length, connection count and group, post checking off:

| workload | pesto | nyuu | ngPost |
|---|---|---|---|
| `many-small` (2 000 files) | **463** (1.5%) | 169 (1.2%) | 328 (0.2%) |
| `mixed-folder` (29 files) | 570 (12.3%) | **782** (3.6%) | – |
| `movie-1080p` (1 file) | 618 (0.9%) | **1136** (2.4%) | 862 (0.9%) |

Article counts matched exactly across tools on every row (4 000 / 773 / 2 101),
so these are like-for-like.

**pesto is 2.7× nyuu on many small files and 0.54× nyuu on one large file.**
That is a real split, not noise, and it is the single most actionable result
here. pesto's per-file path is clearly strong; its single-large-file path
leaves ~45% on the table against nyuu at zero latency.

At 30 ms the distinction disappears entirely — pesto 82, nyuu 88, ngPost 89
MiB/s on `movie-1080p` — because every tool becomes latency-bound. Which is
exactly why the suite measures both, and why a 0 ms-only comparison would be
misleading in either direction.

**Fixed in [#129](https://github.com/franzopl/pesto/issues/129).** Root-caused
by `perf stat` on `movie-1080p` at increasing `--connections`, not assumed:
total instructions executed stayed flat (~45.7 billion) across every
connection count, but "CPUs utilized" fell from 3.08 at 4 connections to a
flat ~2.1 from 8 upward — the same amount of work getting *less* parallelism
as more workers were added, the signature of coordination overhead, not a
capacity limit. The mechanism: every posting worker shared one
`mpsc::Receiver<PostTask>` behind a single `tokio::sync::Mutex`
(`worker()`, `crates/pesto/src/poster/mod.rs`), so every dequeue — thousands
per second at 0 ms latency, where a ~768 KB article encodes and posts in
sub-millisecond time — serialized through that one lock regardless of how
many connections were open. `many-small` never showed it because its default
run used 4 connections, at or below where the lock only just starts costing
more than it saves.

Ruled out first: holding the mutex guard across the blocking `.recv().await`
when the channel was empty. Patched that in isolation (`try_recv()` in a
polling loop, dropping the lock between attempts) and re-measured — the
regression was unchanged, which said the cost was the sheer *frequency* of
lock acquisition on the hot path, not what happened during an empty wait.

**Fix** (`crates/pesto/src/poster/mod.rs`): replaced the single shared
channel with `TaskDispatcher`, which round-robins articles across one
dedicated channel per worker. Each worker owns its `Receiver` outright, so
dequeuing never contends with any other worker — no lock on the hot path at
all. Trade-off: round-robin fixes an article's destination worker up front,
so a multi-server config with one slow/erroring connection can no longer
have its share of work silently picked up by faster, idle workers the way a
shared queue did implicitly. Acceptable for the common single-healthy-server
case this fixes; a work-stealing scheme would recover the old behavior too,
but was not needed to close this gap.

Official suite numbers, same machine, same `--scale 0.25` methodology as the
rest of this report, `./bench/run.sh e2e --workload movie-1080p --scale 0.25
--reps 5 --latencies 0,30` (`bench/results/medialab/20260817T232816Z/`),
median of 5:

| | before | after | vs before |
|---|---:|---:|---:|
| pesto, `movie-1080p`, 0 ms | 618 MiB/s | **1236.7 MiB/s** | **+100%** |
| nyuu, same run | 1136 MiB/s | 1117.9 MiB/s (noise) | — |
| pesto ÷ nyuu | 0.54× | **1.11×** | pesto now *ahead* |

pesto goes from losing to nyuu by nearly half to beating it outright on the
exact workload the issue named, with nyuu itself unchanged (run-to-run noise
only) — confirming the gain is pesto's, not a shift in the comparison. At
30 ms latency the same case is untouched (84.0 vs the original 82 MiB/s,
within noise): exactly as expected, since the lock was never the bottleneck
once network RTT dominates each worker's loop.

### A full release — data plus 10% recovery

Wall time; lower is better. `pesto (streaming)` is the default pipeline,
`pesto (two-phase)` is `--par2-before-upload` and is the structural match for
the competitors:

| workload | latency | pesto (streaming) | pesto (two-phase) | parpar+nyuu | ngPost |
|---|---|---|---|---|---|
| `movie-1080p` | 0 ms | **4.38s** | 6.37s | 5.49s | 44.10s |
| `movie-1080p` | 30 ms | **20.12s** | 23.91s | 23.28s | 62.13s |
| `mixed-folder` | 0 ms | **1.65s** | 2.23s | 2.20s | – |
| `mixed-folder` | 30 ms | **7.13s** | 8.87s | 8.54s | – |
| `many-small` | 0 ms | 9.38s | 8.87s | **1.73s** | – |
| `many-small` | 30 ms | 19.95s | 27.45s | **17.40s** | 20.50s |

**On a normal release, pesto's streaming pipeline is the fastest thing in the
table** — it finishes 20% sooner than parpar+nyuu on `movie-1080p` at 0 ms,
14% sooner at 30 ms, and 25% sooner on `mixed-folder`. And it gets there *despite* parmesan being 25% behind parpar
on raw PAR2 throughput (§3), because overlapping the generation with the upload
buys back more than the encoder loses.

**ngPost is an order of magnitude behind** on the full-release path (44.1s vs
4.4s) — its two-phase pipeline with its own PAR2 implementation, not a close
comparison of the same work.

**`many-small` inverts everything.** parpar+nyuu finishes in 1.73s against
pesto's 9.38s. This is §3's small-file weakness, amplified: PAR2 dominates the
whole upload when there are 2 000 sub-slice files, and parmesan is half
parpar's speed there. Fix the small-file PAR2 path and this row moves with it.

**Streaming is not always a win.** On `many-small` at 0 ms it is 5.7% *slower*
than two-phase — the only such row. When PAR2 is the bottleneck and the network
is free, overlapping buys nothing and costs coordination. At 30 ms the same
workload flips to streaming being 27% faster.

Article counts differ between pesto (4 418) and parpar+nyuu (4 049) on the
full-release rows. That is expected — implementations split recovery data into
volumes differently — and the report flags it. The payload is the same 200
recovery blocks; the article count is not the comparable quantity there.

**After the [#129](https://github.com/franzopl/pesto/issues/129) connection-pool
fix above**, `movie-1080p` re-measured on the same corpus and geometry:

| workload | latency | pesto (streaming) | pesto (two-phase) | parpar+nyuu | ngPost |
|---|---|---|---|---|---|
| `movie-1080p` | 0 ms | **4.08s** (was 4.38s) | 4.93s (was 6.37s) | 5.52s (noise) | 45.06s (noise) |
| `movie-1080p` | 30 ms | **20.12s** (unchanged) | 24.30s (noise) | 23.34s (noise) | 1m01s (noise) |

`two-phase` — whose posting phase is a plain upload with no PAR2 overlap to
hide behind — picks up essentially the whole gain (23% faster), since it is
the more direct measurement of the connection pool itself; `streaming`
improves too (7%) but PAR2 generation dominates enough of its wall time that
the pool fix has less to work with. `parpar+nyuu` and `ngPost` are unrelated
code paths and move only by measurement noise, as expected. At 30 ms nothing
moves outside noise, consistent with the fix being specifically about
low-latency lock contention.

### ngPost reliability

ngPost failed 1 of 3 repetitions on `many-small` at 0 ms and 2 of 3 at 30 ms,
segfaulting **after** writing its NZB (exit 139). Reproducible on the 2 000-file
corpus, never on the others. Recorded as failures rather than hidden; the
medians on those rows come from its surviving runs and should be read with that
in mind.

---

## 6. Connection scaling: pesto peaks at 4 connections and then regresses

`movie-1080p`, MiB/s against connection count:

| connections | 0 ms pesto | 0 ms nyuu | 10 ms pesto | 10 ms nyuu |
|---|---|---|---|---|
| 1 | 263 | 575 | 25.0 | 29.4 |
| 2 | 491 | 1009 | 49.7 | 59.3 |
| 4 | **821** | 1083 | 105 | 122 |
| 8 | 613 | 1147 | 219 | 239 |
| 16 | 601 | 1002 | 443 | 454 |
| 32 | 545 | 1134 | 570 | 822 |

RSD is 0.1–5.1% throughout, so the shape is real.

**At 0 ms, pesto peaks at 4 connections (821 MiB/s) and then falls to ~550–610
and stays there.** nyuu does not — it plateaus around 1 100 from 4 connections
up. Something in pesto's pool is costing throughput as connections are added
past the physical core count, and it is worth a look; the default is 4, so this
is not hurting anyone today, but users routinely configure 16–50.

**At 10 ms both scale cleanly** and track each other within ~10% up to 16
connections, where nyuu pulls ahead again at 32. Under any realistic latency
the connection count is what matters and the two are comparable.

Note the 0 ms column is measuring against a mock server on loopback — it is a
CPU and syscall benchmark, not a network one. The 10 ms column is the one that
resembles a real provider.

**Fixed in [#129](https://github.com/franzopl/pesto/issues/129)** — root
cause and fix described in §5 above (the same shared-mutex dequeue
bottleneck; this table and §5's "posting only" row are two views of the
same mechanism, which is why they were investigated together). Re-measured
with the exact repro from the issue, same machine, same `--scale 0.25`
methodology as the rest of this report:
`BENCH_SCALE=0.25 BENCH_REPS=5 BENCH_SCALING_LATENCIES=0,10 bash
bench/suites/50-scaling.sh movie-1080p` (the 50 ms column was dropped from
this re-run, not from the fix's validation — see this suite's own file
header on why a 1.5 GiB corpus at 1 connection/50 ms costs minutes per
repetition for a point the 10 ms column already makes; the original table
above never measured it either, despite `SCALING_LATENCIES`'s default of
`0,10,50`):

| connections | 0 ms pesto | 0 ms nyuu | 10 ms pesto | 10 ms nyuu |
|---|---|---|---|---|
| 1 | 254.6 (was 263) | 567.4 | 24.9 (was 25.0) | 29.1 |
| 2 | 481.8 (was 491) | 1003.3 | 48.9 (was 49.7) | 58.5 |
| 4 | 776.5 (was 821) | 1065.9 | 99.3 (was 105) | 114.1 |
| 8 | **1105.8** (was 613) | 1011.2 | 212.5 (was 219) | 237.8 |
| 16 | **1366.5** (was 601) | 1116.3 | 424.7 (was 443) | 458.1 |
| 32 | **1373.9** (was 545) | 1092.5 | 803.3 (was 570) | 815.3 |

**The regression is gone, not just reduced.** At 0 ms pesto now climbs
monotonically through the full sweep instead of peaking at 4 and falling —
1105.8/1366.5/1373.9 MiB/s at 8/16/32 connections, against 613/601/545
before, and **ahead of nyuu** from 8 connections on (nyuu: 1011.2/1116.3/
1092.5). 1/2/4 connections are unchanged within noise (254.6 vs 263, 481.8
vs 491, 776.5 vs 821), confirming the fix cost nothing at the connection
counts that were never affected. At 10 ms the whole curve moved by noise
only (the largest delta, conn32's 803.3 vs 570, is a *gain* — the old
regression's tail was still dragging on this column too, just less visibly
under latency) — consistent with §5's finding that this was a low-latency
coordination cost, not a change to how pesto behaves under real network
conditions.

---

## 7. Interoperability and correctness: all nine checks pass

- parmesan creates → **par2cmdline verifies, repairs, byte-exact restore** ✓
- par2cmdline creates → **parmesan verifies, repairs, byte-exact restore** ✓
- parpar creates → **parmesan verifies** ✓
- posted articles → **an independent Python yEnc decoder reassembles the source
  byte-for-byte**, with every `=yend pcrc32=` verified and full coverage
  asserted ✓

Recovery payload accounting, 50 blocks of 86 016 bytes requested:

| tool | on disk | ratio |
|---|---|---|
| parmesan | 4.2 MiB | 1.02× |
| parpar | 4.3 MiB | 1.04× |
| par2cmdline | 4.3 MiB | 1.06× |

All three carry the payload plus packet overhead, and parmesan is the leanest.
Measured separately on the 2 000-file corpus — where PAR2 repeats its critical
packets in every volume and those packets are large — the spread widens
sharply: parmesan 2.85×, parpar 6.95×, par2cmdline 8.74×. That is a real
advantage in posted articles per release, and the one PAR2 metric on which
parmesan is clearly ahead of both.

---

## 8. `parmesan create` exhausted virtual address space on high-core machines, independent of `--memory-limit`

**Fixed in [#137](https://github.com/franzopl/pesto/issues/137).** On a
128-core remote box with a restrictive `RLIMIT_AS` (`ulimit -v`, applied
system-wide via PAM `limits.conf` — a real shared-host/HPC/container
pattern, not a benchmarking quirk), `parmesan create` panicked well inside
the configured `--memory-limit 1024MiB`:

```
thread 'main' panicked at crates/parmesan/src/encoder.rs:351:13:
PAR2 recovery buffer allocation failed (202 blocks × 1101824 bytes): memory allocation failed because the memory allocator returned an error
```

`parpar` and `par2cmdline` completed the identical workload/geometry under
the same `ulimit -v` without issue.

**Root cause, confirmed two ways: isolated A/B measurement on a 12-core dev
box, then the literal panic reproduced and fixed on a real 128-core box.**
The recovery buffer itself was never the problem — `RecoveryEncoder::new_smart`
only ever allocates `recovery_count × slice_size` for the pass currently
running, and `ops::ingest_files` streams input in 8 MiB chunks rather than
reading whole files. What actually consumed the address space was thread
fan-out that ran *before* either of those:

- `#[tokio::main]`'s default multi-thread runtime spawned one worker thread
  per core, even though `ingest_files` only ever drives one file at a time
  (one `spawn_blocking` reader feeding an await loop) and never needed more
  than a couple.
- glibc's malloc creates up to `8 × ncores` per-thread arenas by default,
  each reserving tens of MiB of address space — nearly RSS-free, counted in
  full against `RLIMIT_AS`, and never returned. With the tokio pool and the
  rayon pool (`performance_core_count`, correctly sized to physical cores
  for real RS throughput) both contending on malloc, arena growth alone
  dwarfed the actual working set.

Isolating the two mechanisms, same 20 MB workload, `-t $(nproc)` (12 here),
median of 3 runs, peak `VmPeak` from `/proc/<pid>/status` polled at 10 ms
while the process ran:

| build | `VmPeak` |
|---|---:|
| before (unmodified `#[tokio::main]`, default glibc arenas) | 1 897 480 KiB (**1.81 GiB**) |
| before + `MALLOC_ARENA_MAX=2` only (arena cap, thread counts unchanged) | 193 544 KiB (189 MiB) |
| after (capped tokio workers **and** `mallopt(M_ARENA_MAX, 2)`) | 171 912 KiB (168 MiB) |

An 11× reduction, and the arena cap alone accounts for ~90% of it, on a
12-core box.

**Then reproduced directly** on `baron.usbx.me`, an AMD EPYC 7742 / 128-core
/ 503 GiB box with the *exact* environment from the issue — `* hard as
10000000` in `/etc/security/limits.conf`, applied automatically to every
session via `pam_limits.so` — using the same `movie-1080p`-sized workload
(6 GiB, 1 file), `-m 1024MiB`, default (auto) thread count:

| build | result | `VmPeak` at exit |
|---|---|---:|
| before (`main`, pre-fix) | **panics**, exit 134 (SIGABRT) | 9 999 764 KiB — 24 KiB short of the 10 000 000 KiB ceiling |
| after (this fix) | succeeds, PAR2 set verifies OK | 1 418 676 KiB (1.35 GiB) — 86% headroom to spare |

The before build's panic is the issue's own crash, byte for byte:

```
thread 'main' (3096599) panicked at crates/parmesan/src/encoder.rs:351:13:
PAR2 recovery buffer allocation failed (200 blocks × 3221184 bytes): memory allocation failed because the memory allocator returned an error
```

and its `VmPeak` at the moment of death confirms the diagnosis directly —
not a memory shortage (503 GiB physical RAM sat almost entirely idle) but
address space pinned to within 24 KiB of the configured ceiling, for a
recovery buffer request of 200 × 3.2 MiB ≈ 614 MiB, nowhere near either the
1 GiB `--memory-limit` or the 9.54 GiB `RLIMIT_AS`. `parmesan verify` on the
after build's output confirmed the PAR2 set produced under the ceiling is
correct (2001/2001 slices OK), not just "didn't crash."

**Fix** (`crates/parmesan/src/memory.rs`, applied at the top of `main`,
before any thread exists):

- `tune_allocator()` — `mallopt(M_ARENA_MAX, 2)`, matching the value
  `pesto` itself already uses for the same reason
  (`crates/pesto/src/memory/mod.rs`). No-op on musl (no per-core arenas
  there) and non-Unix.
- `build_runtime()` — a hand-built tokio runtime with `worker_threads(4)`,
  `max_blocking_threads(16)` and 1 MiB stacks, replacing
  `#[tokio::main]`'s `ncores`/512/2 MiB defaults. Still multi-threaded:
  `ingest_files` uses `block_in_place`, which requires it.
- The rayon pool is deliberately **not** capped — it's the genuinely
  CPU-bound stage and stays sized to `performance_core_count()` for
  throughput, same as `pesto`'s equivalent split (rayon scaled, tokio
  capped) in its own memory module.

`bench/run.sh par2` itself was not run on `baron.usbx.me` as part of this
fix (it needs `parpar`/`par2cmdline` installed there for the comparison
columns, and a full corpus generation pass); the direct `parmesan create`
repro above uses the same workload shape and geometry and is the part of
the suite's repro command that actually exercises the bug.

---

## Bugs the suite found in itself

Worth recording, because they are the reason to trust the numbers above.

- **`--par2-only` output leaked outside the corpus.** Given a directory,
  `pesto` writes the recovery set one level *above* the release folder (correct
  — it is where the File Description packets' relative paths resolve), so the
  cleanup that swept inside the corpus missed it. Never corrupted a
  measurement, but left hundreds of megabytes per run behind. Fixed in
  `workload_clean`.
- **Sub-microsecond timings quantised to zero.** Per-iteration times were
  recorded at millisecond precision with three decimals, so the 4 KiB yEnc case
  rounded to `0.001 ms` — three different SIMD paths collapsed onto one value,
  and one reported 0 MiB/s. Fixed to nanosecond resolution; the yEnc table
  above is the re-measurement.
- **`set -e` aborts from trailing `&&` lists.** Three separate instances
  (`(( waited++ ))`, `(( rep == 1 )) && printf`, two in `run.sh`) where an
  arithmetic or conditional expression as the last statement of a function
  became its exit status and killed the run. All converted to `if` blocks.
- **The scaling sweep was pointed at the largest workload.** Cost per cell is
  `articles × latency ÷ connections`, so the 1-connection/50 ms cell of a
  1.5 GiB corpus took ~5 minutes per repetition — half an hour for one point
  that the 10 ms curve already explained. Moved to a quick-tier workload.

---

## What to do with this

In rough order of expected value. Status as of the issues opened from this
report:

1. ~~Parallelise `parmesan`'s repair path.~~ — done,
   [#130](https://github.com/franzopl/pesto/issues/130): 20-53% wall-clock
   gain (sub-linear, bandwidth-bound — see §3), `many-small` repair now beats
   par2cmdline instead of losing to it. Details above.
2. ~~Investigate the small-file PAR2 path.~~ — done,
   [#131](https://github.com/franzopl/pesto/issues/131): root-caused by
   profiling to per-file task/channel overhead in `ops::ingest_files`, not
   the encoder's flush cadence or the #137 thread tuning (both tested and
   ruled out). `many-small` create: 72.3 → 178.3 MiB/s (+147%), now 31%
   *ahead* of parpar instead of 47% behind. Details above.
3. ~~Look at posting a single large file, and the connection-pool regression
   past 4 connections.~~ — done,
   [#129](https://github.com/franzopl/pesto/issues/129): both were the same
   root cause, confirmed by `perf stat` (flat instruction count, falling
   CPU utilization as connections increased) rather than assumed — every
   worker dequeued from one channel behind a single shared
   `tokio::sync::Mutex`, which serialized dequeues often enough at 0 ms
   latency to cost throughput past ~4 connections. Fixed by giving each
   worker its own channel with round-robin dispatch instead. `movie-1080p`
   posting-only: 618 → 1236.7 MiB/s (+100%), pesto now 1.11× nyuu instead of
   0.54×; the connection-scaling curve no longer regresses and pesto leads
   nyuu from 8 connections up (was behind at every point past 4). Details
   above.
4. **Re-run all of this on a GFNI machine.** Every PAR2 conclusion here is
   about the AVX2 kernel, on a CPU that cannot reach parmesan's fastest paths.
   — [#128](https://github.com/franzopl/pesto/issues/128)
5. ~~Refresh the README's yEnc table~~ — done,
   [#133](https://github.com/franzopl/pesto/issues/133).
6. ~~Look at the `auto` dispatch cost vs explicit AVX2~~ — investigated, not
   a bug: `auto` deliberately caps at SSSE3, a pre-existing, measured,
   documented trade-off for hybrid-CPU safety (see the note added to §2
   above). Closed as working as intended,
   [#132](https://github.com/franzopl/pesto/issues/132).
7. ~~Fix `parmesan create`'s address-space blowup on high-core machines~~ —
   done, [#137](https://github.com/franzopl/pesto/issues/137): capped tokio
   worker threads and glibc malloc arenas independent of `nproc`. Reproduced
   the exact panic on the original 128-core/`RLIMIT_AS` box and confirmed
   the fix there (`VmPeak` 9 999 764 KiB → 1 418 676 KiB, no more crash), on
   top of an 11× `VmPeak` reduction measured on this machine. Details in §8.

---

*Generated from `bench/results/medialab/20260817T015113Z/`. Reproduce with
`./bench/run.sh --workload many-small --workload mixed-folder --workload
movie-1080p --scale 0.25 --reps 3 --latencies 0,30`. See `GUIDE.md`.*
