use super::*;

/// Storage for recovery accumulator buffers.
///
/// `Normal` holds one `Vec<u16>` per recovery block (the existing layout).
/// `Altmap` holds one `Vec<u8>` per recovery block in ALTMAP bit-plane format
/// (Phase 27d/27e); both variants occupy the same total memory.
/// `Shuffle2x` holds one `Vec<u8>` per recovery block in the Shuffle2x layout
/// (Phase 28a): lo-bytes in lane 0, hi-bytes in lane 1 of each 32-byte chunk.
pub(super) enum RecoveryBufferSet {
    Normal(Vec<Vec<u16>>),
    /// Each inner `Vec<u8>` has length `altmap_size(slice_words)` = `slice_words * 2`.
    /// Only ever constructed on x86_64 (see `new_altmap`); matched against
    /// generically elsewhere so it can't be `#[cfg(target_arch = "x86_64")]`
    /// itself without breaking those match arms on other targets.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Altmap(Vec<Vec<u8>>),
    /// Each inner `Vec<u8>` has length `shuffle2x_buffer_size(slice_words)` = `slice_words * 2`.
    /// Only ever constructed on x86_64 (see `new_shuffle2x`); same reasoning
    /// as `Altmap` above.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Shuffle2x(Vec<Vec<u8>>),
    /// Per-lane lo/hi split (Affine2x). Same footprint as Normal. Used by the
    /// AVX2+GFNI kernel; converted back in `finish`.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Affine2x(Vec<Vec<u8>>),
    /// Parpar Affine shuffle-prepare (64-byte groups). GFNI Affine kernel.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Affine(Vec<Vec<u8>>),
    /// Affine shuffle-prepare, dests interleaved by 4 KiB tile (parpar
    /// `memProcessing`: `out*chunk + round*numOutputs*chunk`).
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Affine512(Affine512Acc),
}

/// Affine512 recovery accumulators: one allocation, dests packed per tile.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) struct Affine512Acc {
    pub(super) data: Vec<u8>,
    pub(super) n_rec: usize,
    pub(super) slice_len: usize,
}

impl RecoveryBufferSet {
    /// Borrow the normal (u16) buffers.  Panics when called on the Altmap/Shuffle2x variant.
    pub(super) fn as_normal_mut(&mut self) -> &mut Vec<Vec<u16>> {
        match self {
            Self::Normal(b) => b,
            Self::Altmap(_)
            | Self::Shuffle2x(_)
            | Self::Affine2x(_)
            | Self::Affine(_)
            | Self::Affine512(_) => panic!("expected Normal recovery buffers"),
        }
    }

    /// Number of recovery blocks.
    #[allow(dead_code)]
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Normal(b) => b.len(),
            Self::Altmap(b) | Self::Shuffle2x(b) | Self::Affine2x(b) | Self::Affine(b) => b.len(),
            Self::Affine512(a) => a.n_rec,
        }
    }
}

/// Build `count` zero-filled buffers of `len` elements each, using fallible
/// allocation throughout: both the outer `Vec` (one slot per recovery block)
/// and every inner buffer are `try_reserve_exact`'d before being touched.
/// This is what `RecoveryEncoder`'s buffer matrix is built from — on a large
/// release this single allocation (`recovery_count × slice_size`) is the
/// biggest the whole `pesto`/`parmesan` process ever makes, which is exactly
/// why it needs to fail with an error instead of aborting the process.
pub(super) fn try_zeroed_buffers<T: Clone>(
    fill: T,
    len: usize,
    count: usize,
) -> Result<Vec<Vec<T>>, TryReserveError> {
    let mut out: Vec<Vec<T>> = Vec::new();
    out.try_reserve_exact(count)?;
    for _ in 0..count {
        let mut buf: Vec<T> = Vec::new();
        buf.try_reserve_exact(len)?;
        buf.resize(len, fill.clone());
        out.push(buf);
    }
    Ok(out)
}

/// Returns the size in bytes of one ALTMAP recovery buffer for `slice_words`
/// GF(2^16) words.  Equal to `slice_words * 2` — same footprint as a
/// `Vec<u16>` of `slice_words` elements.
///
/// # Panics
///
/// Panics if `slice_words` is not a multiple of 16.
pub fn altmap_buffer_size(slice_words: usize) -> usize {
    super::altmap::altmap_size(slice_words)
}

/// Returns the size in bytes of one Shuffle2x recovery buffer for `slice_words`
/// GF(2^16) words.  Equal to `slice_words * 2` — same footprint as normal layout.
///
/// # Panics
///
/// Panics if `slice_words` is not a multiple of 16.
pub fn shuffle2x_buffer_size(slice_words: usize) -> usize {
    super::shuffle2x::shuffle2x_buffer_size(slice_words)
}

/// Byte size of one Affine2x recovery buffer. Equal to `slice_words * 2`.
///
/// # Panics
///
/// Panics if `slice_words` is not a multiple of 16.
pub fn affine2x_buffer_size(slice_words: usize) -> usize {
    super::affine2x::affine2x_buffer_size(slice_words)
}

/// Byte size of one Affine (shuffle-prepare) recovery buffer.
pub fn affine_buffer_size(slice_words: usize) -> usize {
    super::affine::affine_buffer_size(slice_words)
}

/// Whether this CPU has the `flush_avx2_altmap` kernel, i.e. whether
/// [`RecoveryEncoder::new_altmap`] will actually keep the ALTMAP layout.
///
/// It needs AVX2 *and* the absence of GFNI: GFNI machines run a different
/// kernel and `build_dep_tables` returns `None` there, so `new_altmap` falls
/// back to the portable layout. The recovery data is the same either way — this
/// only tells a caller that wants to *measure* the ALTMAP kernel (a benchmark)
/// whether it would be measuring something else.
///
/// Advisory, not a guarantee: the constructor can still fall back if its 2 MiB
/// dependency table fails to allocate.
pub fn altmap_kernel_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx2") && !std::is_x86_feature_detected!("gfni")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Whether this CPU has the `flush_avx2_shuffle2x` kernel, i.e. whether
/// [`RecoveryEncoder::new_shuffle2x`] will actually keep the Shuffle2x layout.
///
/// Unlike [`altmap_kernel_available`], this needs only AVX2 — the Shuffle2x
/// kernel runs on GFNI hardware too.
pub fn shuffle2x_kernel_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// AVX-512 BW nibble-shuffle (Normal layout). Used when AVX-512 is present
/// without GFNI (Skylake-X / Cascade Lake).
pub fn shuffle512_kernel_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512bw")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Whether [`RecoveryEncoder::new_affine2x`] keeps the Affine2x layout.
/// Requires AVX2+GFNI (the kernel uses `gf2p8affine`).
pub fn affine2x_kernel_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("gfni")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Parpar Affine (shuffle-prepare) kernel: AVX2+GFNI.
pub fn affine_kernel_available() -> bool {
    affine2x_kernel_available()
}

/// Parpar Affine AVX-512+GFNI (default_method on Ice Lake / SPR).
pub fn affine512_kernel_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("gfni")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}
