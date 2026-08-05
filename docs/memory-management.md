# Pesto memory management — design and implementation plan

Status: **Phase 0 implemented**; Phases 1–5 proposed. Scope: `pesto` (and, where
noted, `parmesan`).

---

## Update: ultra.cc runs the musl build

The first draft of this document led with glibc's malloc-arena explosion. That
finding is real, but **ultra.cc runs only the musl binary**, and musl has no
`mallopt` and no per-core arenas — so it does not apply to the deployment that is
actually crashing. Re-measured on musl (128 threads, each allocating a little):

| libc | per-thread VA, default stacks | at 1 MiB stacks | returned when threads exit? |
|---|---:|---:|---|
| **musl** | **2.10 MiB** | **1.10 MiB** | yes — VmSize returns to 0 |
| glibc | 9.52 MiB | 8.52 MiB | no — ~1 GiB floor persists |

On musl, per-thread address space is **purely the thread stack**: linear in thread
count, fully reclaimed, and entirely controlled by thread count × stack size. That
makes Phase 0 on this platform a matter of bounding pools, not tuning an allocator.
The arena fix is retained for the glibc build's benefit and compiles out on musl.

**Measured result of Phase 0** on the real binary with a 128-thread profile
(`--par2-only --threads 128`): peak address space **803.5 MiB → 443.3 MiB**, a 45%
reduction.

Two further corrections that came out of implementing this, both of which removed
work rather than adding it:

- **tokio's blocking pool does not pre-allocate.** Startup address space is
  identical at `max_blocking_threads` 512 and 64. Capping it bounds the worst case
  but yields no routine saving — the earlier estimate of ~1 GiB recovered here was
  wrong.
- **Bounding the check channel would achieve nothing.** The feeder at
  `check.rs:299` drains the channel into `Inner::heaps` immediately, so the channel
  never accumulates; the backlog lives in the `BinaryHeap`, which is unbounded
  regardless of channel type. Bounding the channel was dropped from Phase 0 (it
  would have added stall risk for zero benefit); bounding the *heap* is Phase 3.

Sections 0.3 and 5 below are annotated accordingly; the rest of the analysis —
which turns on address space rather than RSS — is unchanged and still holds.

---

## 0. Corrected problem statement (read this first)

The brief for this work assumed three things that the code does not support. Getting
these right changes the design substantially, so they come first.

### 0.1 There is (almost) no process tree

PAR2 is **not** a child process. `parmesan` is linked in as a library and
`RecoveryEncoder`/`Par2Worker` run on `pesto`'s own threads
(`crates/pesto/src/poster/mod.rs:1772-1799`). The upload and check pools are tokio
tasks in-process. The only real children `pesto` spawns are:

- external archivers (`rar`/`7z`) in `crates/pesto/src/compress.rs:262,305`
- user hooks in `crates/pesto/src/hooks.rs` and `crates/pesto/src/bin/pesto.rs:3032+`

Neither runs concurrently with PAR2 encode or upload — compression happens before
posting, hooks before/after. So "process-tree memory budgeting" is a real but
**minor** concern (Phase 5 below), not the centrepiece. Nearly all the memory that
matters is this process's own heap, which we can account for exactly and cheaply.

### 0.2 The wall being hit is address space (`RLIMIT_AS`), not RAM

Both reported crashes are `handle_alloc_error` — the allocator returned null. On a
seedbox that overwhelmingly means `RLIMIT_AS` (`ulimit -v`), not physical memory
exhaustion. This matches the symptom you described ("even when system memory is
still available") and it is why the failing allocation in Example 1 is only
**1.5 MB**: a 1.5 MB request failing means there was essentially *no* address space
left, not that memory was tight.

This distinction is load-bearing. A monitor that watches `sysinfo::available_memory()`
and cgroup usage — i.e. the obvious design — **would not have prevented either
crash**, because RSS was never the binding constraint. The governor must track
`VmSize` against `RLIMIT_AS` as a first-class ceiling.

The codebase already knows this in part: `address_space_limit()`,
`address_space_budget()` and `connection_overhead_reserve()`
(`poster/mod.rs:1512-1573`) exist precisely because of an earlier round of this bug.
The gap is that the budget is computed **once, from a static estimate, and never
compared against reality**.

### 0.3 The dominant consumer of address space is the allocator itself

This is the finding that most changes the plan, and it is measured, not theorised.

glibc's malloc creates per-thread arenas (up to `8 × ncores`), each reserving a
64 MiB `HEAP_MAX_SIZE` region via `mmap`. Those reservations are mostly `PROT_NONE`
and cost almost no RSS — but they count in full against `RLIMIT_AS`, and they are
never returned to the OS.

Measured on a 16-core box, with each thread allocating only 2 MiB of real data:

| Threads | `MALLOC_ARENA_MAX` | VmSize (peak) | VmRSS | VmSize after threads exit |
|---:|---|---:|---:|---:|
| 64  | default | **1222 MiB** | 4 MiB | 1001 MiB |
| 64  | 2       | 326 MiB      | 4 MiB | 105 MiB |
| 128 | default | **1481 MiB** | 7 MiB | 1001 MiB |
| 128 | 2       | 585 MiB      | 7 MiB | 105 MiB |
| 256 | default | **1999 MiB** | 12 MiB | 1001 MiB |
| 256 | 2       | 1103 MiB     | 12 MiB | 105 MiB |

Three things to take from this:

1. **~300× amplification** of VmSize over VmRSS. The arena floor here is ~1 GiB of
   pure address space for ~4 MiB of live data.
2. **It scales with core count, not thread count** (the 8×ncores arena cap), and it
   **never shrinks** — note the "after threads exit" column stays at 1001 MiB.
   On a 128-core ultra.cc box the arena ceiling is ~128 arenas × 64 MiB ≈ **8 GiB of
   address space**, which on its own can consume an entire `ulimit -v` before PAR2
   allocates a single slice.
3. `mallopt(M_ARENA_MAX, 2)` called at the top of `main()` is as effective as the
   env var (verified: identical VmSize, `mallopt` returns 1).

`pesto` runs `#[tokio::main]` with no thread configuration
(`bin/pesto.rs:2453`), so it gets `ncores` worker threads, a blocking pool that can
reach 512 threads, **plus** a rayon pool of `ncores`
(`poster/mod.rs:1286`). On a 128-core box that is 250+ threads at 2 MiB of stack
each (another ~500 MiB of VA) on top of the arena reservations.

**Note on musl vs glibc.** You ship both (`.github/workflows/release-pesto.yml:26,29`).
musl's mallocng does not have glibc's per-core arena behaviour, so it does not show
this amplification — consistent with the code comment at `poster/mod.rs:1558-1567`
recording that a *musl* binary held steady at 2.4–3.0 GiB against a ~9.5 GiB ceiling.
**Worth confirming which binary produced your two crash reports.** If they came from
`pesto-linux-x86_64` (glibc), §0.3 is very likely the whole story and Phase 0 alone
may resolve it.

### 0.4 Why backpressure alone cannot fix this

With `panic = "abort"` in the workspace release profile (`Cargo.toml`), an allocation
failure is an immediate `SIGABRT` — no unwind, no log flush, no degradation path.
Backpressure is a *statistical* defence: it lowers average pressure but cannot stop
one large `Vec::with_capacity` from crossing the line. Requirement 5 ("never crash if
it can be avoided") therefore needs **fallible allocation (`try_reserve`) at the
handful of sites that allocate big**, not just throttling. Throttling and fallible
allocation are complementary and both are in the plan.

### 0.5 Summary of the actual bug

| Layer | Real cost on 128-core seedbox | Modelled by current code? |
|---|---|---|
| glibc arena VA reservations | up to ~8 GiB | **No** |
| Thread stacks (tokio + blocking + rayon) | ~0.5–1.5 GiB | Only as `32 MiB × par2_threads` |
| PAR2 pass working set | `memory_limit` | Yes (this is the only modelled term) |
| `Par2Worker` channels (64 × slice_size × 3 stages) | 100s of MiB | Partially (a past fix) |
| Upload in-flight bodies (`conns × 2 × article_size`) | ~120 MiB | As `8 MiB × conns` |
| `results: Vec<PostedSegment>` + check heap clone | ~150 MiB @ 136k segments | **No** |
| Check queue (**unbounded channel**, `check.rs:296`) | unbounded | **No** |

`connection_overhead_reserve` predicts ~5 GiB of overhead on that box and hands PAR2
the remaining half. It is not a bad model — it is just missing the single largest
term, and nothing ever checks the prediction against `/proc/self/statm`.

---

## 1. Architecture

### 1.1 New module: `pesto::memory`

```
crates/pesto/src/memory/
├── mod.rs        // MemoryGovernor: public API, pressure state machine
├── ceiling.rs    // discover the binding limit (RLIMIT_AS / cgroup / host / flag)
├── sampler.rs    // /proc/self/statm, cgroup memory.current, PSI; OS thread
├── alloc.rs      // counting global allocator (exact live-heap accounting)
├── budget.rs     // stage sub-budgets + admission control (permits)
└── tune.rs       // Phase 0: mallopt, thread stacks, pool sizing
```

### 1.2 The governor

A single `Arc<MemoryGovernor>` created in `main()` before the runtime starts, threaded
into `Shared` (`poster/mod.rs:352`) so every stage can reach it.

```rust
pub struct MemoryGovernor {
    ceiling: Ceiling,                  // discovered once, immutable
    live_heap: &'static CountingAlloc, // exact, from the global allocator
    sampled: ArcSwap<Sample>,          // VmSize/VmRSS/cgroup/PSI, refreshed ~250ms
    pressure: AtomicU8,                // Normal | Elevated | Critical | Emergency
    budgets: StageBudgets,             // par2 / upload / check sub-budgets
    notify: tokio::sync::watch::Sender<Pressure>,
}
```

Two independent accounting sources, deliberately:

- **`CountingAlloc`** — a `GlobalAlloc` wrapper doing one relaxed
  `fetch_add`/`fetch_sub` per allocation. Exact, zero lag, tells us what *pesto*
  is holding. Cost is ~1–2 ns per allocation; `pesto` allocates per-article, not
  per-byte, so this is far below noise on the hot path. (Prior art: `cap`,
  `stats_alloc`.)
- **`/proc/self/statm`** — VmSize and VmRSS, i.e. what the *kernel* thinks,
  including allocator overhead, arena reservations, thread stacks and mmap'd
  regions. This is the number `RLIMIT_AS` is enforced against.

The **gap between them is the diagnostic signal**. Live heap 2 GiB with VmSize 9 GiB
means allocator/VA overhead is eating the budget — exactly the §0.3 failure, and
something neither source alone can tell you.

### 1.3 Integration points

| Stage | Hook | File |
|---|---|---|
| Startup tuning | `tune::apply()` before runtime | `bin/pesto.rs:2453` |
| PAR2 pass sizing | replace ad-hoc calc with `governor.budget(Stage::Par2)` | `poster/mod.rs:1645-1685` |
| PAR2 channel depth | depth from budget, not `DEFAULT_CHANNEL_DEPTH` | `poster/mod.rs:1795` |
| Upload concurrency | `Semaphore` permits resized by pressure | `poster/mod.rs:966-977` |
| Check queue | bounded channel + backlog cap | `check.rs:296` |
| Compression children | pass `-md` limits, account RSS | `compress.rs:262,305` |

### 1.4 Control flow

```
        ┌──────────────┐  every 250 ms (dedicated OS thread)
        │   sampler    │──────────────┐
        └──────────────┘              ▼
   CountingAlloc ──────────►  ┌───────────────┐   watch::Sender<Pressure>
                              │   Governor    │──────────┬──────────┬─────────┐
                              └───────────────┘          ▼          ▼         ▼
                                     ▲                 upload     PAR2      check
                                     │                permits   pass/depth  backlog
                              admission control
                              (reserve BEFORE alloc)
```

The sampler runs on a plain `std::thread`, **not** a tokio task: under memory
pressure the runtime may be saturated or blocked in `block_in_place`, and the one
component that must keep reporting is the monitor.

---

## 2. Monitoring strategy

### 2.1 Ceiling discovery (once, at startup)

The effective ceiling is the **minimum** of everything that can kill us:

```rust
pub struct Ceiling {
    pub address_space: Option<u64>, // RLIMIT_AS  — hard wall, zero tolerance
    pub cgroup_max:    Option<u64>, // cgroup v2 memory.max / v1 limit_in_bytes
    pub host_total:    u64,         // sysinfo
    pub explicit:      Option<u64>, // --memory-limit
    pub effective:     u64,         // min of the above, after per-source haircuts
}
```

Per-source haircuts differ because the failure modes differ:

- **`RLIMIT_AS`: 60%.** Crossing it aborts instantly. There is no reclaim, no swap,
  no warning. It also gets consumed by things we do not control (arena
  reservations), so headroom must be generous. *After* Phase 0 caps arenas this
  could rise, but do not raise it until measurements justify it.
- **cgroup `memory.max`: 75%.** Crossing it triggers reclaim first, then the cgroup
  OOM killer. Some slack exists.
- **Host available RAM: 70%.** Matches today's behaviour; softest of the three.

Read cgroup limits **directly** rather than via `sysinfo::cgroup_limits()`:

- v2: `/sys/fs/cgroup/memory.max`, `memory.current`, `memory.pressure`
- v1: `/sys/fs/cgroup/memory/memory.limit_in_bytes`, `memory.usage_in_bytes`

Direct reads are cheaper, and — more importantly — give access to **PSI**
(`memory.pressure`, or `/proc/pressure/memory`), which `sysinfo` does not expose.
Keep `sysinfo` for host RAM; it is already a dependency.

Resolve the cgroup path from `/proc/self/cgroup` rather than assuming the root — on a
seedbox `pesto` sits in a nested user slice and the root's limit is not the one that
applies.

### 2.2 What to sample, and how often

**Every 250 ms**, one `read()` of `/proc/self/statm` — 7 integers, one line, no
parsing of the ~55 lines `/proc/self/status` requires. Fields 0 and 1 are VmSize and
VmRSS in pages.

**Every 1 s**: cgroup `memory.current` and PSI `some avg10`.

**Continuously, free**: `CountingAlloc`'s live-heap counter.

PSI deserves emphasis for requirement 2 ("detect pressure early"). `some avg10` is the
percentage of the last 10 s in which at least one task stalled on memory. It rises
*before* the OOM killer engages and is the single best early-warning signal available
on cgroup v2. Treat `avg10 > 10` as Elevated and `> 30` as Critical regardless of what
the byte counters say.

### 2.3 Pressure state machine

Levels, evaluated against `effective` ceiling, with **hysteresis** (enter at the
threshold, leave at threshold − 10 points) to avoid oscillation:

| Level | Enter at | Behaviour |
|---|---|---|
| Normal | < 60% | Full speed |
| Elevated | ≥ 60% or PSI avg10 > 10 | Stop growing; trim channel depths; log |
| Critical | ≥ 75% or PSI avg10 > 30 | Shed connections, pause PAR2 feed, dump state |
| Emergency | ≥ 90% | Minimum viable config; refuse all non-essential allocation |

Ratchet rule: **de-escalate one level at a time, and no faster than once per 5 s.**
Recovering instantly to Normal after a dip is how you get a sawtooth that spends half
its time in Critical.

---

## 3. Control mechanisms

### 3.1 Preventive (largest win, lowest risk — do this first)

These reduce the *floor* rather than reacting to pressure, and per §0.3 they address
the dominant term.

```rust
// tune.rs — must run before any thread spawns or any arena is created.
pub fn apply(cfg: &TuneConfig) {
    #[cfg(all(unix, target_env = "gnu"))]
    unsafe {
        // Cap per-core arena VA reservations. Measured: 1222 MiB -> 326 MiB VmSize
        // at 64 threads. Trades a little allocator contention for ~8 GiB of address
        // space on a 128-core host.
        libc::mallopt(libc::M_ARENA_MAX, cfg.arena_max as i32);
        // Return freed memory to the OS more eagerly.
        libc::mallopt(libc::M_TRIM_THRESHOLD, 16 * 1024 * 1024);
        // Prefer mmap for large blocks so they are unmapped on free rather than
        // fragmenting the arena.
        libc::mallopt(libc::M_MMAP_THRESHOLD, 4 * 1024 * 1024);
    }
}
```

`libc` is already a `[target.'cfg(unix)'.dependencies]` entry. Guard on
`target_env = "gnu"` so the musl build skips it (musl has no `mallopt`).

Alongside it, replace `#[tokio::main]` with an explicit builder:

```rust
tokio::runtime::Builder::new_multi_thread()
    .worker_threads(cfg.worker_threads)      // min(ncores, 16) — this is an I/O
                                             // bound workload; 128 workers buys
                                             // nothing and costs 256 MiB of stacks
    .max_blocking_threads(cfg.blocking_threads)  // 32, not the 512 default
    .thread_stack_size(1 << 20)              // 1 MiB; default 2 MiB is generous
    .enable_all()
    .build()?
```

and cap the rayon pool similarly at `poster/mod.rs:1286` (`.stack_size(1 << 20)`).

Rough saving on a 128-core box: **~8 GiB of address space**, for no throughput loss
on a network-bound workload. This is the single highest-value change in this document.

### 3.2 Admission control (prevents the spike that sampling cannot catch)

Sampling at 250 ms cannot stop a single 4 GiB allocation. So the few sites that
allocate big must **reserve against the budget before allocating**:

```rust
// Returns Err if the budget cannot accommodate it, after waiting up to `timeout`
// for other stages to release. Never blocks the runtime — it is an async wait.
let guard = governor.reserve(Stage::Par2, bytes, timeout).await?;
let buf = guard.try_alloc_vec(bytes)?;   // try_reserve under the hood
```

Sites that need this (they are few — this is why the approach is tractable):

1. PAR2 pass working set — `poster/mod.rs:1685`
2. `Par2Worker` channel buffers — `poster/mod.rs:1792-1796`
3. `RecoveryEncoder` flush queue — `poster/mod.rs:1780`
4. Article bodies in `PostTask::data` — `poster/mod.rs:328-332`
5. Repost read buffer — `check.rs:574` (`vec![0u8; read_len]`)

### 3.3 Reactive backpressure per stage

**Upload concurrency.** Today `worker_count` connections are spawned once and the
channel is `worker_count * 2` (`poster/mod.rs:966`). Replace with a `Semaphore` whose
permit count the governor resizes:

```rust
Normal    => permits = configured_connections
Elevated  => permits = configured * 3 / 4
Critical  => permits = configured / 2
Emergency => permits = 4                     // floor: never stall to zero
```

Shedding permits does **not** close connections — workers finish the article in hand
and then park on `acquire()`. This is important: dropping and reopening TLS
connections under pressure would allocate *more*, not less.

**PAR2.** Two independent levers:

- *Between passes* (cheap, safe): re-evaluate `slices_per_pass` from the current
  budget. Costs an extra read pass over the input; never corrupts state.
- *Within a pass* (responsive): the feed loop at `poster/mod.rs:1846` checks pressure
  and awaits a `watch` change while Critical. The encoder's channels drain, RSS falls.
  Requires no encoder changes — it is just backpressure on the producer.

Do not try to shrink an in-flight `RecoveryEncoder`'s matrix; abandoning a pass wastes
all the work done in it.

**Check queue.** `check.rs:296` is an `unbounded_channel`, which is a genuine unbounded
liability at 136k segments. Make it bounded (`mpsc::channel(cap)`). Note the subtlety:
the sender is on the posting hot path, so blocking on a full check queue backpressures
*uploading* — which is the correct behaviour, but must be a bounded wait with a
cancellation check, not an unbounded one.

Also worth doing independently of pressure: `PostedSegment` (`poster/mod.rs:186-226`)
carries six heap strings and is stored twice (once in `results`, once cloned into the
check heap). At ~550 bytes × 136k segments × 2 that is ~150 MiB. `Arc<str>` for the
repeated `from`/`subject_name`/`file_path` fields, or interning them, would cut most
of it. Cheap, contained, and helps every run rather than only pressured ones.

### 3.4 Graceful degradation (requirement 5)

Convert the abort paths into error paths:

- `Vec::try_reserve` / `try_reserve_exact` at the five sites in §3.2. `try_reserve`
  returns `Result` instead of calling `handle_alloc_error`.
- On `TryReserveError`: drop to the next-lower configuration (halve the pass size,
  halve the channel depth) and retry once. Only if the retry fails does the run
  fail — and it fails with a *message*, which today it cannot do.
- Consider dropping `panic = "abort"` for the release profile, or at minimum
  installing an allocation-error hook. Note `#[alloc_error_handler]` is still
  unstable on stable Rust, so `try_reserve` is the practical route.

---

## 4. CLI and configuration design

### 4.1 Redefining `--memory-limit`

Today `--memory-limit` maps to `par2_memory_limit` (`bin/pesto.rs:180-182`,
`config/parse.rs:193`) and bounds only the PAR2 pass. Proposal:

| Flag | Meaning | Default |
|---|---|---|
| `--memory-limit <SIZE\|PCT\|auto>` | **Global** budget for the whole process | `auto` |
| `--par2-memory-limit <SIZE>` | PAR2 stage sub-budget (old behaviour) | derived |
| `--memory-report` | One-shot summary at exit | off |
| `--memory-trace` | Periodic pressure lines to the log | off |
| `--malloc-arenas <N>` | Escape hatch for §3.1 | 2 (glibc) |

Migration matters here: `--memory-limit 8G` currently means "PAR2 may use 8 GiB" and
would come to mean "the whole process may use 8 GiB" — strictly more conservative, so
existing invocations get safer rather than more crash-prone. Still, emit a one-line
notice on first use so the change is not silent, and keep
`posting.par2_memory_limit` in TOML working as the stage override.

Accept `70%` as well as `8G`. The existing parser is `parse_upload_rate`
(`config/parse.rs:198`) — percentages need adding.

### 4.2 `auto` for seedboxes

```
effective = min(
    RLIMIT_AS      × 0.60,   // if set
    cgroup_max     × 0.75,   // if confined
    host_available × 0.70,
) clamped to [512 MiB, 32 GiB]
```

Stage split of the effective budget:

| Stage | Share | Notes |
|---|---|---|
| PAR2 | 60% | Largest single consumer; already tuned this way |
| Upload | 25% | `connections × depth × article_size` |
| Check | 10% | Queue + repost buffers |
| Reserve | 5% | NZB assembly, metadata, fragmentation slack |

Shares are ceilings, not reservations: an idle stage's budget is lendable, which is
what makes `--par2-before-upload` (where no connections are open) efficient — the
existing `active_connections: 0` special case at `poster/mod.rs:1586` generalises
naturally into this model.

---

## 5. Implementation phases

Each phase is independently shippable and independently valuable.

### Phase 0 — Reduce the floor ✅ **implemented**

No monitoring, no policy — just stop wasting address space.

1. ✅ New `pesto::memory` module: `VmStats` (`/proc/self/statm`),
   `address_space_limit` (`RLIMIT_AS`, moved from `poster/mod.rs`),
   `address_space_peak` (`VmPeak`), `ThreadTuning`, `tune_allocator`.
2. ✅ `mallopt(M_ARENA_MAX, 2)` + trim/mmap thresholds before any thread spawns —
   glibc only, compiles out on musl.
3. ✅ `#[tokio::main]` replaced with an explicit builder: workers capped at
   `min(ncores, 16)`, `max_blocking_threads(64)`, 1 MiB stacks. The attribute left
   no room to run `tune_allocator` before the first thread, which is a hard
   requirement on glibc.
4. ✅ Rayon keeps its physical-core thread count (PAR2 wants those) but drops to
   1 MiB stacks.
5. ✅ Startup line (usage, ceiling, tuning) and exit line (`VmPeak` vs ceiling,
   with a warning above 80%) at INFO.
6. ❌ *Dropped:* bounding the check channel — see the update at the top; it would
   have been inert.

Overrides for odd hosts, no rebuild needed: `PESTO_WORKER_THREADS`,
`PESTO_BLOCKING_THREADS`, `PESTO_THREAD_STACK_KIB`.

**Measured: 803.5 MiB → 443.3 MiB peak address space** on a 128-thread profile.
Deploy and re-run the 100 GiB post before starting Phase 1 — that result determines
whether the remaining phases are needed at all.

### Phase 1 — Observability (~1–2 days, no behaviour change)

`CountingAlloc`, `/proc/self/statm` + cgroup + PSI sampler, `Ceiling` discovery,
`--memory-report` / `--memory-trace`, pressure levels **computed and logged but not
acted on**. Run real 100 GiB posts and collect the numbers before writing policy —
this is what tells you whether the §4.2 shares are right.

The budget model itself was re-derived ahead of this phase — see
[§9](#9-the-budget-model-validated-against-a-live-run), which supersedes the
`connection_overhead_reserve` note that used to sit here. The remaining Phase 1
work is the sampler and attribution, which is what turns §9's flat constants into
measured per-segment figures.

### Phase 2 — Unified budget (~2–3 days)

`--memory-limit` becomes global; stage sub-budgets; PAR2 sizing moves from the ad-hoc
calculation at `poster/mod.rs:1645-1685` to `governor.budget(Stage::Par2)`. Still no
dynamic reaction — but every stage is now bounded by a single coherent number instead
of one stage being bounded and the rest unmanaged.

### Phase 3 — Soft backpressure (~2–3 days)

`watch` channel, upload semaphore resizing, PAR2 producer pause at Critical, dynamic
channel depths, hysteresis and the de-escalation ratchet.

### Phase 4 — Hard limits and graceful degradation (~2–3 days)

`try_reserve` at the five allocation sites, retry-at-lower-config, structured
"degraded" progress events so the TUI can show *why* throughput dropped, memory-state
dump on Critical.

### Phase 5 — Child processes (~1–2 days)

Read children's RSS from `/proc/<pid>/statm`; pass `-md<N>` to rar and `-md=<N>` to 7z
derived from the compression budget; include children in the accounting during the
compression stage.

---

## 6. Edge cases and risks

**Rapid spikes.** Sampling cannot catch a single large allocation between ticks. This
is why §3.2 admission control (reserve-before-allocate) exists and why it, not the
sampler, is the real safety mechanism. Treat the sampler as advisory.

**The gap between live heap and VmSize.** Fragmentation and arena reservations mean
VmSize can climb while live heap is flat. Always drive `RLIMIT_AS` decisions from
VmSize; use live heap only to attribute *which stage* is responsible.

**External tools ignore our budget.** rar/7z honour only their own `-md` dictionary
settings. Cap them explicitly and account their RSS; do not attempt to `RLIMIT_AS`
them — an archiver that dies mid-volume is worse than one that is slow.

**OOM killer.** Under a cgroup, the kernel kills on `memory.max`, not on our budget,
and it kills the largest RSS in the cgroup — likely `pesto` itself. PSI is the early
warning. Do **not** lower `oom_score_adj` for `pesto` to protect it: on a shared
seedbox that just redirects the kill to someone else's process.

**Very low free memory.** Below the 512 MiB floor, refuse to start with a clear
message rather than starting and aborting at 40% through a 100 GiB encode. A fast,
explicit failure is strictly better than a slow implicit one.

**Multiple pesto instances.** Two instances each independently claiming 70% of RAM
oversubscribe by 40%. `RLIMIT_AS` is per-process so it is unaffected, but cgroup and
host budgets are not. Deriving pressure from cgroup `memory.current` (which counts
*all* processes in the cgroup) handles this automatically and is the reason to prefer
it over per-process RSS for the cgroup ceiling. A shared lock file coordinating an
explicit split is possible but is probably over-engineering; revisit only if the
cgroup-current signal proves insufficient in practice.

**Throughput regressions.** Capping tokio workers and arenas could in principle cost
throughput. Posting is network-bound at 30–60 connections, so I expect no measurable
loss — but benchmark it, because it is the main thing Phase 0 could plausibly get
wrong.

---

## 7. Testing strategy

**Unit.** Ceiling arithmetic and haircuts; pressure state machine including hysteresis
and the de-escalation ratchet (pure function over a synthetic sample sequence — no
real memory needed); percentage parsing in `config/parse.rs`.

**Fault injection.** A test-only `GlobalAlloc` wrapper that fails allocations past a
configurable threshold. This is the only way to test the §3.4 degradation paths
deterministically, and it must be feature-gated so it never ships.

**Constrained-environment integration.** These are the tests that would actually have
caught the reported bugs:

```bash
# Reproduce the address-space wall deterministically.
( ulimit -v 4194304; ./target/release/pesto ... )     # 4 GiB

# Reproduce the cgroup wall.
systemd-run --user --scope -p MemoryMax=2G ./target/release/pesto ...
```

Assert: process exits 0, or fails with an actionable message — **never** `SIGABRT`.
Capture peak VmSize from `/proc/self/status` `VmPeak` and assert it stayed under the
ceiling.

**Regression metric.** Record `VmPeak`, peak live heap, pressure-level time
distribution, and articles/sec for a fixed synthetic 20 GiB post. Track across
releases; a jump in `VmPeak` at constant throughput is the signature of this class of
bug returning.

Per `CLAUDE.md`, none of this may touch a real NNTP server — drive it against
`examples/mock_nntp_server.rs`, as the existing integration tests do.

**Manual validation on ultra.cc.** The 100 GiB / 60-connection case, with
`--memory-trace` on, on **both** the glibc and musl binaries. §0.3 predicts the two
will look very different before Phase 0 and nearly identical after it — that is a
falsifiable prediction and worth checking explicitly.

---

## 8. Recommended priority order

| # | Work | Effort | Risk | Value |
|---|---|---|---|---|
| 1 | **Phase 0** — arenas, thread/stack caps, bounded check channel | ½ day | Low | **Very high** — likely fixes the reported crashes outright |
| 2 | Phase 1 — measurement and reporting | 1–2 d | None | High — everything after this depends on real numbers |
| 3 | `PostedSegment` slimming (§3.3) | ½ day | Low | Medium — ~150 MiB, helps every run |
| 4 | Phase 2 — unified budget | 2–3 d | Medium | High — one coherent number instead of one managed stage |
| 5 | Phase 4 — `try_reserve` degradation | 2–3 d | Medium | High — turns aborts into messages |
| 6 | Phase 3 — dynamic backpressure | 2–3 d | Medium | Medium — mostly redundant once 1–5 land |
| 7 | Phase 5 — child processes | 1–2 d | Low | Low — narrow window, no PAR2/upload overlap |

Phase 3 is deliberately ranked below Phase 4. Dynamic backpressure is the most
visible part of the brief, but once the floor is lowered (Phase 0), the budget is
coherent (Phase 2), and large allocations are fallible (Phase 4), there is much less
left for it to do. Build it if measurements from Phase 1 show it is needed — not
before.

**Total: ~10–14 days** for all phases, but the first half-day carries most of the
value. Do Phase 0, deploy it to ultra.cc, and re-run the 100 GiB post before
committing to the rest.

---

## 9. The budget model, validated against a live run

A complete production run on ultra.cc — 83.4 GiB, 116 619 segments, 116 603 posted
with 0 failures and 0 missing, 40:55 wall clock — was instrumented end to end. It is
the first hard data this design has, and it corrected two things this document
previously got wrong.

### 9.1 What the run measured

| Phase | VmPeak | % of the 9.54 GiB `RLIMIT_AS` |
|---|---:|---:|
| PAR2 pass 1 | 4.98 GiB | 52.2% |
| PAR2 pass 2 | 7.15 → 7.38 GiB | 75.0 → 77.4% |
| **Tail (final posting, results, NZB)** | **8.13 GiB** | **85.2%** |

Two findings, both of which invalidate earlier assumptions here:

**The PAR2 passes are not the high-water mark.** The tail of the run adds
**+0.75 GiB** over the PAR2 peak — the accumulated `results` vector, the check
queue's heap, and NZB assembly at 116 k segments. §0.5 estimated that at ~150 MiB;
it is 5x larger, and unlike PAR2 it is paid on *every* run, single-pass or not.

**Memory is retained across passes.** VmPeak stepped +2.17 GiB at the pass 1 → 2
transition against a 4.15 GiB pass working set — 52% retained. The pass loop drops
each `Par2Worker` before building the next, so this is not a leak: it is musl not
returning freed spans to the OS, plus the next pass's differently-shaped allocations
not fitting the holes left behind. `RLIMIT_AS` counts the retained mapping anyway.

### 9.2 The corrected model

```text
peak ≈ reserve + budget × PASS_WORKING_SET_FACTOR × (1 + retention)
```

`PASS_WORKING_SET_FACTOR = 1.25` (the encoder's flush queue is `memory_limit / 4` on
top of the recovery buffers); `retention = 0.55` for multi-pass and **0** for
single-pass — a run that never starts a second pass cannot be holding a first pass's
memory. Solving for `budget` under `peak ≤ ceiling × 0.85` gives the two branches now
implemented in `address_space_budget`.

Predicted total peak 8.04 GiB vs **8.13 GiB observed — 1.1% error**.

### 9.3 Why the obvious fix was wrong

The `32 MiB × threads` reserve *was* a ~32x over-estimate of a stack that Phase 0
measured at 1 MiB. But it was silently compensating for the retention term the
formula omitted — two errors cancelling. Correcting only the constant, as this
document previously recommended, raises the budget to 4.19 GiB/pass and pushes
predicted peak to **8.96 GiB, 94% of the ceiling**.

| | reserve | budget | passes | predicted peak |
|---|---:|---:|---:|---:|
| Before | 2.90 GiB | 3.32 GiB | 3 | 8.01 GiB (84.0%) |
| Naive fix (constant only) | 1.15 GiB | 4.19 GiB | 2 | **8.96 GiB (93.9%)** |
| Implemented (both terms) | 1.65 GiB | 3.33 GiB | 3 | 8.04 GiB (84.3%) |

Multi-pass behaviour is deliberately left ~unchanged: it is the configuration just
proven to survive. The gain is on the **single-pass budget, 3.32 → 5.17 GiB (+56%)**,
which is the cheapest way to avoid a pass transition — and therefore the retention
step — altogether.

Note that this did **not** reduce the pass count for the measured workload (92
slices/pass against the 98 needed for two passes), so the "saves a full read" claim
made earlier in this document does not hold. Reducing passes safely requires
eliminating the retention itself — reusing the encoder's buffers across passes in
`parmesan` — not loosening the budget.

### 9.4 Revised priority

The tail (§9.1) now outranks the cross-pass retention: it is larger in the measured
run, it is paid on every run rather than only multi-pass ones, and the fix is
cheaper. `PostedSegment` (`poster/mod.rs:186`) carries six heap strings and is stored
twice — once in `results`, once cloned into the check heap. `Arc<str>` for the
repeated `from` / `subject_name` / `file_path` fields should reclaim most of it.
