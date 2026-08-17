//! Startup tuning that keeps the process's virtual address space bounded on
//! high-core-count machines, independent of `nproc`.
//!
//! # Background (issue #137)
//!
//! On a 128-core box with a restrictive `RLIMIT_AS` (`ulimit -v`, e.g. set
//! system-wide via PAM `limits.conf` on a shared host), `create()` could
//! panic on an allocation as small as ~220 MiB — nowhere near the configured
//! `--memory-limit`. The recovery buffer itself was never the problem;
//! [`RecoveryEncoder::new_smart`](crate::encoder::RecoveryEncoder::new_smart)
//! only ever allocates `recovery_count × slice_size` bytes for the pass
//! currently running, and `ops::ingest_files` streams input in fixed 8 MiB
//! chunks rather than reading whole files. What actually consumed the
//! address space was the *thread fan-out* that ran before either of those:
//!
//! - `#[tokio::main]`'s default multi-thread runtime spawns one worker
//!   thread per core (~128 here), even though this binary's async layer
//!   only ever drives one file through [`crate::ops::ingest_files`] at a
//!   time — a handful of workers is enough.
//! - glibc's malloc creates up to `8 × ncores` per-thread arenas by
//!   default, each reserving tens of MiB of address space (nearly RSS-free,
//!   but counted in full against `RLIMIT_AS`, and never returned). With
//!   ~128 tokio workers plus a rayon pool also sized to ~128 cores
//!   (`performance_core_count`, in `main.rs`) both contending on malloc,
//!   that alone can exceed a single-digit-GiB ceiling before a single PAR2
//!   slice is accounted for.
//!
//! `parpar` and `par2cmdline` don't hit this because neither fans out
//! anywhere near that many OS threads.
//!
//! The fix here only caps the *incidental* fan-out (tokio's own worker/
//! blocking pools, and glibc's arena count) — never the rayon pool, which
//! is the genuinely CPU-bound stage and stays sized to physical cores via
//! `performance_core_count` for throughput.

/// Cap glibc malloc's per-thread arena reservations.
///
/// No-op on musl (no `mallopt`, no per-core arenas) and on non-Unix
/// targets. Must run before any thread is spawned: arenas are created
/// lazily on first contended allocation from a new thread and are never
/// reclaimed afterwards, so this has to be the first thing `main` does —
/// before the tokio runtime is built and before the rayon pool.
///
/// `M_ARENA_MAX = 2` trades some allocator contention for address space.
/// That's the right trade here: `parmesan`'s hot path allocates per slice,
/// not per byte, so arena contention is far below the noise floor, while
/// uncapped arenas are what turned a 128-core machine's normal core count
/// into an address-space-exhausting `create()` in issue #137.
pub fn tune_allocator() {
    #[cfg(all(unix, target_env = "gnu"))]
    {
        // SAFETY: `mallopt` takes two ints and only adjusts allocator
        // tunables; it has no memory-safety preconditions. Called before any
        // thread is spawned (see `main`), which is a correctness (not
        // soundness) requirement for the setting to take effect.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 2);
        }
    }
}

/// Worker-thread cap for the tokio runtime, independent of core count.
///
/// `ops::ingest_files` processes one file at a time — one `spawn_blocking`
/// reader task feeding an await loop that hands slices to
/// [`crate::worker::Par2Worker`] — so it never needs more than a couple of
/// runtime workers. `#[tokio::main]`'s default of one worker per core buys
/// nothing here and was the largest single contributor to the fan-out in
/// issue #137.
const MAX_WORKER_THREADS: usize = 4;

/// Cap on tokio's blocking-thread pool.
///
/// A ceiling on worst-case growth, not a saving in the common case — tokio
/// creates blocking threads lazily on demand. Bounds the tail instead of
/// leaving tokio's default of 512.
const MAX_BLOCKING_THREADS: usize = 16;

/// Reduced stack size for tokio's own thread pools.
///
/// Rust's default is 2 MiB per thread; the async orchestration in this
/// binary (channel plumbing, `block_in_place` calls) needs nowhere near
/// that. The RS/SIMD work runs on rayon's threads, not these.
const THREAD_STACK_SIZE: usize = 1024 * 1024;

/// Build the multi-threaded tokio runtime `parmesan` runs on.
///
/// Replaces `#[tokio::main]`, whose defaults scale worker-thread count with
/// `nproc` — see the module docs. Must stay multi-threaded:
/// `ops::ingest_files` uses `block_in_place`, which panics on a
/// current-thread runtime.
pub fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(MAX_WORKER_THREADS)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_stack_size(THREAD_STACK_SIZE)
        .thread_name("parmesan-rt")
        .enable_all()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tune_allocator_is_safe_to_call() {
        // No-op on musl/non-Unix; on glibc it must not fault or abort.
        tune_allocator();
    }

    #[test]
    fn build_runtime_succeeds_and_is_bounded() {
        // Constructing the runtime is itself the regression check: a bad
        // `worker_threads`/`max_blocking_threads` value (e.g. 0) panics
        // inside `build()` rather than failing gracefully.
        let rt = build_runtime().expect("runtime should build with fixed, valid pool sizes");
        rt.block_on(async {});
    }
}
