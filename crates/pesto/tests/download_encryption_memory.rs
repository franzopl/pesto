//! Encrypted article decoding must allocate by article size, not file geometry.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

mod support;
use pesto::crypto::{DownloadDecryptionAdapter, EncryptionSession, UploadEncryptionAdapter};
use pesto::yenc::PartSpec;
use support::fixture_kdf_input;

struct TrackingAllocator;
thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static MAX_ALLOCATION: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation(size: usize) {
    let _ = TRACK.try_with(|track| {
        if track.get() {
            let _ = MAX_ALLOCATION.try_with(|maximum| maximum.set(maximum.get().max(size)));
        }
    });
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn physical_multipart_wire(session: &Arc<EncryptionSession>, file_size: u64) -> Vec<u8> {
    // Adapter-only fixture (R11-PC-01: control-line helpers are no longer
    // public surface for tests). The physical-multipart `total=1` variant
    // of this geometry is covered in-crate by
    // `crypto::adapter::tests::geometry`; here the adapter-produced
    // multipart article exercises the same article-sized allocation seam.
    UploadEncryptionAdapter::new(session.clone())
        .encode_article(
            "memory.bin",
            file_size,
            PartSpec {
                number: 1,
                total: 2,
                offset: 4,
            },
            b"test",
            128,
            1,
            &mut Vec::new(),
        )
        .unwrap()
        .body
}

#[test]
fn tiny_multipart_of_large_file_allocates_by_article_size() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x42; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    // Warm the fixed 64-MiB KDF cache separately from per-article allocations.
    adapter
        .decode_article(&physical_multipart_wire(&session, 20))
        .unwrap();
    let wire = physical_multipart_wire(&session, 3u64 << 30);
    MAX_ALLOCATION.set(0);
    TRACK.set(true);
    let decoded = adapter.decode_article(&wire);
    TRACK.set(false);
    let maximum = MAX_ALLOCATION.get();
    eprintln!(
        "Maximum allocation: {maximum} bytes for a {}-byte article",
        wire.len()
    );
    let decoded = decoded.unwrap();
    assert_eq!(decoded.data, b"test");
    assert_eq!(
        (decoded.begin, decoded.end, decoded.file_size),
        (5, 8, 3u64 << 30)
    );
    // Line indexes and crypto working buffers may reserve a small multiple
    // of the wire length, but never capacity based on the whole file size.
    assert!(
        maximum <= wire.len() * 8,
        "allocated {maximum} bytes for a {}-byte article",
        wire.len()
    );
}
