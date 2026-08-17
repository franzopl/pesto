# Running the benchmarks yourself

A step-by-step guide. `README.md` next to this file is the reference —
methodology, exact flags, limitations. This one is the walkthrough: what to
type, what you will see, and what to fix when a number looks wrong.

You do **not** need a Usenet account, a network connection, or any of the
competing tools. Those only add rows to the tables.

---

## 1. Five-minute setup

```bash
git clone https://github.com/franzopl/pesto
cd pesto
cargo build --release
```

That builds `pesto`, `parmesan`, and the three helper binaries the suite
drives (`bench-gen`, `yenc-bench`, `mock_nntp_server`). If you skip it,
`run.sh` builds them for you on first use.

Check what the suite can see on your machine:

```bash
./bench/run.sh --list
```

```
── workloads ──
  many-small       [quick]   2000 sub-article files (per-file overhead worst case)
  mixed-folder     [quick]   Release folder: one video plus subtitles, NFO, sample, artwork
  movie-1080p      [standard] Single 1080p WEB-DL movie (one large file)
  ...

── tools ──
  TOOL           STATUS     VERSION
  par2           present    par2cmdline version 0.8.1
  parpar         absent     -
  nyuu           absent     -
  ...
```

Anything `absent` is simply skipped. Section 5 covers installing them.

**Requirements:** Linux or macOS with bash 4.4+ (macOS ships 3.2 —
`brew install bash`), GNU `time` for the memory column (`apt install time`;
without it the RSS column reads 0), and Python 3 for one correctness check.

---

## 2. Your first run

Start with the microbenchmarks. They need no test data at all, so nothing is
written to disk and the whole thing takes a couple of minutes:

```bash
./bench/run.sh yenc
```

```
── yEnc encode/decode microbenchmark ──
  sizes=4096,131072,768000,8388608 line_lens=128,256 min_time=1.0s
  in-memory only — no file I/O, no articles, no network

  encode   scalar   ll=128     768000 B     553.9 MiB/s
  encode   ssse3    ll=128     768000 B    2317.4 MiB/s
  encode   avx2     ll=128     768000 B    2379.6 MiB/s
  encode   auto     ll=128     768000 B    2275.7 MiB/s
  decode   auto     ll=128     768000 B     694.2 MiB/s
```

Then the interoperability checks, which build their own small corpus (~40 MiB)
and finish in under a minute:

```bash
./bench/run.sh correctness
```

If those two work, everything else will.

---

## 3. Reading what comes out

Every run writes a timestamped directory:

```
bench/results/<your-hostname>/20260817T005947Z/
├── report.md      ← start here
├── summary.csv    one row per case: median, mean, stddev, noise, rates
├── raw.csv        one row per repetition — re-aggregate it however you like
├── results.json   summary + machine fingerprint, for plotting
├── system.json    CPU, cores, SIMD, kernel, governor, tool versions
└── logs/          stdout+stderr of every measured command
```

`bench/results/<hostname>/latest` always points at the most recent run.

Four things to look at in `report.md`:

**The noise column.** It is the relative standard deviation across
repetitions. A row reading 6% noise cannot support a claim about a 3%
difference. If most rows are above ~5%, see section 6.

**`auto` versus the explicit SIMD paths** in the yEnc table. `auto` is the
runtime dispatch. If it is meaningfully slower than the best explicit path on
your CPU, dispatch is picking wrong — that is a finding, not a rounding error.

**The attribution table** under *Pipeline stages*. Each line is one stage
difference between two real runs that differ by exactly one thing:

```
| cost of                | measured as               | value  |
| storage read           | read                      | 0.03s  |
| yEnc + articles + NZB  | yenc − read               | +0.73s |
| PAR2 generation        | yenc+par2 − yenc          | +11.45s|
| streaming PAR2 overlap | post+par2 − post+par2-pre | -1.18s |
```

A negative "streaming PAR2 overlap" is time saved by computing recovery data
while articles are already going out. They do not sum to the total — the
pipeline overlaps stages on purpose.

**⚠ markers** in the comparison tables. `article counts differ` means the tools
did not post the same number of articles; on a `post-only` row that makes the
row meaningless, on a `full-*` row it is expected (implementations split
recovery volumes differently). `N failed rep(s)` means a median was computed
from a tool's lucky runs.

---

## 4. The full run — sizing it for your machine

The complete suite generates real test data. Before it writes anything it
tells you how much and checks your free space:

```
  corpus needed : 21.9 GiB
  free on disk  : 84.0 GiB
  generate corpus and continue? [y/N]
```

`--scale` multiplies every file size, so the same workloads fit any machine:

```bash
./bench/run.sh --scale 0.1     # ~2 GiB corpus, everything still exercised
./bench/run.sh                 # full size, quick + standard tiers (~22 GiB)
./bench/run.sh --tier "quick standard heavy"   # adds the 40 GiB 4K remux
```

**Start at `--scale 0.1`.** Every code path runs, the whole thing finishes in
a fraction of the time, and you find out whether your setup works before
committing an afternoon to it. Scale up once you want numbers to quote.

Corpora are cached per scale under `bench/data/<workload>@<scale>/` and reused
between runs, so the second run of a given scale skips generation entirely.
They are generated from fixed seeds, which is what makes your tables directly
comparable with someone else's — verify that at any time:

```bash
./bench/run.sh --verify-data
```

Delete `bench/data/` whenever you want the space back; it regenerates.

Useful subsets:

```bash
./bench/run.sh micro                       # yenc + par2, no corpus for yenc
./bench/run.sh par2 --simd-sweep           # per-SIMD-path PAR2 breakdown
./bench/run.sh e2e --workload movie-1080p  # one workload, one layer
./bench/run.sh stages e2e --scale 0.1
```

Or run one suite directly, which is how you iterate on a single thing without
waiting for the rest:

```bash
./bench/suites/20-par2.sh movie-1080p
```

---

## 5. Adding the competitors

Optional, but they are the point of half the tables.

```bash
# par2cmdline — PAR2 create/verify/repair reference implementation
sudo apt install par2            # Debian/Ubuntu
brew install par2                # macOS

# parpar — the fast PAR2 creator
npm install -g @animetosho/parpar

# nyuu — the poster most comparisons are against
npm install -g nyuu

# node-yencode — the yEnc encoder nyuu uses, for the micro comparison
npm install yencode              # from the repo root; a local install is fine

# ngPost — https://github.com/mbruel/ngPost (build or download a release)
```

The suite never touches their config files. It generates its own and points
everything at the local mock server, so your real `~/.ngPost` and your real
provider are not reachable from a benchmark run.

Re-run `./bench/run.sh --list` to confirm they were picked up.

---

## 6. Making the numbers trustworthy

In rough order of how much they matter:

**Close everything else.** Browsers, editors, containers, syncing clients. The
suite reports noise honestly, which means a busy machine produces visibly
useless tables rather than quietly wrong ones.

**Set the CPU governor to performance.** `powersave` can halve a
single-threaded result and adds a lot of variance. The suite warns you when it
detects it:

```bash
sudo cpupower frequency-set -g performance
# or: echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor
```

**Raise the repetition count** for anything you intend to publish:

```bash
./bench/run.sh --reps 7
```

**Decide about the page cache.** By default runs are warm-cache, with an
explicit priming read before each comparison so every tool starts from the
same state. That measures CPU-bound throughput. For storage-inclusive numbers:

```bash
./bench/run.sh --drop-caches      # needs passwordless sudo
```

Whichever mode ran is recorded in `system.json`, so a reader can tell.

**Add a warmup** if your storage or CPU needs a moment to settle:

```bash
./bench/run.sh --warmup 1         # one unrecorded repetition per case
```

---

## 7. Measuring your own change

This is the workflow the suite is really for. Before and after, same machine,
same session:

```bash
# before
cargo build --release
./bench/run.sh micro --workload many-small --scale 0.1 --reps 5 --yes
cp bench/results/*/latest/results.json /tmp/before.json

# ...make your change...
cargo build --release
./bench/run.sh micro --workload many-small --scale 0.1 --reps 5 --yes

# compare
./bench/compare.sh /tmp/before.json bench/results/*/latest/results.json
```

```
regression check
────────────────────────────────────────────────────────────────────────────
  baseline  : /tmp/before.json
  candidate : bench/results/medialab/latest/results.json
  cpu       : Intel(R) Core(TM) i5-10400 CPU @ 2.90GHz
  threshold : 10% slower
────────────────────────────────────────────────────────────────────────────

CASE                                                         BASE ms    NEW ms    CHANGE
yenc|micro|pesto|encode/avx2/ll128/768000B                      0.30      0.27    -10.3%
par2|many-small|parmesan|create-r10                          1460.00   1455.00     -0.3%

2 case(s) compared

1 case(s) improved by more than 10%:
  yenc|micro|pesto|encode/avx2/ll128/768000B                      0.30      0.27    -10.3%

no regression beyond 10%
```

It exits 0 when nothing regressed past the threshold and 1 when something did,
so it drops straight into a script or a git hook. `--threshold 5` tightens it.

It also refuses to run when the two files came from different CPUs:

```
bench error: CPU mismatch — baseline 'Intel(R) Core(TM) i5-10400 CPU @ 2.90GHz'
vs candidate 'Intel(R) Core(TM) i9-14900K'.
A comparison across machines measures the machines, not the change.
Refresh the baseline on this runner, or pass --allow-cpu-mismatch.
```

That is deliberate. A "regression" that is really a different machine is worse
than no check at all, because it teaches you to ignore the check.

---

## 8. Sharing results

The tables in `report.md` are meant to be pasted directly into an issue or a
discussion. Paste the machine block at the top with them — a throughput number
without the CPU, core count and SIMD tier it came from cannot be compared with
anything.

Because the corpus comes from fixed seeds, two people running

```bash
./bench/run.sh --scale 0.25 --reps 5
```

are encoding byte-identical input, and their tables can be put side by side.
That is the whole reason for the seeded generator.

Please include, along with the tables:

- whether the governor was set to `performance`
- whether the machine was otherwise idle
- anything unusual about the storage (tmpfs, network filesystem, spinning disk)

---

## 9. When something goes wrong

**"bash 4.4+ required"** — macOS. `brew install bash`, then run the scripts
with the new one (`/opt/homebrew/bin/bash ./bench/run.sh …`) or put it earlier
in `PATH`.

**RSS column is always 0** — GNU `time` is missing. `sudo apt install time`,
or `brew install gnu-time` on macOS. Everything else still works.

**A tool row says `FAILED (exit N)`** — the tool's own output is in
`bench/results/*/latest/logs/<suite>-<workload>-<tool>-<variant>.log`. That is
almost always a flag mismatch or a tool limitation, and the log says which. A
failure is recorded as a failure, never hidden or averaged away.

**"not enough free space for the corpus"** — lower `--scale`, pick fewer
workloads with `--workload`, or point the cache elsewhere:

```bash
BENCH_DATA_DIR=/mnt/big/bench-data ./bench/run.sh
```

**A competitor says "not installed" though you installed it** — it has to be
on `PATH` for the shell running the suite. `command -v nyuu` is the exact test
the suite uses.

**The run seems stuck** — the microbenchmark iterates each case to a minimum
wall time and only prints on the first repetition, so several quiet minutes
are normal. `BENCH_YENC_MIN_TIME=0.3 ./bench/run.sh yenc` speeds it up at the
cost of noisier numbers. The end-to-end suite at 30 ms simulated latency is
genuinely slow — that is the measurement.

**A number looks impossible** — check the noise column first, then
`system.json` for the governor, then whether the corpus is on tmpfs. If it
still looks wrong, `raw.csv` has every individual repetition and `logs/` has
every command's output; both are there so a surprising result can be
investigated rather than believed.

---

## Where to go next

- `README.md` — methodology, the exact flags that make each comparison fair,
  and the limitations that apply to every number here.
- `FINDINGS.md` — what the suite has already turned up, with the measurements.
- `workloads/*.workload` — the workload definitions; adding one is a single
  file and no code.
