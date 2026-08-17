# pesto benchmark suite

Reproducible measurements of `pesto` and `parmesan`, and fair comparisons
against `nyuu`, `ngPost`, `parpar` and `par2cmdline`.

Everything here runs on one machine, needs no Usenet account, and generates
its own test data from fixed seeds — so two people on two continents can run
the same command and compare the resulting tables directly.

```bash
cargo build --release
./bench/run.sh --list          # what would run, and which competitors are present
./bench/run.sh micro           # yEnc + PAR2 microbenchmarks (no corpus needed)
./bench/run.sh                 # everything, quick + standard workloads
```

**New here?** [`GUIDE.md`](GUIDE.md) is the step-by-step walkthrough: what to
type, what the output means, and what to fix when a number looks wrong. This
file is the reference behind it. [`FINDINGS.md`](FINDINGS.md) is what the
suite has turned up so far.

Results land in `bench/results/<host>/<UTC timestamp>/`:

| file | what it is |
|---|---|
| `report.md` | human-readable tables — start here |
| `summary.csv` | one row per case: median, mean, stddev, noise, derived rates |
| `raw.csv` | one row per repetition — the source of truth, re-aggregatable |
| `results.json` | `summary.csv` + the machine fingerprint, for plotting |
| `system.json` | CPU, cores, SIMD, kernel, governor, every tool version |

---

## Table of contents

- [Why it is built this way](#why-it-is-built-this-way)
- [The four layers](#the-four-layers)
- [Workloads](#workloads)
- [Running it](#running-it)
- [Metrics: what each number means](#metrics-what-each-number-means)
- [Fair comparison: exact settings](#fair-comparison-exact-settings)
- [Correctness checks](#correctness-checks)
- [Reproducibility and safety](#reproducibility-and-safety)
- [Limitations — read before quoting any number](#limitations--read-before-quoting-any-number)
- [Extending the suite](#extending-the-suite)
- [CI and regression detection](#ci-and-regression-detection)
- [Layout](#layout)

---

## Why it is built this way

Three decisions shape everything else.

**Real data, not zeroes.** The previous version of this suite used sparse
files. That is fast and free, and it is wrong for almost everything measured
here: PAR2 timings against zero-filled input do not reflect real work, `7z`
compresses a 5 GB corpus of zeroes to nothing, and a sparse file removes disk
I/O from the measurement entirely. Corpora are now written by `bench-gen`
(`crates/pesto/examples/bench-gen.rs`) from a seeded xoshiro256++ stream at
~2 GB/s, with a tunable `--entropy` so the compression workloads have
something realistic to compress. Same seed and scale ⇒ same bytes on any
machine, and the CRC-32 of every file is recorded in a manifest so you can
prove it (`./bench/run.sh --verify-data`).

**A local mock NNTP server, not a real provider.** End-to-end posting numbers
taken against a real provider are not reproducible: route latency, per-account
connection caps and time-of-day load swamp the differences between posters,
and it requires an account. `examples/mock_nntp_server` ACKs `POST`/`STAT` as
fast as the kernel will carry them, and can add a fixed per-response delay.
That last knob is the interesting one — see [latency](#latency-is-a-dimension).

**Statistics, not a single run.** Every case runs `--reps` times (3 by
default); the reported figure is the **median**, and every table carries a
relative-standard-deviation column. Benchmark noise is one-sided — something
else on the machine steals time, it never gives time back — so a mean is
pulled around by a single unlucky repetition while the median is not. When the
noise column is high, the row's small differences are not differences.

---

## The four layers

Each layer answers a different question. Mixing them up is how benchmarks end
up meaning nothing.

### Layer A — microbenchmarks (`yenc`, part of `par2`)

In-memory, single-purpose, no I/O and no process startup in the measurement.

- **`10-yenc.sh`** drives `target/release/examples/yenc-bench`, which sweeps
  every SIMD path available on the CPU (scalar / SSSE3 / AVX2 / NEON /
  `auto`) across several input sizes and line lengths, plus the decoder.
  Because `auto` is measured alongside the explicit paths, the *dispatch
  decision itself* is visible: if `auto` is slower than `avx2` on your CPU,
  the table says so.
- Comparison: **node-yencode**, the C++ addon `nyuu` encodes with. Both sides
  use an internal timer after a warmup and iterate to a minimum wall time, so
  neither pays for process startup.

This is the right place to catch a regression in `crates/pesto/src/yenc/`.
It is the wrong place to draw any conclusion about upload speed.

### Layer B — pipeline stage isolation (`stages`)

The same corpus run through `pesto` with progressively more of the pipeline
switched on. Every stage is a real invocation of the real binary — nothing
here depends on internal instrumentation that could drift from what the
shipped binary does.

| stage | flags | adds |
|---|---|---|
| `read` | `cat > /dev/null` | the storage floor |
| `yenc` | `--dry-run --par2 0` | read, yEnc, article headers, NZB |
| `yenc+par2` | `--dry-run` | recovery generation |
| `par2-only` | `--par2-only` | recovery generation with no article pipeline |
| `compress` | `--dry-run --compress` | archiving |
| `post` | (mock server) `--par2 0` | NNTP framing, connection pool, sockets |
| `post+par2` | (mock server) | the default streaming pipeline |
| `post+par2-pre` | `--par2-before-upload` | the two-phase shape |
| `post+check` | `--check` | the streaming STAT confirmation pass |

The report differences *pairs of runs that differ by exactly one thing* and
prints the attribution directly. The line to watch is
`post+par2 − post+par2-pre`: that is the entire value of overlapping recovery
generation with the upload, which is what `pesto` does by default and what
`parpar+nyuu` and `ngPost` structurally cannot.

### Layer C — end-to-end uploads (`e2e`)

Complete uploads against the mock server, in two scenarios, because "which
poster is faster" has two honest answers:

- **`post-only`** — no PAR2 anywhere. `pesto` vs `nyuu` vs `ngPost` at the
  same article size, line length, connection count and group, with post
  checking off on all three. A pure poster comparison.
- **`full-release`** — data plus a 10% recovery set, i.e. what an uploader
  actually runs. Four rows, and the shapes genuinely differ:
  `pesto` (streaming), `pesto --par2-before-upload` (two-phase, the
  like-for-like row), `parpar + nyuu` (two-phase, both phases timed together
  because that is the wall time the operator waits), and `ngPost --gen_par2`.

### Layer D — scaling curves (`scaling`)

Throughput against connection count at several simulated latencies, and
`parmesan` against thread count. Output is a set of points meant to be
plotted, not read as a table.

#### Latency is a dimension

The mock server's `--latency-ms` is not a detail. At **0 ms** the measurement
is encode-and-write throughput, and a poster's pipelining strategy barely
matters. At **30 ms** — an ordinary transatlantic round trip — the same
comparison becomes a measurement of how many articles a tool keeps in flight
per connection, and the ranking can change completely. Reporting only the 0 ms
number would flatter whichever tool has the tightest local loop and say
nothing about real uploads, so `e2e` runs both by default
(`--latencies 0,30`).

---

## Workloads

A workload is a declarative description of a real upload. Six ship with the
suite, chosen to cover the axes that actually change behaviour:

| workload | tier | size @1.0 | what it stresses |
|---|---|---|---|
| `movie-1080p` | standard | 6 GiB | the common case: one large file, streaming throughput |
| `movie-remux-4k` | heavy | 40 GiB | PAR2 memory budgeting, multi-pass encoding, the 32 768-slice ceiling |
| `season-pack` | standard | ~9.4 GiB | multi-file File-ID ordering, per-episode vs global PAR2 |
| `scene-rar-set` | standard | 4 GiB | compression to volumes + obfuscation + PAR2, all on |
| `mixed-folder` | quick | ~2.1 GiB | nested folders, five orders of magnitude of file size |
| `many-small` | quick | 500 MiB | per-file overhead: 2 000 sub-article files |

**Tiers** decide what runs by default. `quick` and `standard` run unless you
say otherwise; `heavy` is opt-in (`--tier "quick standard heavy"`) because it
is 40 GiB on its own.

**Scale** shrinks or grows everything: `--scale 0.1` turns the full default
set into ~2 GiB and still exercises every code path. Corpora are cached per
scale (`bench/data/<workload>@<scale>/`), so different scales never collide
and nothing is regenerated between runs.

`many-small` deserves a note: throughput in MiB/s is *not* the interesting
number there. Files per second is. Everything that is O(files) rather than
O(bytes) shows up — directory walking, per-file CRC-32, one PAR2 File
Description packet each, one NZB `<file>` element each, and an article whose
header is a large fraction of its payload.

---

## Running it

```bash
./bench/run.sh                          # all suites, quick + standard tiers
./bench/run.sh micro                    # yenc + par2 only
./bench/run.sh e2e --workload movie-1080p
./bench/run.sh par2 --simd-sweep        # add a per-SIMD-path breakdown
./bench/run.sh --scale 0.1 --reps 5     # small corpus, more repetitions
./bench/run.sh --tier "quick standard heavy"
./bench/run.sh --drop-caches            # cold-cache runs (needs sudo)
./bench/run.sh --verify-data            # re-checksum the corpus, then exit
./bench/run.sh --report-only bench/results/<host>/latest
```

Every suite is also runnable on its own, which is how you iterate on one
without waiting for the rest:

```bash
./bench/suites/20-par2.sh movie-1080p
./bench/suites/60-correctness.sh
```

Missing competitors are detected and skipped with a note, never silently. A
machine with none of them installed still produces a complete `pesto`-only
report.

---

## Metrics: what each number means

| metric | definition | why it is there |
|---|---|---|
| `wall_ms` | nanosecond clock around the process | the number a user experiences. GNU `time`'s own `%e` is only 10 ms granular, so wall time is taken by the harness and only CPU/RSS come from `time` |
| `mibps_median` | `input_bytes / median wall` | throughput against **source** bytes, not bytes on the wire — comparable across tools that differ in overhead |
| `cpu_ratio` | `(user + sys) / wall` | parallel efficiency. ~1.0 means single-threaded; 6.0 on a 6-core box means the cores are actually being used |
| `max_rss_kb` | peak resident set | PAR2 encoders trade memory for passes over the input, so a throughput number without RSS beside it can simply be bought with RAM |
| `articles` | `<segment>` count in the tool's own NZB | a tool-independent check that two rows did the same work. The report flags rows whose counts disagree |
| `output_bytes` | recovery/archive bytes produced | catches a tool that is fast because it produced less |
| `rsd_pct` | relative standard deviation over repetitions | the noise floor for that row. Treat differences smaller than this as nothing |
| `failures` | repetitions that exited non-zero | excluded from the statistics but counted, so a tool that crashes half the time cannot post a good median |
| GF madd rate | `input_bytes × recovery_blocks / time` | the redundancy-independent PAR2 rate. Two tools at 5% and 10% recovery are not comparable on MiB/s but are directly comparable on this |

**Page cache.** Warm by default, with an explicit priming read before each
comparison so every tool in a table starts from the same state — otherwise
whichever ran first pays for the cold read and looks slower for reasons that
have nothing to do with it. `--drop-caches` switches to cold-cache runs; it
needs passwordless sudo, and which mode was used is recorded in
`system.json`.

**CPU frequency.** The suite records the governor and boost state and warns
when the governor is `powersave`, which can halve a single-threaded result. It
does not change your system settings. For publishable numbers:
`sudo cpupower frequency-set -g performance`.

---

## Fair comparison: exact settings

This is where a comparison either becomes defensible or stays marketing. The
canonical mapping lives in `bench/lib/tools.sh`; it is reproduced here so it
can be audited without reading shell.

### PAR2 creation

One geometry is computed **once** and pushed into all three tools. This is the
crux: `parmesan --slice-count 2000`, `parpar -s2000` and `par2 -b2000` each
round differently, producing three different slice sizes and therefore three
different amounts of GF(2¹⁶) arithmetic. Passing explicit bytes and an
explicit recovery block count removes that entire class of unfairness.

| | parmesan | parpar | par2cmdline |
|---|---|---|---|
| slice size | `-s <bytes>` | `-s <bytes>B` | `-s<bytes>` |
| recovery blocks | `--recovery-count N` | `-r <N>` | `-c<N>` |
| threads | `-t N` | `-t N` | `-t<N>` |
| memory ceiling | `-m <N>MiB` | `-m <N>M` | `-m<N>` |
| output | `-o DIR -b NAME` | `-o DIR/NAME.par2` | `-a DIR/NAME.par2` |
| path handling | basenames | `-f basename` | `-B DIR` |
| quiet | `-q` | `-q` | `-q -q` |

The memory ceiling matters as much as the geometry: left to their defaults
`parpar` takes up to 75% of free RAM while `parmesan` takes 1 GiB, so `parpar`
would make one pass over the input where `parmesan` makes several — and the
table would be measuring memory policy, not arithmetic throughput. Both get
the same explicit budget (`BENCH_PAR2_MEMORY`, default 1024 MiB).

The input slice count is the **sum of per-file ceilings**, not one division of
the total, because PAR2 slices each file independently and pads its last
slice. A corpus of 2 000 small files has ~2 000 slices regardless of its total
size.

`parpar` has no verify or repair mode, so it appears in create tables only.

### Posting

| | pesto | nyuu | ngPost |
|---|---|---|---|
| article size | `--article-size N` | `-a N` | `ARTICLE_SIZE` (config) |
| line length | `--line-length 128` | `--article-line-size 128` | fixed at 128 |
| connections | `--connections N` | `-n N` | `connection` (config) |
| post check | `--no-check` | `--check-tries 0` | `nzbCheck = false` |
| group | `--groups G` | `-g G` | `GROUPS` (config) |
| recursion | (walks directories) | `-r keep` | (walks directories) |
| NZB output | `-o FILE` | `-o FILE -O` | `-o FILE` |

Defaults differ and are always overridden: `nyuu` defaults to 700 K articles
and `pesto` to 768 000, so both are set to the workload's value and the
article count comes out identical. That identity is then *checked*, not
assumed — the report compares the `<segment>` count in each tool's own NZB and
flags any row where they disagree.

`pesto` additionally runs with `--no-hooks --no-history --no-notify
--no-session-log` so the measurement has no side effects.

### The full-release comparison

`pesto`'s default pipeline overlaps PAR2 generation with the upload; `nyuu`
does not generate PAR2 at all (you run `parpar` first) and `ngPost` generates
before posting. Comparing a streaming pipeline against a two-phase one on wall
time alone would be either unfair or uninformative depending on which side you
favour, so the suite reports **both**: `pesto` streaming *and* `pesto
--par2-before-upload`, the latter being structurally identical to what the
competitors do. The gap between those two rows isolates the overlap; the gap
between `--par2-before-upload` and `parpar+nyuu` isolates raw throughput.

`parpar + nyuu` is timed as a single script, because two phases the operator
waits through sequentially are one wall time.

A workload that sets `WL_COMPRESS` (only `scene-rar-set` does) gets archiving
folded into the same measurement, because for a scene-style release the
archive step *is* part of the upload rather than something you did earlier.
`pesto` and `ngPost` both do it themselves and both get the same format and
volume size; `nyuu` has no archiving stage at all, so on those workloads it
appears in the `post-only` scenario only. That absence is a capability
difference, not an omission, and the report shows it as a missing cell rather
than a zero.

---

## Correctness checks

`60-correctness.sh`. A PAR2 encoder can be made arbitrarily fast by producing
recovery data that does not recover anything, and a yEnc encoder by producing
output nobody can decode. So:

1. `parmesan` creates → `par2cmdline` verifies, repairs, and the restored tree
   matches the original byte for byte.
2. `par2cmdline` creates → `parmesan` verifies and repairs, same check.
3. `parpar` creates → `parmesan` verifies.
4. **Recovery size accuracy** — bytes produced vs bytes requested, per tool.
   A tool that quietly produced fewer blocks would otherwise post an excellent
   throughput number.
5. **Wire round-trip** — `pesto` posts to the mock server with `--save-dir`,
   and `bench/tools/yenc_decode.py` reassembles the source from the captured
   articles and compares SHA-256. That decoder is deliberately *not* pesto's:
   it is written from the yEnc draft with nothing but the Python standard
   library, so agreement means the bytes on the wire are genuinely yEnc, not
   that two halves of one codebase share a misunderstanding. It also verifies
   every `=yend pcrc32=` and asserts that the decoded parts cover the file
   exactly once, with no gap or overlap.

Cross-tool verify/repair needs every protected file to sit directly beside the
index, because all three tools store bare file names in their File Description
packets. Workloads with subdirectories are skipped for those checks
explicitly, rather than reported as tool failures.

---

## Reproducibility and safety

**Reproducible.** Corpora come from fixed seeds; `--verify-data` re-checksums
every file against the manifest written when it was generated. The exact tool
versions, CPU, core count, SIMD flags, kernel, governor, filesystem and
repetition count all land in `system.json` next to the numbers — a throughput
figure without the machine it came from is not a result.

**Safe.** Every tool runs inside a throwaway sandbox with `HOME`,
`XDG_CONFIG_HOME` and `TMPDIR` redirected into the run directory. That is the
mechanism behind the "no Usenet account needed" guarantee, not tidiness:

- `pesto` resolves its config from `$XDG_CONFIG_HOME/pesto/config.toml` and
  runs hook scripts from `$XDG_CONFIG_HOME/pesto/hooks/`. Pointing both at an
  empty directory means a benchmark can never pick up your real credentials
  and can never fire your indexer hooks.
- `ngPost` reads `~/.ngPost` by default, which on a real machine names a real
  provider. The suite always passes an explicit generated `-c` config.
- Every server-facing invocation goes through `assert_local_target`, which
  refuses any host that is not loopback.

Nothing outside `bench/results/` and `bench/data/` is written. The PAR2 suite
does write recovery sets into the corpus directory — the only place every tool
can verify and repair from — and removes them afterwards, including
`par2cmdline`'s numbered pre-repair backups.

---

## Limitations — read before quoting any number

- **The mock server is not a Usenet provider.** It removes propagation delay,
  per-account concurrency caps, TLS, and server-side rejection behaviour by
  design. End-to-end numbers describe how fast a tool can *produce and send*
  articles, not how fast a given provider will accept them. `--latencies`
  simulates round-trip time only.
- **TLS is not measured.** All posting is plaintext so that the comparison
  covers the posters rather than three different TLS stacks. `pesto` uses
  rustls in production; that cost is not in these tables.
- **Node-yencode is the only external yEnc implementation compared**, and it
  is the encoder only — nothing here compares decoders across projects.
- **`ngPost` reports its own NZB path**, appending `_1`, `_2` on collision; the
  suite reads the newest NZB in its output directory to count articles.
- **Single machine, single run.** Cross-machine comparison works because the
  corpus is identical, but CPU, memory bandwidth, filesystem and kernel differ,
  and `system.json` exists so you can see how much.
- **A high `rsd_pct` invalidates its row.** Shared runners, thermal throttling
  and background work all show up there. Raise `--reps` before believing a
  small difference.
- **`--drop-caches` is off by default.** Numbers are warm-cache unless you
  turn it on, which means storage read cost is understated for corpora that
  fit in RAM. This is recorded in `system.json`.

---

## Extending the suite

**A new workload** is one file in `bench/workloads/`, no code changes:

```sh
# bench/workloads/my-release.workload
WL_DESC="What this represents and why it is worth measuring"
WL_TIER=quick                     # quick | standard | heavy
WL_SEED=2001                      # any fixed number; changes the bytes
WL_FILES=(                        # relpath|size|entropy(0-100)
    "Release/main.mkv|3G|100"
    "Release/notes.nfo|8K|30"
)
WL_GENSETS=(                      # subdir|template|count|size|entropy
    "Release/Subs|sub-%02d.srt|20|64K|25"
)
WL_PAR2_PCT=10
WL_ARTICLE_SIZE=768000
WL_OBFUSCATE=none                 # none | full | full-shared | light
WL_COMPRESS=""                    # "" | 7z | rar
WL_CONNECTIONS=8
WL_LAYERS="stages e2e"            # which suites should use it
```

**A new competitor** is a detection entry in `detect_tools`, a version rule in
`tool_version`, and one `bench_case` call in the relevant suite, guarded by
`skip_missing`. Its suite-specific settings go in the `extra` column as
`k=v;k=v` — the CSV schema itself never changes, which is what keeps additions
from being migrations.

**A new metric** goes into `extra` too, unless it varies between repetitions.
Anything in `extra` is part of the grouping key, so a *measured* value there
would put every repetition in its own group and leave nothing to take a median
over.

---

## CI and regression detection

Microbenchmarks are the only layer worth gating in CI: they are CPU-bound,
need no corpus, and finish in a couple of minutes. End-to-end numbers on a
shared runner are too noisy to gate on and should be run on known hardware.

`.github/workflows/bench.yml` (manual + weekly) runs the `yenc` and small
`par2` cases with extra repetitions, then compares the medians against a
committed baseline:

```bash
./bench/run.sh micro --scale 0.05 --reps 5 --yes
./bench/compare.sh bench/baseline/ci-x86_64.json bench/results/*/latest/results.json --threshold 10
```

`compare.sh` exits non-zero when a case regresses by more than the threshold,
refuses to compare across different CPU models, and prints a table of what
moved. Refresh a baseline deliberately, with the run that justifies it:

```bash
cp bench/results/<host>/latest/results.json bench/baseline/ci-x86_64.json
```

The threshold is 10% on purpose. GitHub runners drift by several percent
between runs; a tighter gate produces false alarms, which are worse than no
gate because they train people to ignore it. Catching a 10% regression
automatically, and finding the 2% ones by running the suite on real hardware,
is the right split.

---

## Layout

```
bench/
├── run.sh                  entry point: orchestration, corpus, reporting
├── compare.sh              regression gate: two results.json, one threshold
├── lib.sh                  the single include (sources everything in lib/)
├── lib/
│   ├── core.sh             paths, terminal output, the safety sandbox
│   ├── sysinfo.sh          machine + toolchain fingerprint → system.json
│   ├── stats.sh            the live median; the rest is computed in record.sh
│   ├── measure.sh          what "a measurement" means; repetitions
│   ├── record.sh           the CSV schema and its aggregation
│   ├── data.sh             workload definitions → cached corpora
│   ├── tools.sh            competitor discovery + the matched-flag mapping
│   ├── nntp.sh             mock server lifecycle
│   └── report.sh           summary.csv → report.md
├── suites/
│   ├── 10-yenc.sh          Layer A: yEnc kernels vs node-yencode
│   ├── 20-par2.sh          PAR2 create/verify/repair vs parpar, par2cmdline
│   ├── 30-stages.sh        Layer B: pipeline stage attribution
│   ├── 40-e2e.sh           Layer C: full uploads vs nyuu, ngPost, parpar+nyuu
│   ├── 50-scaling.sh       Layer D: connection and thread scaling curves
│   └── 60-correctness.sh   cross-tool interop + wire-level round-trip
├── workloads/*.workload    declarative corpus + posting definitions
├── tools/yenc_decode.py    independent yEnc decoder for the round-trip check
├── yencode.js              node-yencode driver, matched to the Rust one
├── baseline/               committed reference numbers for compare.sh
├── data/                   generated corpora (gitignored, cached by scale)
└── results/                run outputs (gitignored)
```

`yenc.sh`, `par2.sh` and `posting.sh` still exist as one-line forwarders to the
suites that replaced them, so older links and notes keep working. They print a
deprecation notice; the methodology they used is described in their headers and
is not what runs any more.

Supporting binaries, built from the workspace:

| binary | source | role |
|---|---|---|
| `bench-gen` | `crates/pesto/examples/bench-gen.rs` | deterministic corpus generator |
| `yenc-bench` | `crates/pesto/examples/yenc-bench.rs` | yEnc micro driver, JSON output |
| `mock_nntp_server` | `crates/pesto/examples/mock_nntp_server.rs` | local NNTP endpoint with latency, stats and article capture |
