# PAR2 geometry for Usenet posting

This report covers how pesto sizes PAR2 slices, why the sizes are aligned to the
article size, and what the defaults cost and buy. It answers issue #217.

## Summary

- Automatic slices are a whole multiple of the article size, so a lost article
  damages exactly one slice. Before, slices were about `size / 2000` bytes, and
  most articles straddled two slices.
- Small releases get one slice per article. A release up to about 2.4 GiB
  with 10 % PAR2 now survives 10 % of its articles lost anywhere. Before, it
  survived 4 to 8 %.
- Large releases keep about 2000 slices, so creating their PAR2 costs the same
  CPU as before. Alignment alone still makes them survive slightly more
  scattered loss, and never less.
- `par2_effort` (`--par2-effort`) trades CPU for protection. `par2_slice_size =
  "article"` (`--slice-size article`) gives the most protection.
- The recovery percentage keeps its meaning: recovery data is still 10 % of
  the release by default, and memory use is unchanged.

## What a recovery block repairs

A PAR2 set cuts every file into slices of one size and adds recovery blocks of
that size. Each recovery block replaces one damaged slice, however little of
it is damaged. On Usenet the damage unit is the article: a missing article
zeroes 768000 bytes (pesto's default article size) in one place.

Two numbers therefore decide what a set survives:

- **The number of recovery blocks.** It sets the CPU time to create and repair
  the set, which grows with data × blocks. With aligned slices, it is also
  exactly how many scattered lost articles the set can repair.
- **The size of a block.** Blocks × size is the recovery data posted (the
  `par2` percentage). It is also how long a run of consecutive lost articles
  can be repaired.

A 6 GiB file with the old default had 2001 slices of 3.2 MB and 200 recovery
blocks. A lost article spoiled a whole 3.2 MB slice, often two. The set
repaired about 160 scattered lost articles out of 8389 (1.9 %), although it
carried 10 % of recovery data.

To repair the same share of scattered loss in a larger release, more recovery
blocks are needed, so the cost grows with the square of the release size. No
geometry gives a 40 GiB release 10 % of scattered loss tolerance at the cost
of a 2 GiB one. The defaults below spend CPU where it is cheap and keep large
releases at the old cost.

## Default geometry

With no explicit `--slice-size` or `--slice-count`:

1. Every slice is `k ×` the article size for a whole `k ≥ 1`. If the article
   size is not a multiple of 64 bytes, the unit is the least common multiple
   of the two.
2. The slice budget is 2000 × `par2_effort` slices. A release smaller than
   4 GiB gets the budget scaled up by `4 GiB / size`, so its total CPU time
   stays about that of a 4 GiB release at the old default. Up to about
   2.4 GiB that is one slice per article.
3. `k` is the value whose slice count is closest to the budget on a log scale.
   This can land up to about 10 % above the budget. The slice count never
   exceeds 32768, the PAR2 limit.
4. If padding small files to that size would grow the set by more than 15 %,
   the old unaligned heuristic is used instead. Files smaller than an article
   are a single article each, so alignment buys them nothing, and the
   heuristic pads them less.

`parmesan::ops::calculate_geometry` implements this when
`CreateOptions::slice_unit` is set. pesto passes its article size, and the
effort from the config.

## Method

`crates/pesto/examples/par2-geometry-sim.rs` runs the real
`calculate_geometry` for each workload and strategy. It then removes random
articles and counts damaged slices. A loss is repairable when the damaged
slices do not exceed the recovery blocks. "Scattered loss" is the largest
share of articles that can be lost at random while at least 95 % of 200 trials
stay repairable. "Burst loss" does the same with runs of 20 consecutive
articles.

The create time estimate comes from a linear fit of `parmesan create` on a
4 GiB file in tmpfs on an i5-10400 (6 cores, AVX2). The fit is about 1.06 s
per GiB of reading and hashing, plus 0.00996 s per GiB per recovery block.
"Create cost" is relative to the old default (slices × padded bytes).

```text
cargo run --release -p pesto-poster --example par2-geometry-sim
```

## Results

10 % recovery, 768000-byte articles.

| workload | old default | aligned (default) | aligned, effort 2 | slice = article |
|---|---:|---:|---:|---:|
| 300 MiB | 8.05 % | **10.00 %** at 0.21× | same | same |
| episode, 1200 MiB | 5.37 % | **9.95 %** at 0.81× | same | same |
| movie, 2 GiB | 4.11 % | **9.97 %** at 1.39× | same | same |
| 2 GiB + 28 small files | 4.08 % | **10.00 %** at 1.42× | same | same |
| rar volumes, 6 × 500 MiB | 3.25 % | **5.05 %** at 1.02× | 9.98 % at 2.04× | same as effort 2 |
| movie, 3 GiB | 3.17 % | **5.03 %** at 1.04× | 9.99 % at 2.09× | same as effort 2 |
| movie, 4 GiB | 2.59 % | **3.36 %** at 0.93× | 9.99 % at 2.79× | same as effort 2 |
| movie, 6 GiB | 1.90 % | **2.54 %** at 1.04× | 5.07 % at 2.09× | 9.99 % at 4.19× |
| season pack, 8 × 1200 MiB | 1.32 % | **1.47 %** at 0.94× | 3.41 % at 2.18× | 10.00 % at 6.53× |
| movie, 15 GiB | 0.88 % | **0.93 %** at 0.95× | 2.06 % at 2.09× | 10.00 % at 10.5× |
| remux, 40 GiB | 0.35 % | **0.36 %** at 0.99× | 0.74 % at 1.99× | over the 32768-slice limit |
| 2000 files × 256 KiB | 10.00 % | 10.00 % at 1.00× | 10.00 % at 2.00× | 10.00 %, but 29 % recovery data |

Each cell shows scattered loss survived, at the create cost relative to the old
default. Burst loss was 4 % to 9.7 % in every configuration and never lower
than with the old default. The full table, with slice sizes, block counts and
burst loss, is the simulator's output.

## Measurements

On the i5-10400, with a 4 GiB file in tmpfs and 10 % recovery. Creation used
`parmesan create`. Repair used par2cmdline 0.8.1 after zeroing 55 random
articles (1 %).

| geometry | slices | create | repair |
|---|---:|---:|---:|
| old default | 2001 | 12.2 s | 88 s |
| 2 × article | 2797 | 16.0 s | 70.5 s |
| 1 × article | 5593 | 26.5 s | 69 s |

Every repair was byte-identical. Aligned sets repair faster, because fewer
slices are damaged.

The default geometry was then checked end to end, with `pesto --par2-only` on
the same file: 1865 slices of 3 articles each and 186 recovery blocks.

- **167 scattered articles lost (3.0 %).** 164 slices were damaged, and
  par2cmdline repaired the file byte for byte.
- **223 scattered articles lost (4.0 %).** 215 slices were damaged, and
  par2cmdline reported that it needed 29 more recovery blocks.

Both results match the model.

## Choosing your own default

All of these are `[posting]` keys in the config file, and CLI flags for a
single run.

| key | flag | effect |
|---|---|---|
| `par2` | `--par2` | Recovery data as a percentage of the release. With one slice per article, also the share of scattered lost articles repaired. |
| `par2_slice_size` | `--slice-size` | `"auto"` (default), `"article"` for one slice per article, or a fixed size such as `"4 MiB"`. |
| `par2_effort` | `--par2-effort` | Scales the automatic slice budget: `2.0` doubles slices, scattered tolerance of large releases and CPU time; `0.5` halves them. |
| `threads` | `--threads` | PAR2 compute threads. The default is one per usable physical core, at most 16. |

For example, a dedicated machine that wants every release to survive 10 % of
scattered loss sets `par2_slice_size = "article"`. A weak shared seedbox sets
`par2_effort = 0.5`.

## Not changed

- **Redundancy.** 10 % stays the default. With aligned slices it now means
  10 % of scattered loss for releases up to about 2.4 GiB.
- **Volume layout.** Volumes keep the exponential layout (`vol000+001`,
  `vol001+002`, `vol003+004`, …) that par2cmdline uses by default. SABnzbd and
  NZBGet download the smallest set of volumes that covers the damage, and
  exponential sizes let them get close to the exact number of blocks they
  need. A uniform layout was not measured.
- **Empty files.** par2cmdline leaves an empty file out of the set. ParPar
  lists it with a File Description but no IFSC. parmesan lists it with a File
  Description and an empty IFSC. par2cmdline 0.8.1 reports "repair required"
  for that empty file in both the ParPar and the parmesan sets, while still
  verifying and repairing the real data. No change is needed.
- **Client repair of very large slice counts** in SABnzbd's par2cmdline-turbo
  and NZBGet was not measured. The default keeps large releases near 2000
  slices, as before. Only smaller releases get more, at most about 3400
  slices around 2.4 GiB. par2cmdline repaired a 5593-slice set faster than
  the old geometry.
