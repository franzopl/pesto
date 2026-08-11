//! Process memory footprint: address-space accounting and startup tuning.
//!
//! # Why address space, not RSS
//!
//! On a shared seedbox the limit that actually kills `pesto` is almost never
//! physical RAM — it's `RLIMIT_AS` (`ulimit -v`), commonly applied per login
//! session via PAM `limits.conf`. Crossing it makes the allocator return
//! null, which Rust turns into `handle_alloc_error` → `SIGABRT`. With
//! `panic = "abort"` in the release profile nothing unwinds long enough to
//! flush a log line, so it looks like the process simply vanished:
//!
//! ```text
//! memory allocation of 1584548 bytes failed
//! Aborted
//! ```
//!
//! A *1.5 MB* request failing means address space was entirely exhausted, not
//! that memory was tight — which is why watching available RAM (or even
//! cgroup usage) does not predict this failure at all. Everything here is
//! therefore denominated in **VmSize** (what `RLIMIT_AS` is enforced
//! against), with RSS reported alongside only for diagnosis.
//!
//! # Where the address space actually goes
//!
//! Measured with a 128-thread harness, each thread allocating a little:
//!
//! | libc  | per-thread VA, default stacks | with 1 MiB stacks |
//! |-------|------------------------------:|------------------:|
//! | musl  |                      2.10 MiB |          1.10 MiB |
//! | glibc |                      9.52 MiB |          8.52 MiB |
//!
//! The two libcs fail for different reasons, so they need different fixes:
//!
//! * **musl** (what ultra.cc and the `pesto-linux-x86_64-musl` build use):
//!   per-thread VA is *purely* the thread stack, it scales linearly with
//!   thread count, and it is fully returned when threads exit. The only
//!   levers that matter are how many threads exist and how big their stacks
//!   are — both of which [`ThreadTuning`] bounds.
//! * **glibc**: dominated by malloc's per-core arenas (up to `8 × ncores`,
//!   each reserving a 64 MiB `HEAP_MAX_SIZE` region). Those reservations are
//!   nearly RSS-free but count in full against `RLIMIT_AS` and are *never*
//!   returned — ~1 GiB of VA floor on a 16-core box for a few MiB of live
//!   data, and ~8 GiB on a 128-core one. Shrinking stacks barely dents it;
//!   [`tune_allocator`] caps the arena count instead.
//!
//! Untuned, `pesto` requested `ncores` tokio workers (each running yEnc
//! inline), a rayon pool of `ncores`, and 2 MiB stacks throughout. Measured
//! end to end on the real binary with a 128-thread profile
//! (`--par2-only --threads 128`), capping workers at 16 and stacks at 1 MiB
//! takes **peak address space from 803.5 MiB to 443.3 MiB** — a 45%
//! reduction, before a single PAR2 slice is accounted for.
//!
//! Note that the tokio blocking-pool cap is a bound on the tail rather than a
//! saving; see `DEFAULT_MAX_BLOCKING_THREADS`.

pub mod alloc;
pub mod budget;
pub mod ceiling;
pub mod cgroup;
pub mod pressure;

pub use ceiling::Ceiling;
pub use pressure::{Pressure, PressureTracker};

/// A snapshot of this process's memory footprint, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmStats {
    /// Total virtual address space mapped — the figure `RLIMIT_AS` caps.
    pub vm_size: u64,
    /// Resident set size: the part actually backed by physical RAM.
    pub vm_rss: u64,
}

/// The high-water mark of this process's address space (`VmPeak`), in bytes.
///
/// The number worth reporting at the *end* of a run: a post that finished
/// with VmPeak at 95% of `RLIMIT_AS` succeeded by luck, and the next slightly
/// larger one will not. Instantaneous usage at exit — after PAR2 buffers are
/// freed — hides that entirely.
///
/// Only `/proc/self/status` carries it (`statm` has no peak field), but this
/// is read once per run, so the extra parsing cost is irrelevant here.
pub fn address_space_peak() -> Option<u64> {
    let raw = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("VmPeak:") {
            let kib: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kib * 1024);
        }
    }
    None
}

impl VmStats {
    /// Read the current footprint from `/proc/self/statm`.
    ///
    /// `statm` rather than `status`: it is a single line of seven integers
    /// (size and resident are fields 0 and 1, in pages) instead of ~55 lines
    /// of `key: value kB` that have to be scanned, which matters once this is
    /// sampled on an interval.
    ///
    /// Returns `None` on platforms without `/proc` (or if the format is not
    /// what we expect) — callers treat that as "unknown", never as zero.
    pub fn read() -> Option<Self> {
        let raw = std::fs::read_to_string("/proc/self/statm").ok()?;
        let mut fields = raw.split_ascii_whitespace();
        let size_pages: u64 = fields.next()?.parse().ok()?;
        let resident_pages: u64 = fields.next()?.parse().ok()?;
        let page = page_size();
        Some(Self {
            vm_size: size_pages * page,
            vm_rss: resident_pages * page,
        })
    }
}

#[cfg(unix)]
fn page_size() -> u64 {
    // SAFETY: `sysconf` with a valid name has no preconditions and no
    // side effects; a negative return means "indeterminate", handled below.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 {
        n as u64
    } else {
        4096
    }
}

#[cfg(not(unix))]
fn page_size() -> u64 {
    4096
}

/// This process's own virtual address-space ceiling (`RLIMIT_AS` / `ulimit
/// -v`), when the platform enforces one. `None` means unlimited (or the
/// platform doesn't have the concept, e.g. Windows) — nothing to clamp
/// against.
///
/// Shared seedboxes commonly cap this per login session (PAM `limits.conf`)
/// well below host RAM, independently of any cgroup, so it's invisible to
/// `sysinfo`'s RAM/cgroup readings and has to be checked separately.
#[cfg(unix)]
pub fn address_space_limit() -> Option<u64> {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rlim` is a valid `libc::rlimit` output buffer; `getrlimit`
    // only reads `RLIMIT_AS` and writes the current/max values into it.
    let ok = unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut rlim) == 0 };
    if !ok || rlim.rlim_cur == libc::RLIM_INFINITY {
        return None;
    }
    // `rlim_t` is `u64` on Linux but a narrower type on some other Unixes;
    // the cast is a no-op there and load-bearing here.
    #[allow(clippy::unnecessary_cast)]
    Some(rlim.rlim_cur as u64)
}

#[cfg(not(unix))]
pub fn address_space_limit() -> Option<u64> {
    None
}

/// Cap glibc malloc's per-core arena reservations.
///
/// No-op on musl (which has no `mallopt` and no per-core arenas) and on
/// non-Unix; see the module docs for why the two libcs need different
/// treatment. Must run before any thread is spawned — arenas are created
/// lazily on first contended allocation from a new thread, and cannot be
/// reclaimed afterwards.
///
/// Measured effect on glibc, 128 threads: VmSize 1481 MiB → 585 MiB.
///
/// `M_ARENA_MAX = 2` trades some allocator contention for a large amount of
/// address space. That trade is right here because `pesto`'s hot path
/// allocates per *article* (a few thousand allocations a second at most), not
/// per byte, so arena contention is far below the noise floor — while the
/// address space it buys back is the difference between finishing a 100 GiB
/// post and aborting mid-encode.
pub fn tune_allocator() {
    #[cfg(all(unix, target_env = "gnu"))]
    {
        // SAFETY: `mallopt` takes two ints and only adjusts allocator
        // tunables; it has no memory-safety preconditions. Called before any
        // thread is spawned (see `main`), which is a correctness (not
        // soundness) requirement for the setting to take effect.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 2);
            // Return freed memory to the OS more eagerly than the adaptive
            // default, so long runs don't ratchet their footprint upward.
            libc::mallopt(libc::M_TRIM_THRESHOLD, 16 * 1024 * 1024);
            // Serve large blocks (PAR2 slices, article buffers) with mmap so
            // they are unmapped on free instead of fragmenting an arena.
            libc::mallopt(libc::M_MMAP_THRESHOLD, 4 * 1024 * 1024);
        }
    }
}

/// Default cap on tokio worker threads.
///
/// Posting is network-bound: 30-60 TLS connections moving articles, with
/// yEnc encoding done inline on the worker task. yEnc runs at GB/s per core,
/// so even a saturated gigabit link needs well under one core of encode
/// capacity — `ncores` workers on a 128-core seedbox buys nothing and costs
/// ~270 MiB of address space in stacks. PAR2, the genuinely CPU-bound stage,
/// has its own rayon pool sized to physical cores and is unaffected.
const DEFAULT_MAX_WORKER_THREADS: usize = 16;

/// Default cap on the tokio blocking pool.
///
/// This is a **ceiling on worst-case growth, not a saving**: tokio creates
/// blocking threads on demand, and measurement confirms startup address space
/// is identical at 512 and at 64. Lowering it from tokio's default of 512
/// therefore costs nothing in the common case and bounds the tail — 512
/// threads that all happened to be live at once would be ~560 MiB of address
/// space on musl, which against a seedbox `ulimit -v` is the difference
/// between finishing and aborting.
///
/// `pesto` uses `spawn_blocking` for a handful of coarse operations
/// (compression, NFO generation, hooks) and `block_in_place` when feeding
/// PAR2 slices; 64 is far more than either needs, while leaving generous room
/// for `block_in_place` replacement workers.
const DEFAULT_MAX_BLOCKING_THREADS: usize = 64;

/// Default thread stack size for pools we control.
///
/// Rust's default is 2 MiB. 1 MiB halves the per-thread address-space cost
/// while staying well clear of what async tasks, yEnc encoding and the PAR2
/// SIMD kernels actually use. Deliberately *not* pushed to 512 KiB: the
/// measured extra saving is ~40 MiB across all pools, which is not worth
/// carrying any stack-overflow risk in SIMD code.
const DEFAULT_THREAD_STACK: usize = 1024 * 1024;

/// How many threads `pesto` asks for, and how much stack each gets.
///
/// Every field can be overridden by an environment variable, so a run on an
/// unusual host can be re-tuned without a rebuild:
///
/// | Field | Variable |
/// |---|---|
/// | [`worker_threads`](Self::worker_threads) | `PESTO_WORKER_THREADS` |
/// | [`max_blocking_threads`](Self::max_blocking_threads) | `PESTO_BLOCKING_THREADS` |
/// | [`thread_stack_size`](Self::thread_stack_size) | `PESTO_THREAD_STACK_KIB` |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadTuning {
    pub worker_threads: usize,
    pub max_blocking_threads: usize,
    pub thread_stack_size: usize,
}

impl ThreadTuning {
    /// Derive the tuning for this host, applying environment overrides.
    pub fn detect() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self {
            worker_threads: env_usize("PESTO_WORKER_THREADS")
                .unwrap_or_else(|| cores.min(DEFAULT_MAX_WORKER_THREADS))
                .max(1),
            max_blocking_threads: env_usize("PESTO_BLOCKING_THREADS")
                .unwrap_or(DEFAULT_MAX_BLOCKING_THREADS)
                .max(1),
            thread_stack_size: env_usize("PESTO_THREAD_STACK_KIB")
                .map(|kib| kib * 1024)
                .unwrap_or(DEFAULT_THREAD_STACK)
                .max(256 * 1024),
        }
    }

    /// Build the multi-threaded tokio runtime `pesto` runs on.
    ///
    /// Replaces `#[tokio::main]`, whose defaults (`ncores` workers, 512
    /// blocking threads, 2 MiB stacks) are the single largest avoidable
    /// consumer of address space on a many-core seedbox. Must stay
    /// multi-threaded: the PAR2 feed path uses `block_in_place`, which panics
    /// on a current-thread runtime.
    pub fn build_runtime(&self) -> std::io::Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(self.worker_threads)
            .max_blocking_threads(self.max_blocking_threads)
            .thread_stack_size(self.thread_stack_size)
            .thread_name("pesto-rt")
            .enable_all()
            .build()
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|n| *n > 0)
}

/// Coarse stage of a run, used to attribute the address-space high-water mark
/// to the work that caused it.
///
/// Knowing a run peaked at 85% of the ceiling is only half the answer; the
/// actionable half is *where*. A production run that peaked at 8.13 GiB was
/// assumed to have done so inside the PAR2 passes — they are the largest
/// single budgeted allocation — but the passes topped out at 7.38 GiB and the
/// remaining 0.75 GiB appeared afterwards, in a stage nothing was measuring.
/// Guessing at which one cost two wrong hypotheses; this records it instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Startup = 0,
    Compress = 1,
    Par2 = 2,
    Posting = 3,
    Check = 4,
    Nzb = 5,
    Nfo = 6,
    Hooks = 7,
    Shutdown = 8,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Startup => "startup",
            Phase::Compress => "compress",
            Phase::Par2 => "par2",
            Phase::Posting => "posting",
            Phase::Check => "check",
            Phase::Nzb => "nzb",
            Phase::Nfo => "nfo",
            Phase::Hooks => "hooks",
            Phase::Shutdown => "shutdown",
        }
    }

    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Compress,
            2 => Phase::Par2,
            3 => Phase::Posting,
            4 => Phase::Check,
            5 => Phase::Nzb,
            6 => Phase::Nfo,
            7 => Phase::Hooks,
            8 => Phase::Shutdown,
            _ => Phase::Startup,
        }
    }
}

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

static CURRENT_PHASE: AtomicU8 = AtomicU8::new(0);
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_PHASE: AtomicU8 = AtomicU8::new(0);
static PEAK_PRESSURE: AtomicU8 = AtomicU8::new(Pressure::Normal as u8);
static TRACE_ENABLED: AtomicBool = AtomicBool::new(false);
static REPORT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable `--memory-report`'s exit-time summary block.
///
/// A module-level flag rather than a return value threaded back out of
/// `run()`: `main()` logs the exit summary after `run()` returns regardless
/// of which of its several return points was taken (including error paths),
/// and this mirrors `start_sampler`'s own `trace` flag for the same reason.
pub fn set_report_enabled(enabled: bool) {
    REPORT_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether `--memory-report` was requested — see [`set_report_enabled`].
pub fn report_enabled() -> bool {
    REPORT_ENABLED.load(Ordering::Relaxed)
}

/// Sentinel for "no explicit `--memory-limit`" in [`EXPLICIT_MEMORY_LIMIT`].
/// No real invocation sets an 18 exabyte budget.
const NO_EXPLICIT_LIMIT: u64 = u64::MAX;
static EXPLICIT_MEMORY_LIMIT: AtomicU64 = AtomicU64::new(NO_EXPLICIT_LIMIT);

/// Record the resolved global `--memory-limit` (`Config::memory_limit`), so
/// `main()`'s exit-time `--memory-report` can rediscover the same
/// [`Ceiling`] `run()` used for startup/sampling — `Config` itself isn't
/// available across that call boundary. Mirrors [`set_report_enabled`].
pub fn set_explicit_memory_limit(limit: Option<u64>) {
    EXPLICIT_MEMORY_LIMIT.store(limit.unwrap_or(NO_EXPLICIT_LIMIT), Ordering::Relaxed);
}

/// The value recorded by [`set_explicit_memory_limit`], if any.
pub fn explicit_memory_limit() -> Option<u64> {
    match EXPLICIT_MEMORY_LIMIT.load(Ordering::Relaxed) {
        NO_EXPLICIT_LIMIT => None,
        v => Some(v),
    }
}

/// How often the sampler reads `/proc/self/statm`.
///
/// This is the resolution of peak attribution, so it wants to be short: a
/// 3-second `--par2-only` run at a 1 s interval credited its peak to `startup`
/// simply because the allocation and the release both fell between samples.
/// Four reads a second of a single-line pseudo-file is far below the noise
/// floor of anything `pesto` does.
const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Record that the run has entered `phase`.
///
/// A single relaxed store — safe to call from anywhere, including inside the
/// posting hot path, though callers only mark coarse boundaries.
pub fn set_phase(phase: Phase) {
    CURRENT_PHASE.store(phase as u8, Ordering::Relaxed);
}

/// Start the background sampler.
///
/// Deliberately a plain OS thread rather than a tokio task: under memory
/// pressure the runtime may be saturated or parked in `block_in_place`, and the
/// one component that must keep reporting is the thing watching memory.
///
/// The sampler always tracks the peak and the phase it occurred in — one
/// `/proc/self/statm` read per interval, far below the noise floor. `trace`
/// additionally logs every sample, which is opt-in because at one line per
/// second it would bury everything else in a long run.
///
/// Every fourth tick (~1 s) it also reads cgroup memory + PSI and re-evaluates
/// the pressure level against `ceiling`. Level *transitions* are logged
/// unconditionally — not gated on `trace` — because an escalation is exactly
/// the kind of thing worth surfacing by default; nothing yet reacts to it
/// (that's Phase 3, see `docs/memory-management.md`), this phase only reports
/// it.
pub fn start_sampler(trace: bool, ceiling: Ceiling) {
    TRACE_ENABLED.store(trace, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("pesto-memwatch".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let limit = address_space_limit();
            let mut tracker = PressureTracker::new(std::time::Instant::now());
            let mut tick: u32 = 0;
            loop {
                if let Some(stats) = VmStats::read() {
                    let phase = CURRENT_PHASE.load(Ordering::Relaxed);
                    // Keep the phase consistent with the peak it belongs to:
                    // only claim the phase when we actually raise the peak.
                    let prev = PEAK_BYTES.fetch_max(stats.vm_size, Ordering::Relaxed);
                    if stats.vm_size > prev {
                        PEAK_PHASE.store(phase, Ordering::Relaxed);
                    }
                    // Sample fast, log slow. Attribution accuracy is bounded by
                    // the sampling interval — a peak reached and released
                    // between two samples gets credited to whichever phase was
                    // current at the next one — but a trace at the sampling
                    // rate would be four lines a second of mostly identical
                    // output, so only every fourth sample is logged.
                    if tick.is_multiple_of(4) {
                        if TRACE_ENABLED.load(Ordering::Relaxed) {
                            let pct = limit
                                .map(|l| {
                                    format!(" ({:.1}%)", stats.vm_size as f64 / l as f64 * 100.0)
                                })
                                .unwrap_or_default();
                            tracing::info!(
                                phase = Phase::from_u8(phase).name(),
                                "memory: address space {}{pct}, RSS {}",
                                crate::progress::format_size(stats.vm_size),
                                crate::progress::format_size(stats.vm_rss),
                            );
                        }

                        let psi = cgroup::read_cgroup_memory().and_then(|c| c.psi_avg10);
                        let pct_of_ceiling =
                            stats.vm_size as f64 / ceiling.effective as f64 * 100.0;
                        if let Some(level) =
                            tracker.update(pct_of_ceiling, psi, std::time::Instant::now())
                        {
                            PEAK_PRESSURE.fetch_max(level as u8, Ordering::Relaxed);
                            let psi_text = psi
                                .map(|p| format!(", PSI avg10 {p:.1}%"))
                                .unwrap_or_default();
                            let msg = format!(
                                "memory pressure: {} ({:.1}% of effective ceiling {}{psi_text})",
                                level.name(),
                                pct_of_ceiling,
                                crate::progress::format_size(ceiling.effective),
                            );
                            match level {
                                Pressure::Normal | Pressure::Elevated => tracing::info!("{msg}"),
                                Pressure::Critical | Pressure::Emergency => {
                                    tracing::warn!("{msg}")
                                }
                            }
                        }
                    }
                }
                tick = tick.wrapping_add(1);
                std::thread::sleep(SAMPLE_INTERVAL);
            }
        })
        .ok();
}

/// The phase that was active when the sampler last raised the peak, if the
/// sampler ever observed one.
fn peak_phase() -> Option<Phase> {
    (PEAK_BYTES.load(Ordering::Relaxed) > 0)
        .then(|| Phase::from_u8(PEAK_PHASE.load(Ordering::Relaxed)))
}

/// The worst pressure level the sampler observed over the life of the run,
/// for `--memory-report`. `Normal` if the sampler never escalated (or was
/// never started).
pub fn peak_pressure() -> Pressure {
    Pressure::from_u8(PEAK_PRESSURE.load(Ordering::Relaxed))
}

/// One-line summary of the current footprint against the address-space
/// ceiling, for the startup and exit log lines.
///
/// Reports VmSize first and RSS second, deliberately: VmSize is what the
/// ceiling applies to, and a large gap between the two is itself the
/// diagnostic signal that allocator overhead (rather than live data) is
/// consuming the budget.
pub fn footprint_summary() -> String {
    let stats = VmStats::read();
    let limit = address_space_limit();
    let fmt = crate::progress::format_size;
    match (stats, limit) {
        (Some(s), Some(limit)) => {
            let pct = (s.vm_size as f64 / limit as f64) * 100.0;
            format!(
                "address space {} / {} ({pct:.1}%), RSS {}",
                fmt(s.vm_size),
                fmt(limit),
                fmt(s.vm_rss),
            )
        }
        (Some(s), None) => format!(
            "address space {} (no RLIMIT_AS), RSS {}",
            fmt(s.vm_size),
            fmt(s.vm_rss),
        ),
        (None, Some(limit)) => format!("address-space limit {} (usage unknown)", fmt(limit)),
        (None, None) => "memory usage unavailable".to_string(),
    }
}

/// One-line summary of the run's *peak* address-space use against the
/// ceiling, for the exit log line. See [`address_space_peak`] for why the
/// peak — not the final instantaneous figure — is the number that matters.
pub fn peak_summary() -> String {
    let fmt = crate::progress::format_size;
    match (address_space_peak(), address_space_limit()) {
        (Some(peak), Some(limit)) => {
            let pct = (peak as f64 / limit as f64) * 100.0;
            let note = if pct >= 80.0 {
                " — close to the limit; consider --memory-limit or a lower --connections/--threads"
            } else {
                ""
            };
            format!(
                "peak address space {} / {} ({pct:.1}%){}{note}",
                fmt(peak),
                fmt(limit),
                during(),
            )
        }
        (Some(peak), None) => {
            format!(
                "peak address space {}{} (no RLIMIT_AS)",
                fmt(peak),
                during()
            )
        }
        (None, _) => "peak memory usage unavailable".to_string(),
    }
}

/// ` during <phase>` when the sampler attributed the peak, else empty.
///
/// `VmPeak` from the kernel is authoritative for the *value*; the sampler is
/// what knows *when*. They can disagree slightly — the kernel sees every
/// instant, the sampler only its polling interval — so the phase is reported
/// as supporting detail next to the kernel's number rather than replacing it.
fn during() -> String {
    match peak_phase() {
        Some(p) => format!(" during {}", p.name()),
        None => String::new(),
    }
}

/// Multi-line `--memory-report` block, printed once at exit. Complements
/// [`peak_summary`] (the kernel's `VmPeak` against `RLIMIT_AS`) with the
/// three other things that only this run's own accounting knows: the full
/// ceiling breakdown, the live-heap-vs-`VmSize` gap (allocator/VA overhead),
/// and the worst pressure level reached.
pub fn report_summary(ceiling: &Ceiling) -> String {
    let fmt = crate::progress::format_size;
    let opt = |v: Option<u64>| v.map(fmt).unwrap_or_else(|| "none detected".to_string());

    let live_peak = alloc::live_bytes_peak();
    let vm_peak = address_space_peak();
    let gap = match vm_peak {
        Some(vm) if vm >= live_peak => format!(
            "gap {} (allocator/VA overhead)",
            fmt(vm.saturating_sub(live_peak))
        ),
        Some(_) => "gap unavailable (VmPeak below live heap — allocator not the process's actual global allocator)".to_string(),
        None => "gap unavailable (VmPeak unreadable)".to_string(),
    };

    format!(
        "memory report:\n  \
         ceiling: address-space {} | cgroup {} | host {} | effective {}\n  \
         live heap peak {}, VmPeak {} — {}\n  \
         pressure: worst level reached {}",
        opt(ceiling.address_space),
        opt(ceiling.cgroup_max),
        fmt(ceiling.host_total),
        fmt(ceiling.effective),
        fmt(live_peak),
        vm_peak
            .map(fmt)
            .unwrap_or_else(|| "unavailable".to_string()),
        gap,
        peak_pressure().name(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_stats_are_plausible() {
        // Only meaningful where /proc exists; elsewhere `read` returns None
        // and there is nothing to assert.
        if let Some(stats) = VmStats::read() {
            assert!(stats.vm_size > 0, "VmSize should be non-zero");
            assert!(
                stats.vm_rss <= stats.vm_size,
                "RSS ({}) cannot exceed VmSize ({})",
                stats.vm_rss,
                stats.vm_size
            );
        }
    }

    #[test]
    fn tuning_defaults_are_bounded() {
        let t = ThreadTuning::detect();
        assert!(t.worker_threads >= 1);
        assert!(
            t.worker_threads <= DEFAULT_MAX_WORKER_THREADS,
            "worker threads should be capped regardless of core count"
        );
        assert!(t.max_blocking_threads >= 1);
        assert!(
            t.thread_stack_size >= 256 * 1024,
            "stack floor guards against a hostile override"
        );
    }

    #[test]
    fn env_usize_rejects_junk_and_zero() {
        // Zero would mean "unbounded" to tokio for blocking threads and is a
        // panic for worker_threads, so it must never survive parsing.
        assert_eq!(env_usize("PESTO_DEFINITELY_UNSET_VAR_XYZ"), None);
    }

    #[test]
    fn footprint_summary_is_non_empty() {
        assert!(!footprint_summary().is_empty());
    }

    #[test]
    fn every_phase_has_a_stable_name_and_round_trips() {
        // The exit line's "during <phase>" is only useful if the discriminant
        // survives the AtomicU8 round trip; a mismatch would silently
        // misattribute the peak, which is worse than reporting nothing.
        let all = [
            Phase::Startup,
            Phase::Compress,
            Phase::Par2,
            Phase::Posting,
            Phase::Check,
            Phase::Nzb,
            Phase::Nfo,
            Phase::Hooks,
            Phase::Shutdown,
        ];
        for p in all {
            assert_eq!(
                Phase::from_u8(p as u8),
                p,
                "{} did not round trip",
                p.name()
            );
            assert!(!p.name().is_empty());
        }
        // Unknown discriminants must degrade to Startup, never panic.
        assert_eq!(Phase::from_u8(200), Phase::Startup);
    }

    #[test]
    fn set_phase_is_observable() {
        set_phase(Phase::Nzb);
        assert_eq!(
            Phase::from_u8(CURRENT_PHASE.load(Ordering::Relaxed)),
            Phase::Nzb
        );
        set_phase(Phase::Startup);
    }

    #[test]
    fn peak_summary_mentions_the_phase_once_a_peak_is_recorded() {
        // Simulate what the sampler does, without racing the real one.
        PEAK_BYTES.store(4096, Ordering::Relaxed);
        PEAK_PHASE.store(Phase::Nzb as u8, Ordering::Relaxed);
        assert!(
            peak_summary().contains("during nzb"),
            "peak_summary should attribute the peak: {}",
            peak_summary()
        );
        PEAK_BYTES.store(0, Ordering::Relaxed);
    }

    #[test]
    fn tune_allocator_is_safe_to_call() {
        // No-op on musl; on glibc it must not fault or abort.
        tune_allocator();
    }

    #[test]
    fn peak_pressure_defaults_to_normal_and_is_observable() {
        let before = PEAK_PRESSURE.load(Ordering::Relaxed);
        assert_eq!(peak_pressure(), Pressure::from_u8(before));
        PEAK_PRESSURE.fetch_max(Pressure::Critical as u8, Ordering::Relaxed);
        assert_eq!(peak_pressure(), Pressure::Critical);
        PEAK_PRESSURE.store(before, Ordering::Relaxed);
    }

    #[test]
    fn report_summary_includes_ceiling_and_pressure() {
        let ceiling = Ceiling::discover(None);
        let report = report_summary(&ceiling);
        assert!(report.contains("memory report:"));
        assert!(report.contains("ceiling:"));
        assert!(report.contains("pressure:"));
    }
}
