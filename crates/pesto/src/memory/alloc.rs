//! A [`GlobalAlloc`] wrapper that tracks this process's live heap in real
//! time, for comparison against [`super::VmStats`].
//!
//! `VmSize` (what `RLIMIT_AS` is enforced against) and the process's actual
//! live data can diverge a great deal — glibc's per-core arena reservations
//! alone can account for several GiB of `VmSize` backed by almost no live
//! bytes (see the module docs on [`super`]). `live_bytes` is the other half
//! of that comparison: the allocator's own exact count of what `pesto`
//! itself is holding, with zero sampling lag. A large gap between the two is
//! the diagnostic signal that allocator/VA overhead, not live data, is
//! consuming the address-space budget.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

/// Bytes currently live on the heap, per the allocator's own bookkeeping.
///
/// Only meaningful once [`CountingAlloc`] has been declared as the process's
/// `#[global_allocator]` (see `bin/pesto.rs`) — otherwise this just reads two
/// atomics that nothing is updating.
pub fn live_bytes() -> u64 {
    LIVE.load(Relaxed)
}

/// High-water mark of [`live_bytes`] over the life of the process.
pub fn live_bytes_peak() -> u64 {
    PEAK.load(Relaxed)
}

/// `GlobalAlloc` wrapper that adds one relaxed `fetch_add`/`fetch_sub` per
/// allocation/deallocation to track [`live_bytes`] exactly.
///
/// Cost is ~1-2 ns per call — `pesto` allocates per-article, not per-byte, so
/// this is far below the noise floor of the hot path.
///
/// Deliberately **not** declared as `#[global_allocator]` in this crate: that
/// attribute is a whole-binary, one-per-binary choice, and `pesto` is also
/// consumed as a library by `upapasta`, `penne` and `sugo`. Doing it here
/// would silently force this allocator on every consumer. It's public so any
/// binary that wants it — starting with `pesto`'s own — can opt in itself.
pub struct CountingAlloc<A = System> {
    inner: A,
}

impl CountingAlloc<System> {
    pub const fn new() -> Self {
        Self { inner: System }
    }
}

impl Default for CountingAlloc<System> {
    fn default() -> Self {
        Self::new()
    }
}

fn record_grow(added: usize) {
    let live = LIVE.fetch_add(added as u64, Relaxed) + added as u64;
    PEAK.fetch_max(live, Relaxed);
}

fn record_shrink(removed: usize) {
    LIVE.fetch_sub(removed as u64, Relaxed);
}

// SAFETY: every method delegates the actual allocation work to `inner`,
// which upholds `GlobalAlloc`'s contract on its own; this wrapper only adds
// atomic bookkeeping around it and never touches the returned pointer or the
// size/alignment passed through to `inner`.
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAlloc<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.inner.alloc(layout) };
        if !ptr.is_null() {
            record_grow(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.inner.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record_grow(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.inner.dealloc(ptr, layout) };
        record_shrink(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { self.inner.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            let old_size = layout.size();
            if new_size >= old_size {
                record_grow(new_size - old_size);
            } else {
                record_shrink(old_size - new_size);
            }
        }
        new_ptr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `LIVE`/`PEAK` are process-global statics — that's the whole point of
    // `CountingAlloc` — but it means any test that reads a "before" snapshot
    // and later asserts against `before + N` needs exclusive access to them,
    // or a standalone instance's allocations in one test thread corrupt
    // another's expected count. `cargo test` runs test fns on separate OS
    // threads by default, so this lock (not `#[serial]`-style crates, to
    // avoid a new dependency for two tests) is what actually serializes them.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn live_bytes_and_peak_are_readable() {
        // Under `cargo test` the process's actual global allocator is the
        // default one, not `CountingAlloc` (that's only wired up in
        // `bin/pesto.rs`), so this only asserts the counters are sane —
        // `a_standalone_instance_tracks_its_own_allocations` below is what
        // exercises the accounting arithmetic itself.
        assert!(live_bytes_peak() >= live_bytes());
    }

    #[test]
    fn a_standalone_instance_tracks_its_own_allocations() {
        let _guard = TEST_LOCK.lock().unwrap();
        let alloc = CountingAlloc::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let before = live_bytes();
        unsafe {
            let ptr = alloc.alloc(layout);
            assert!(!ptr.is_null(), "test allocation must succeed");
            assert_eq!(live_bytes(), before + 64);
            assert!(live_bytes_peak() >= before + 64);
            alloc.dealloc(ptr, layout);
            assert_eq!(live_bytes(), before);
        }
    }

    #[test]
    fn realloc_growing_and_shrinking_both_update_live_bytes_correctly() {
        let _guard = TEST_LOCK.lock().unwrap();
        let alloc = CountingAlloc::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let before = live_bytes();
        unsafe {
            let ptr = alloc.alloc(layout);
            let grown = alloc.realloc(ptr, layout, 256);
            assert!(!grown.is_null());
            assert_eq!(live_bytes(), before + 256);

            let grown_layout = Layout::from_size_align(256, 8).unwrap();
            let shrunk = alloc.realloc(grown, grown_layout, 32);
            assert!(!shrunk.is_null());
            assert_eq!(live_bytes(), before + 32);

            let shrunk_layout = Layout::from_size_align(32, 8).unwrap();
            alloc.dealloc(shrunk, shrunk_layout);
            assert_eq!(live_bytes(), before);
        }
    }
}
