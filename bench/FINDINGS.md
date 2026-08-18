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

**Measured in [#128](https://github.com/franzopl/pesto/issues/128): GFNI is
real, but it does not close the gap to `parpar` — on large files it widens.**
Every number above this point in §3 was measured on the i5-10400, which has
no GFNI. Re-run on an AWS `c6i.4xlarge` (Intel Xeon Platinum 8375C, Ice
Lake — `lscpu` confirms `gfni vaes vpclmulqdq avx512_vnni` present), same
`--scale 0.25 --reps 5` methodology, `--simd-sweep` for the per-kernel
breakdown:

| workload | case | MiB/s | noise |
|---|---|---:|---:|
| `many-small` | create (auto) | 224.8 | 1.5% |
| `many-small` | create (AVX2) | 175.3 | 0.5% |
| `many-small` | create (AVX2+GFNI) | 231.9 | 1.5% |
| `many-small` | create (AVX-512+GFNI) | 228.9 | 0.5% |
| `mixed-folder` | create (auto) | 341.2 | 3.1% |
| `mixed-folder` | create (AVX2) | 240.2 | 2.4% |
| `mixed-folder` | create (AVX2+GFNI) | 330.5 | 2.7% |
| `mixed-folder` | create (AVX-512+GFNI) | 345.5 | 1.5% |
| `movie-1080p` | create (auto) | 356.4 | 6.7% |
| `movie-1080p` | create (AVX2) | 259.8 | 0.9% |
| `movie-1080p` | create (AVX2+GFNI) | 357.4 | 0.7% |
| `movie-1080p` | create (AVX-512+GFNI) | 355.2 | 1.3% |

**GFNI's own contribution, isolated on identical hardware (AVX2 →
AVX2+GFNI, nothing else changed), is real and consistent: +32% on
`many-small`, +38% on `mixed-folder`, +38% on `movie-1080p`.**
**AVX-512+GFNI does not clearly beat AVX2+GFNI** — the two are within noise
of each other on every workload (`movie-1080p`: 357.4 vs 355.2; the
`mixed-folder` gap, 345.5 vs 330.5, is the largest and still only 4.5%).
GFNI is the multiplier here, not vector width.

**But `parpar` gained even more from the same hardware jump on large
files, so the gap widened instead of closing:**

| workload | parmesan vs parpar (i5-10400, AVX2) | parmesan vs parpar (Ice Lake, best kernel) |
|---|---:|---:|
| `many-small` | +31% (parmesan ahead, #131) | **+52%** (parmesan ahead) |
| `mixed-folder` | −27% | **−38%** |
| `movie-1080p` | −25% | **−49%** |

`parpar` on `movie-1080p` went from 357.9 to 694.1 MiB/s across the same
two machines — a 94% gain, more than double parmesan's own 38% gain from
GFNI alone. `many-small` moved the other way, because that's the workload
#131 already fixed for parmesan specifically; the large-file cases did not
get an equivalent fix and the hardware advantage went disproportionately
to `parpar` there.

**Peak memory got relatively worse too, not just relative speed.** On
`movie-1080p`, parmesan's peak RSS was 472.5 MiB against parpar's
247.4 MiB (1.91×) while running at only 0.51× parpar's throughput — a
worse memory-for-speed trade than the original i5-10400 measurement
(442 vs 255 MiB, 1.73×, at 0.75× parpar's speed).

This answers #128's open question, but sharpens rather than closes the
investigation: GFNI is not the bottleneck holding back parmesan's
large-file create performance relative to parpar — something in how the
work is distributed across cores for one large input is. Tracked in
[#148](https://github.com/franzopl/pesto/issues/148).

**Investigated in [#148](https://github.com/franzopl/pesto/issues/148):
single-threaded MD5 hashing is a real, measured cost, but neither
candidate fix helped — this stays open.** Profiling `parmesan create` on
`movie-1080p` (i5-10400, no GFNI needed — the gap predates GFNI) with
`perf record -g` found **19.6–20.9% of all CPU cycles in MD5 compression**
(`md5::compress::soft::compress`), running on a single dedicated thread —
[`Par2Worker`](../crates/parmesan/src/worker.rs)'s pipeline is a fixed
three-stage design (reader → hasher → RS-consumer, the RS-consumer's
`add_slice` calls triggering periodic rayon-parallel flushes), and the
whole-file MD5 the hasher computes is algorithmically serial: PAR2's File
Description packet needs the standard, spec-compliant digest, and there
is no way to compute one MD5 stream across multiple cores without
changing the algorithm. `strace -f -c` on the same run showed 92.2% of
syscall time in `futex` (8,458 calls) — real thread park/wake overhead,
though at a far lower density (~1,461 calls/s) than the 22,270-futex,
2.05s run that characterized #131's *pre-fix* many-small case.

Two concrete levers were tested, both empirically, both rejected by the
numbers (5 repetitions each, tight distributions, same machine and
corpus):

- **The `md-5` crate's `asm` feature** (hand-written x86-64 assembly MD5
  compression, `md5-asm`) — confirmed actually linked (`nm` shows the
  `md5_compress` symbol) and profiled again: still 20.7% of cycles in
  hashing, no better than the portable Rust `soft` path. Wall-clock got
  *slower*, not faster: 5.79s (soft, median of 5) vs 6.02s (asm, median of
  5). Old hand-tuned assembly from over a decade ago does not beat
  LLVM-optimized scalar Rust for MD5's simple add/rotate/xor mix on a
  modern out-of-order core — not a bug, just not a win here. Not adopted.
- **Coarser RS flush batching** — raising `RecoveryEncoder::add_slice`'s
  cache-blocking cap from 128 to 256 queued slices, on the theory that
  fewer, larger rayon flush bursts would amortize thread park/wake cost
  better. Measured *worse*: 6.40s median (5 reps) against the 5.79s
  baseline, with *lower* total CPU-seconds (26.6s vs 27.2s) — i.e. less
  work done in more wall-clock time, the signature of worse core
  utilization, not better. This confirms, on a large-slice workload, the
  same conclusion #131 reached on `many-small`'s small slices: the
  128-slice cache-blocking cap is already correctly tuned, not a
  bottleneck. Not adopted.

As a clean upper bound on what fixing hashing entirely could buy: forcing
`compute_hashes = false` (breaks correctness — diagnostic only, not
shippable) cut wall time from a 5.79s baseline to a 5.39s median (5
reps) — **roughly 7%**. Real, but not close to explaining a gap that
runs from −25% to −49% behind parpar. For external context only (its
source is a native Node addon, not read as part of this investigation):
`strace -f -c` on `parpar` doing the identical create shows the same
shape, 77.7% of syscall time in `futex` — so a futex-heavy profile under
`strace` is not, by itself, something distinguishing parmesan's pipeline
from parpar's; it's consistent with any multi-threaded work-stealing pool
traced this way.

**No safe, scoped fix came out of this pass.** MD5's serial cost is real
but bounded (~7% ceiling) and inherent to the format, not a parmesan bug —
every conformant PAR2 implementation pays it. The other ~40+ points of
the gap remain unexplained by anything measured here: core utilization
sat at 78–81% (4.7–4.85 of 6 physical cores) even in the best configuration
tested, so scheduling/batching tweaks alone have limited further headroom.
Whatever else parpar is doing differently for one large file was not
isolated in this pass and needs its own investigation — worth an
off-CPU/`perf sched` timeline of the `Par2Worker` pipeline specifically
(which of the reader/hasher/RS-consumer stages' channels the 8,458 futex
calls actually belong to was not broken down here) before the next
attempt.

**A second lead, tested and closed: the Shuffle2x layout does not help on
GFNI hardware, even in principle.** `RecoveryEncoder::try_new_smart`
(`crates/parmesan/src/encoder.rs`) only ever selects the Shuffle2x buffer
layout when `avx2 && !gfni`; on GFNI hardware it always falls back to the
Normal layout, and no Shuffle2x+GFNI (or Altmap+GFNI) kernel exists
anywhere in the codebase. Every GFNI benchmark in this file to this point
therefore measured the Normal layout exclusively. Two questions followed:
does the Shuffle2x layout's advantage on non-GFNI hardware come from its
buffer arrangement (which would plausibly still help combined with GFNI)
or from its `PSHUFB`-based multiply trick (which GFNI's dedicated
`GF2P8AFFINEQB` instruction would make redundant)? Answered in two steps,
both as `#[ignore]`d tests in `crates/parmesan/src/encoder.rs`:

*Step 1, this i5-10400 (no GFNI): does the layout matter at all, holding
the kernel fixed at plain AVX2?* Yes, clearly — but the first reading was
misleading. A single run said Shuffle2x beat Normal by +53%; five
independent trials (this machine runs other services, not a dedicated
bench box) ranged +8.2% to +53.0%, median **+51.3%**, floor **+33.2%**
even under heavy contention. The low outlier traced to a load spike
hitting specifically the back-to-back Shuffle2x block of that one trial —
the original test ran all Normal reps then all Shuffle2x reps in two
blocks, letting a transient spike contaminate one layout's numbers
entirely. The layout axis is real and large on this hardware.

*Step 2, AWS `c6i.4xlarge` (Ice Lake, GFNI): does it survive against
Normal's actual GFNI kernel, not Normal's plain AVX2?* A second test
(`shuffle2x_avx2_vs_normal_gfni_layout_throughput_movie_1080p`) compares
Shuffle2x+AVX2 (still its only kernel) against Normal auto-dispatching to
GFNI, with reps interleaved this time (Normal, Shuffle2x, Normal, ...)
rather than blocked, specifically to avoid step 1's contamination
mechanism. Three independent runs on a dedicated cloud instance, 7
interleaved reps each:

| run | Normal+GFNI | Shuffle2x+AVX2 | delta |
|---|---:|---:|---:|
| 1 | 634.7 MiB/s | 577.8 MiB/s | −9.0% |
| 2 | 662.2 MiB/s | 606.4 MiB/s | −8.4% |
| 3 | 662.5 MiB/s | 608.0 MiB/s | −8.2% |

Tight and consistent: **Normal+GFNI wins by 8–9%, every time.** GFNI's
dedicated instruction beats Shuffle2x's layout-plus-multiply-trick combo
outright, even though the layout alone is worth +51% against a plain AVX2
kernel. `try_new_smart`'s current heuristic (Normal on GFNI hardware,
Shuffle2x otherwise) is confirmed correct, not a missed optimization — a
combined Shuffle2x+GFNI kernel is very unlikely to beat what GFNI already
achieves with the simpler Normal layout, and is not worth building on this
evidence. This lead is closed.

Two leads tested and closed (MD5/flush-batching, and layout-vs-GFNI); the
~40+ points of unexplained gap named above stand as the next place to
look — the off-CPU `perf sched` breakdown, not attempted in either pass.

**A third lead, tested and closed: widening the recovery-block unroll does
not help — and reading parpar's actual kernel explains why.** §3's
`flush_avx2_work` (Normal layout) unrolls 4 recovery-block buffers at a
time (`crates/parmesan/src/encoder.rs`, `par_chunks_mut(4)`); with 200
buffers that means every queued input slice's bytes get re-read from
memory ~50 times per flush instead of once — the same redundant-read shape
`bench/FINDINGS.md` already flagged (§3's #130 discussion) as the kind of
thing a batched/transposed layout should fix. A width-8 variant
(`flush_avx2_work_w8`, mechanically identical, correctness-verified
byte-identical output via a differential test) was implemented and
benchmarked specifically to test whether halving the redundant re-reads
(50 passes → 25) would show up as real throughput. It didn't: **7
independent isolated single-flush measurements, both under heavy system
load and after clearing it, all landed at 0.96×–1.04× parity** — no
measurable improvement.

That negative result motivated reading parpar's actual AVX2 kernel rather
than just its dispatch/tiling layer (`gf16/controller_cpu.cpp`, cited
above). Found in `gf16/gf16_shuffle_x86_common.h:94-124`, function
`mul16_vec2x`:

```c
*dstLo = _mm256_xor_si256(tl, _mm256_shuffle_epi8(mulLo, ti));
*dstHi = _mm256_xor_si256(th, _mm256_shuffle_epi8(mulHi, ti));
```

**2 `_mm256_shuffle_epi8` per accumulator update.** parpar pre-separates
each input word's low/high bytes into two vectors before this call, so one
shared nibble index (`ti`) drives both the low-output and high-output
shuffle. Comparing shuffle counts per accumulator update, all confirmed
directly in this codebase's own code/comments:

| kernel | shuffles/update | where |
|---|---:|---|
| parmesan Normal (`avx2_apply_block`) | 8 | `encoder.rs:1651` |
| parmesan Shuffle2x | 4 | `encoder.rs:60`, `:1649` ("~33% fewer instructions") |
| parpar (`mul16_vec2x`) | 2 | `gf16_shuffle_x86_common.h:94-124` |

An 8:4:2 ratio. This is a plausible, cited explanation for why the width-8
test found nothing: if Normal's kernel already does 4× the shuffle
instructions per byte that parpar's does, instruction throughput — not
redundant memory reads — is the more likely dominant cost, and halving
re-reads on top of an already compute-bound kernel wouldn't be expected to
move wall time much. Not fully closed: whether parpar amortizes its
per-coefficient multiply-table construction (`gf16_initial_mul_vector_x2`/
`shuf0_vector`, same file, lines 31-66 and 222-248) differently than
parmesan's `all_tables` — built from scratch via scalar `gf.mul()` calls on
every single flush call, `n_rec × n_queued` entries — was not traced to a
quantified comparison and is flagged open, not assumed.

**New concrete lead for a future pass:** a 2-shuffle kernel using parpar's
shared-nibble-index technique (pre-separate low/high bytes, one shuffle
each side instead of four pairs) could plausibly close real ground on
Normal-layout throughput. Not attempted here — this is unverified GF(2^16)
multiply math on the correctness-critical hot path, and needs careful
differential-testing before any benchmark from it would be trustworthy,
the same discipline `width4_and_width8_avx2_kernels_produce_identical_recovery_data`
applied to the (negative) width-8 experiment above.

**That "2-shuffle" lead is now closed — it was a misreading of parpar's
source.** Fetched and read parpar's actual upstream files (not paraphrased
excerpts) to check the lead before building it:
[`gf16/gf16_shuffle_x86_common.h`](https://github.com/animetosho/ParPar/blob/master/gf16/gf16_shuffle_x86_common.h)
and
[`gf16/gf16_shuffle_avx2.c`](https://github.com/animetosho/ParPar/blob/master/gf16/gf16_shuffle_avx2.c).
`mul16_vec2x` (`gf16_shuffle_x86_common.h:94`) is *not* the per-input-vector
data multiply — it's a table-construction helper, called exactly 3 times
per flush setup (`gf16_shuffle_avx2.c:89-91`) to derive the Shuffle2x-style
4-way lookup tables (`prodLo1..3`/`prodHi1..3`) from the coefficient's base
table via repeated ×16 doubling, instead of parmesan's 4× scalar
`gf.mul()` loops. The actual per-input-vector hot loop is
`gf16_shuffle2x_muladd_round_avx2` (`gf16_shuffle_avx2.c:20-45`), and it
uses **4** `_mm256_shuffle_epi8` calls per source vector (`shufSwapLoA`,
`shufNormLoA`, `shufSwapHiA`, `shufNormHiA`) — the same instruction count
as parmesan's existing Shuffle2x kernel, confirmed identical technique
(pre-separated low/high-byte tables, one shared nibble index per byte-half).
There is no 2-shuffle GF(2^16) multiply-by-arbitrary-coefficient kernel in
parpar to port; the 8:4:2 ratio recorded above compared a table-setup
helper against a data hot loop and isn't a like-for-like number. Practical
consequence: on the AVX2-without-GFNI shuffle path, parmesan and parpar are
already running architecturally the same kernel, so the §3 baseline gap
(25% on `movie-1080p`, 27% on `mixed-folder`, i5-10400) is *not* explained
by a faster available multiply algorithm — it has to be tiling, threading,
or memory behavior instead.

**Fourth lead, tested and closed: parmesan's own "XOR Bit Dependencies"
kernel (ALTMAP) is not competitive, even after removing its worst
inefficiency.** Prompted by parpar's own
[`fast-gf-multiplication.md`](https://github.com/animetosho/ParPar/blob/master/fast-gf-multiplication.md),
which calls this technique "the fastest technique I've found for most x86
CPUs... for w=16" — ahead of the shuffle/Vector-Split-Lookup family both
Normal and Shuffle2x use — and confirmed live in parpar's own dispatch
(`Galois16Mul::default_method`, `gf16/gf16mul.cpp:1519-1524`): on AVX2 x86-64
it prefers `GF16_XOR_JIT_AVX2` over `GF16_SHUFFLE_AVX2` whenever
`propFastJit` is set. That gate turns out to be AMD-only
(`gf16mul.cpp:100`, "*basically, AMD, prefer 256-bit XorJit over Shuffle*",
matching Bulldozer/Jaguar/Zen family IDs) — this machine and the original
i5-10400 §3 baseline are both Intel (`cpu family: 6`, not in that list), so
parpar's own default here is `GF16_SHUFFLE_AVX2` too, same as the point
above: apples-to-apples kernels on this specific hardware. Still, since
`crates/parmesan/src/gf16.rs`'s `xor_dep_matrix` already implements the
*math* of this technique (cited from the same doc,
`gf16.rs:169`) and `RecoveryBufferSet::Altmap` already exists, it was worth
actually measuring rather than leaving it as dead code `try_new_smart`
never selects.

Reading `flush_avx2_altmap_work` first found a real inefficiency unrelated
to the algorithm itself: for every 32-byte output vector (`n_vec` of
them — 1,576 for `movie-1080p`'s slice size), the inner loop re-tested all
16×16 = 256 `(plane_out, plane_in)` mask bits with a branch per bit, even
though the mask only depends on the coefficient, which is fixed for the
entire `vi` loop. Hoisted that decode into `decode_plane_deps`, called once
per (recovery-chunk, input-slice) pair instead of once per output vector,
producing a flat index list the hot loop just walks — same XOR count, same
order (XOR is commutative/associative), verified byte-identical against
`new_altmap_produces_correct_recovery_data`. Benchmarked against Shuffle2x
on this AVX2-without-GFNI machine (`altmap_vs_shuffle2x_layout_throughput_movie_1080p`,
same `movie-1080p` geometry, 5 interleaved reps):

| kernel | median | throughput |
|---|---:|---:|
| Shuffle2x+AVX2 | 3434 ms | 447.5 MiB/s |
| ALTMAP+AVX2 (hoisted) | 32361 ms | 47.5 MiB/s |

**ALTMAP is still 89% slower — essentially unchanged in kind, just no
longer paying the branch tax on top.** This confirms the bottleneck isn't
the branching parpar's docs complain about; it's instruction count.
parmesan's ALTMAP does independent per-`plane_out` summation — for a
representative coefficient with ~50% dependency-matrix density, roughly
16 × 8 = 128 vector XORs to transform one 16-word-wide chunk, against
Shuffle2x's 4 shuffles for the same chunk. parpar's docs are explicit that
its *JIT* version only wins after **common-subexpression elimination**
across the 16 output-bit computations ("the above sample implementation
lacks optimisations such as common-expression elimination", same doc) —
sharing partial XOR sums between output bits instead of recomputing each
one from scratch, which a per-row independent loop (JIT or not) cannot do.
Building that — a small compiler pass over each coefficient's dependency
matrix producing a shared, minimal XOR sequence, then either JIT-emitting
it or interpreting it as a portable "micro-program" — is a real, bounded
piece of engineering, but a materially bigger one than anything tried in
this issue so far, and needs the same differential-testing discipline
applied here before trusting its output. Flagged as the concrete next step
if this technique is pursued further; branch removal alone is not enough,
so this is closed as "needs CSE, not just cleanup."

**Fifth lead, tested and closed: parpar's tiling figure doesn't transfer.**
parpar tunes its chunk/tile size per kernel (`gf16mul.cpp`'s
`idealChunkSize` switch, around line 452): **8 KiB** for `GF16_SHUFFLE_AVX2`
and `GF16_SHUFFLE2X_AVX2` ("try to target L2"), against parmesan's fixed
**32 KiB** chunk (`encoder.rs`, `flush_avx2_shuffle2x_work`'s
`chunk_size_bytes`) for the same kernel family — a plausible, cheap-to-test
cache-locality candidate. Swept `chunk_size_bytes` from 4 KiB to 128 KiB (a
32× range) through the real `parmesan create` CLI on this machine's
`movie-1080p` reproduction (200 recovery blocks, `-t 6 -m 1024MiB`, 3 reps
per point): every value landed at 5.6–5.9 s, indistinguishable from noise.
**This chunking constant has no measurable effect in this range on this
workload.** parpar's 8 KiB figure does not transfer to parmesan's flush
structure — most likely because the two tools tile completely differently
(parpar tiles a single (coefficient, source, dest) triple; parmesan's chunk
is a slice of *already-4×-unrolled* recovery buffers being walked against a
pre-built `all_tables` matrix, a different memory-access shape the same
tile-size intuition doesn't obviously carry over to). Closed as a dead end,
not pursued further.

**Sixth lead: the flush batch-size cap — found it.** Reproduced the §3 gap
directly on this machine with the real CLIs and current binaries (not
historical numbers): `parmesan create` on `movie-1080p`
(`-s 806912 --recovery-count 200 -t 6 -m 1024MiB`) at 5.72 s median
(268.7 MiB/s) against `parpar` (matched flags) at 3.90 s median
(394.1 MiB/s) — parmesan **31.6% behind**, same shape as the original §3
finding, on the same-model CPU (`cpu family: 6`, i5-10400-class). `perf
stat` on both (identical workload, identical thread count) is the "off-CPU
breakdown" flagged as never-attempted at the top of this section:

| metric | parmesan | parpar | |
|---|---:|---:|---|
| instructions | 190.08 B | 188.87 B | **0.6% apart — confirms the kernels really are equivalent** |
| cycles | 108.82 B | 91.76 B | parmesan +18.6% |
| IPC | 1.75 | 2.06 | parmesan −15% |
| CPUs utilized (of 6 physical) | 4.77 | 5.79 | parmesan 82% scaling, parpar 96% |
| cache-references | 6.12 B | 3.38 B | parmesan +81% |
| cache-misses | 1.41 B | 0.73 B | parmesan +92% |
| page-faults | 155 127 | 64 230 | parmesan 2.4× |

Near-identical instruction counts plus a large gap in cache traffic,
page-faults, and core utilization point at memory behavior, not compute —
consistent with §3's earlier peak-RSS finding (parmesan ~1.8–1.9× parpar's
RSS on this same workload). The suspect: every `flush_*_work` pre-builds
one SIMD coefficient table per (recovery_block × queued_slice) pair
(`all_tables`) in one rayon pass, then a second rayon pass reads it back
while doing the actual multiply — a "materialize, then stream" pattern.
parpar's equivalent (`gf16_shuffle2x_muladd_x_avx2`) builds one
coefficient's tables directly on the stack right before the loop that
consumes them, immediately, on the same thread — it never materializes a
`recovery_count × queued_slices` matrix in heap memory at all. The queued-
slice cap in `add_slice` (flat `128`, undocumented origin) controls exactly
how big that materialized table gets: at `recovery_count=200`, 128 queued
slices sizes `all_tables` at ~28 MiB against this machine's 12 MiB L3.

Swept the cap directly through the real CLI, 200 recovery blocks, 2–5 reps
per point: 16 → ~5.6 s, 32 → ~5.0 s, **48–64 → ~4.7–4.9 s (best)**, 96 →
~5.3 s, 128 (original) → ~5.7 s, 256 → ~6.35 s. Non-monotonic in both
directions — confirms two competing costs (a materialized working set that
grows worse with a bigger cap, and per-flush fixed overhead that grows
worse with a smaller one), not simply "smaller is better." A follow-up
`perf stat` on a cap-64 build partially undercuts the cache-locality
explanation, though: cache-references/misses were **essentially unchanged**
(6.61 B / 1.39 B, both ~flat vs. the cap-128 baseline) while CPU utilization
rose past parpar's (6.13 of 6 physical cores) and page-faults dropped 16%
(129 929 vs 155 127) — so wall-clock did improve for a real, measured
reason, but it's more consistent with allocation size / parallelism
efficiency than with the clean "L3 overflow" story the cache-miss counters
were expected to confirm. Recorded honestly as *not fully explained*,
alongside the fix that the sweep did find.

A first attempt made the cap adaptive — shrinking it as `recovery_count`
grows, sized to keep the table under a fixed byte budget — but that formula
does not generalize: tested at `recovery_count=1000` (pooled across two
sweep sessions, 5–6 reps each — this machine is noisy enough that a single
pass understated it), the adaptive formula's floor (16) measured **worse**
than the original flat 128 in an early narrow comparison, and even the
better-tuned flat 64 only won by a modest, noisy margin there
(median ≈19.9 s vs ≈21.2 s, pooled, +6%) — nothing like the clean +21% at
`recovery_count=200`. At `recovery_count=20` cap 64 and 128 are
indistinguishable (~2.6 s either way, table already tiny regardless of cap).
**Shipped fix:** a flat `64` (`crates/parmesan/src/encoder.rs`,
`add_slice`), not a formula — empirically safe (no regression at any tested
`recovery_count` from 20 to 1000) and a real win at the geometry this issue
is about (200 recovery blocks): **+21% on `movie-1080p`, 5.67 s → 4.67 s
median** (5-rep interleaved, cross-verified against real `par2cmdline`
create/verify/repair round-trips, all passing). Closes roughly a third of
the remaining gap to parpar: **31.6% behind → 18.8% behind**
(268.7 → 316.9 MiB/s, vs. parpar's 390.1 MiB/s that session). Not a full
close — the underlying mechanism is still not fully pinned down (see
above), and `recovery_count` values far outside the 20–1000 range tested
here are unvalidated — but a real, measured, cross-tool-verified
improvement, and the most concrete progress on #148 to date.

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
4. ~~Re-run all of this on a GFNI machine.~~ — done,
   [#128](https://github.com/franzopl/pesto/issues/128): GFNI is real
   (+32–38% isolated on identical Ice Lake hardware, AVX2 → AVX2+GFNI) but
   it does not close the gap to parpar on large files — the gap *widened*
   (`movie-1080p`: −25% → −49%, `mixed-folder`: −27% → −38%), while
   `many-small` (already fixed by #131) widened further in parmesan's
   favor (+31% → +52%). Sharper follow-up opened as
   [#148](https://github.com/franzopl/pesto/issues/148): something about
   large-single-file work distribution, not raw SIMD throughput, is now
   the bottleneck. Details above.
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
8. **Find why `parmesan create` loses ground to parpar on large files** —
   partially fixed, [#148](https://github.com/franzopl/pesto/issues/148):
   confirmed single-threaded MD5 hashing costs ~20% of cycles and is a real
   but bounded (~7% wall-clock ceiling) contributor; two candidate fixes
   (asm MD5, coarser flush batching) were tested and both measured *worse*,
   not better. A second lead — whether the Shuffle2x layout still helps
   combined with GFNI — was tested cleanly on real GFNI hardware (3
   independent runs) and closed: Normal+GFNI beats Shuffle2x+AVX2 by 8–9%,
   consistently, so `try_new_smart`'s current layout choice is already
   correct and a combined Shuffle2x+GFNI kernel is not worth building. A
   third lead — widening the recovery-block unroll from 4 to 8 to halve
   redundant source-slice re-reads — was implemented, correctness-verified,
   and also closed: 7 independent trials landed at 0.96×-1.04×, no real
   gain. That "2-shuffle kernel" lead (originally attributed to reading
   parpar's `mul16_vec2x`) is now closed as a misreading: fetching and
   reading parpar's actual upstream source (`gf16_shuffle_avx2.c`) shows
   `mul16_vec2x` is a table-construction helper called 3×/flush-setup, not
   the per-vector data hot loop — the real hot loop
   (`gf16_shuffle2x_muladd_round_avx2`) uses 4 shuffles per vector,
   architecturally identical to parmesan's existing Shuffle2x kernel. A
   fourth lead — whether parmesan's own "XOR Bit Dependencies" kernel
   (ALTMAP), the technique parpar's docs call generally fastest for w=16 on
   x86 — could compete once its worst inefficiency (256 branch-tested mask
   bits re-decoded on every output vector) was removed, was implemented,
   correctness-verified, and closed: still 89% slower than Shuffle2x even
   after hoisting the branchy decode out of the hot loop, because the real
   gap is XOR instruction count (~128 vs Shuffle2x's 4 per chunk), which
   only a common-subexpression-elimination pass (JIT or not) would fix — a
   materially bigger undertaking than anything tried in this issue so far.
   A fifth lead — parpar's per-kernel tile-size tuning (8 KiB vs parmesan's
   fixed 32 KiB) — was swept from 4–128 KiB through the real CLI and closed:
   no measurable effect in that range on this workload, parpar's figure
   doesn't transfer to parmesan's different tiling structure. **A sixth
   lead found a real, shipped fix.** Reproducing the gap directly
   (`perf stat`, real CLIs, matched flags) showed near-identical instruction
   counts (190.1B vs 188.9B, confirming the kernels really are equivalent)
   but 81% more cache-references, 92% more cache-misses, and lower CPU
   utilization for parmesan (4.77 vs parpar's 5.79 of 6 physical cores) —
   the "off-CPU breakdown" this list had flagged as never attempted. Traced
   to `add_slice`'s flush-batching cap (flat `128`, undocumented origin),
   which sizes the per-flush SIMD coefficient table (`all_tables`,
   materialized by one rayon pass and read back by another — parpar builds
   its equivalent tables directly on-stack, never materializing a
   `recovery_count × queued_slices` matrix in heap memory at all) —
   swept and replaced with a flat `64`, empirically safe from
   `recovery_count` 20 to 1000 and a real win at this issue's own geometry:
   **+21% on `movie-1080p`** (5.67s → 4.67s median), closing the gap to
   parpar from 31.6% to 18.8% behind, cross-verified against real
   `par2cmdline` round-trips. The underlying mechanism is not fully pinned
   down — a follow-up `perf stat` on the fix showed cache-misses essentially
   unchanged, so it's more likely allocation size/parallelism efficiency
   than the cache-overflow theory that motivated the sweep — flagged
   honestly as open. Details in §3.

---

*Generated from `bench/results/medialab/20260817T015113Z/`. Reproduce with
`./bench/run.sh --workload many-small --workload mixed-folder --workload
movie-1080p --scale 0.25 --reps 3 --latencies 0,30`. See `GUIDE.md`.*
