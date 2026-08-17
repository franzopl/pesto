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
2. **Investigate the small-file PAR2 path.** 47% behind parpar on
   `many-small`, and it is what makes the only end-to-end row pesto loses.
   — [#131](https://github.com/franzopl/pesto/issues/131), blocked on (5)
3. **Look at posting a single large file, and the connection-pool regression
   past 4 connections.** 0.54× nyuu at 0 ms on one big file, while being
   2.7× nyuu on many small files; the connection curve peaking then falling
   as more connections are added is the same shape of problem and likely the
   same root cause. — [#129](https://github.com/franzopl/pesto/issues/129)
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

---

*Generated from `bench/results/medialab/20260817T015113Z/`. Reproduce with
`./bench/run.sh --workload many-small --workload mixed-folder --workload
movie-1080p --scale 0.25 --reps 3 --latencies 0,30`. See `GUIDE.md`.*
