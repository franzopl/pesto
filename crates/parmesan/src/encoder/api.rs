use super::*;

impl RecoveryEncoder {
    /// Create an encoder with the best possible performance layout for the
    /// current CPU, with fallible allocation.
    ///
    /// Auto-selects between Normal and Shuffle2x (AVX2) layouts based on
    /// detected SIMD features and `slice_size` alignment. Never selects
    /// Altmap: measured decisively slower than Shuffle2x on the one AVX2
    /// axis where both are available (§148, `bench/FINDINGS.md`), even
    /// after removing its worst inefficiency — the "XOR Bit Dependencies"
    /// technique it implements needs common-subexpression elimination to be
    /// competitive, which this port does not have.
    ///
    /// # Errors
    ///
    /// Returns `TryReserveError` if buffer allocation fails.
    pub fn try_new_smart(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Result<Self, TryReserveError> {
        // c7i 20260820T004824Z movie-1080p median: Affine512 packed 424 MiB/s
        // vs Normal+GFNI-512 326 (parpar 626). Enable when slice aligns.
        #[cfg(target_arch = "x86_64")]
        if affine512_kernel_available() && slice_size.is_multiple_of(128) {
            return Self::try_new_affine512(
                slice_size,
                total_input_slices,
                exponent_start,
                recovery_count,
            );
        }
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("gfni")
            && std::is_x86_feature_detected!("avx2")
            && slice_size.is_multiple_of(64)
        {
            return Self::try_new_affine(
                slice_size,
                total_input_slices,
                exponent_start,
                recovery_count,
            );
        }
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && !std::is_x86_feature_detected!("gfni")
        {
            // Parpar Shuffle AVX-512 (no GFNI). Normal layout + zmm vpshufb.
            return Self::try_new(
                slice_size,
                total_input_slices,
                exponent_start,
                recovery_count,
            );
        }
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2")
            && !std::is_x86_feature_detected!("gfni")
            && slice_size.is_multiple_of(32)
        {
            return Self::try_new_shuffle2x(
                slice_size,
                total_input_slices,
                exponent_start,
                recovery_count,
            );
        }

        // Default fallback.
        Self::try_new(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        )
    }

    /// Create an encoder with the best possible performance layout for the
    /// current CPU.
    ///
    /// Auto-selects between Normal and Shuffle2x (AVX2) layouts based on
    /// detected SIMD features and `slice_size` alignment. Never selects
    /// Altmap: measured decisively slower than Shuffle2x on the one AVX2
    /// axis where both are available (§148, `bench/FINDINGS.md`), even
    /// after removing its worst inefficiency — the "XOR Bit Dependencies"
    /// technique it implements needs common-subexpression elimination to be
    /// competitive, which this port does not have.
    ///
    /// # Panics
    ///
    /// Panics if buffer allocation fails.
    pub fn new_smart(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        Self::try_new_smart(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        )
        .unwrap_or_else(|e| {
            panic!(
                "PAR2 recovery buffer allocation failed ({recovery_count} blocks × \
                     {slice_size} bytes): {e}"
            )
        })
    }

    /// Create an encoder for `total_input_slices` input slices of `slice_size`
    /// bytes each, producing `recovery_count` recovery blocks (exponents
    /// `exponent_start..exponent_start + recovery_count`), with fallible allocation.
    ///
    /// # Errors
    ///
    /// Returns `TryReserveError` if buffer allocation fails.
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 4.
    pub fn try_new(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Result<Self, TryReserveError> {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(4),
            "slice size must be a positive multiple of 4"
        );
        let slice_words = slice_size / 2;
        Ok(Self {
            gf: Gf16::new(),
            slice_words,
            logbases: input_logbases(total_input_slices),
            exponent_start,
            buffers: RecoveryBufferSet::Normal(try_zeroed_buffers(
                0u16,
                slice_words,
                recovery_count,
            )?),
            next_index: 0,
            queued_slices: Vec::with_capacity(64),
            free_buffers: Vec::new(),
            affine_prepare: Vec::new(),
            flush_limit_bytes: 256 * 1024 * 1024,
            compute_checksums: false,
            pending_checksums: Vec::new(),
            simd_path: SimdPath::Auto,
            #[cfg(feature = "bench-internals")]
            forced_path: None,
            #[cfg(target_arch = "x86_64")]
            dep_tables: Self::build_dep_tables(),
        })
    }

    /// Create an encoder for `total_input_slices` input slices of `slice_size`
    /// bytes each, producing `recovery_count` recovery blocks (exponents
    /// `exponent_start..exponent_start + recovery_count`).
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 4, or if buffer allocation fails.
    pub fn new(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        Self::try_new(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        )
        .unwrap_or_else(|e| {
            panic!(
                "PAR2 recovery buffer allocation failed ({recovery_count} blocks × \
                     {slice_size} bytes): {e}"
            )
        })
    }

    /// Build the XOR dependency table for every GF(2^16) coefficient.
    ///
    /// Only allocated on AVX2-without-GFNI hardware, where the ALTMAP kernel
    /// (Phase 27e) will use it. Returns `None` on GFNI machines, when AVX2
    /// is unavailable, or if the 2 MiB allocation fails.
    #[cfg(target_arch = "x86_64")]
    fn build_dep_tables() -> Option<Box<[[u16; 16]; 65536]>> {
        if !std::is_x86_feature_detected!("avx2") || std::is_x86_feature_detected!("gfni") {
            return None;
        }
        // Heap-allocate 2 MB without touching the stack. `alloc_zeroed` is
        // stable since Rust 1.28 and pre-zeros the memory (index 0 stays [0u16; 16]).
        // Check for allocation failure before dereferencing the pointer.
        let mut table: Box<[[u16; 16]; 65536]> = unsafe {
            let layout = std::alloc::Layout::new::<[[u16; 16]; 65536]>();
            let ptr = std::alloc::alloc_zeroed(layout);
            if ptr.is_null() {
                return None;
            }
            Box::from_raw(ptr.cast())
        };
        for n in 1u16..=65535 {
            table[n as usize] = xor_dep_matrix(n);
        }
        Some(table)
    }

    /// Create an encoder that stores recovery buffers in ALTMAP bit-plane format,
    /// with fallible allocation.
    ///
    /// Identical to [`Self::try_new`] in every respect except that the internal recovery
    /// buffers use the ALTMAP layout (Phase 27d/27e).  The `flush_avx2_altmap`
    /// path (27e) will use these directly; `finish()` converts them back to
    /// normal layout before returning `RecoverySlice`s.
    ///
    /// # Errors
    ///
    /// Returns `TryReserveError` if buffer allocation fails.
    ///
    /// The ALTMAP layout is only requested, never guaranteed: when this CPU has
    /// no kernel that can consume it — no AVX2, a GFNI machine (where
    /// [`Self::build_dep_tables`] returns `None`), a non-x86_64 target, or a
    /// failed table allocation — the encoder falls back to the portable layout
    /// of [`Self::try_new`]. The recovery data is identical either way; only
    /// throughput differs. Without the fallback `flush` would hit its
    /// "unsupported layout" arm, drop every queued slice unprocessed, and
    /// `finish` would hand back all-zero recovery blocks: parity that no PAR2
    /// client can repair with, produced without an error or a warning.
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 32 bytes (= 16
    /// u16 words, the ALTMAP group size). Checked before the fallback above, so
    /// a caller gets the same rejection on every machine.
    pub fn try_new_altmap(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Result<Self, TryReserveError> {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(32),
            "ALTMAP encoder requires slice_size to be a positive multiple of 32 bytes, got {slice_size}"
        );

        #[cfg(not(target_arch = "x86_64"))]
        return Self::try_new(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        );

        #[cfg(target_arch = "x86_64")]
        {
            let dep_tables = match Self::build_dep_tables() {
                Some(tables) => tables,
                // No `flush_avx2_altmap` on this CPU — see the note above.
                None => {
                    return Self::try_new(
                        slice_size,
                        total_input_slices,
                        exponent_start,
                        recovery_count,
                    )
                }
            };
            let slice_words = slice_size / 2;
            let buf_bytes = altmap_buffer_size(slice_words);
            Ok(Self {
                gf: Gf16::new(),
                slice_words,
                logbases: input_logbases(total_input_slices),
                exponent_start,
                buffers: RecoveryBufferSet::Altmap(try_zeroed_buffers(
                    0u8,
                    buf_bytes,
                    recovery_count,
                )?),
                next_index: 0,
                queued_slices: Vec::with_capacity(64),
                free_buffers: Vec::new(),
                affine_prepare: Vec::new(),
                flush_limit_bytes: 256 * 1024 * 1024,
                compute_checksums: false,
                pending_checksums: Vec::new(),
                simd_path: SimdPath::Auto,
                #[cfg(feature = "bench-internals")]
                forced_path: None,
                dep_tables: Some(dep_tables),
            })
        }
    }

    /// Create an encoder that stores recovery buffers in ALTMAP bit-plane format.
    ///
    /// Identical to [`Self::new`] in every respect except that the internal recovery
    /// buffers use the ALTMAP layout (Phase 27d/27e).  The `flush_avx2_altmap`
    /// path (27e) will use these directly; `finish()` converts them back to
    /// normal layout before returning `RecoverySlice`s.
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 32 bytes, or if buffer allocation fails.
    pub fn new_altmap(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        Self::try_new_altmap(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        )
        .unwrap_or_else(|e| {
            panic!(
                "PAR2 recovery buffer allocation failed ({recovery_count} blocks × \
                     {slice_size} bytes, ALTMAP layout): {e}"
            )
        })
    }

    /// Create an encoder that stores recovery buffers in Shuffle2x layout,
    /// with fallible allocation.
    ///
    /// Identical to [`Self::try_new`] in every respect except that the internal recovery
    /// buffers use the Shuffle2x layout (Phase 28a): lo-bytes in lane 0, hi-bytes
    /// in lane 1 of each 32-byte chunk.  The `flush_avx2_shuffle2x` path will
    /// use these directly; `finish()` converts them back to normal layout before
    /// returning `RecoverySlice`s.
    ///
    /// # Errors
    ///
    /// Returns `TryReserveError` if buffer allocation fails.
    ///
    /// Like [`Self::try_new_altmap`], the layout is a request, not a guarantee:
    /// without AVX2 to run `flush_avx2_shuffle2x` (or off x86_64 entirely) the
    /// encoder falls back to the portable layout of [`Self::try_new`] rather
    /// than dropping every slice unprocessed and returning all-zero parity.
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 32 bytes. Checked
    /// before the fallback, so the rejection is the same on every machine.
    pub fn try_new_shuffle2x(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Result<Self, TryReserveError> {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(32),
            "Shuffle2x encoder requires slice_size to be a positive multiple of 32 bytes, got {slice_size}"
        );

        #[cfg(not(target_arch = "x86_64"))]
        return Self::try_new(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        );

        #[cfg(target_arch = "x86_64")]
        {
            // No `flush_avx2_shuffle2x` on this CPU — see the note above.
            if !std::is_x86_feature_detected!("avx2") {
                return Self::try_new(
                    slice_size,
                    total_input_slices,
                    exponent_start,
                    recovery_count,
                );
            }
            let slice_words = slice_size / 2;
            let buf_bytes = shuffle2x_buffer_size(slice_words);
            Ok(Self {
                gf: Gf16::new(),
                slice_words,
                logbases: input_logbases(total_input_slices),
                exponent_start,
                buffers: RecoveryBufferSet::Shuffle2x(try_zeroed_buffers(
                    0u8,
                    buf_bytes,
                    recovery_count,
                )?),
                next_index: 0,
                queued_slices: Vec::with_capacity(64),
                free_buffers: Vec::new(),
                affine_prepare: Vec::new(),
                flush_limit_bytes: 256 * 1024 * 1024,
                compute_checksums: false,
                pending_checksums: Vec::new(),
                simd_path: SimdPath::Auto,
                #[cfg(feature = "bench-internals")]
                forced_path: None,
                // Shuffle2x never uses dep_tables (those are only for ALTMAP).
                dep_tables: None,
            })
        }
    }

    /// Create an encoder that stores recovery buffers in Shuffle2x layout.
    ///
    /// Identical to [`Self::new`] in every respect except that the internal recovery
    /// buffers use the Shuffle2x layout (Phase 28a): lo-bytes in lane 0, hi-bytes
    /// in lane 1 of each 32-byte chunk.  The `flush_avx2_shuffle2x` path will
    /// use these directly; `finish()` converts them back to normal layout before
    /// returning `RecoverySlice`s.
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 32 bytes, or if buffer allocation fails.
    pub fn new_shuffle2x(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        Self::try_new_shuffle2x(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        )
        .unwrap_or_else(|e| {
            panic!(
                "PAR2 recovery buffer allocation failed ({recovery_count} blocks × \
                     {slice_size} bytes, Shuffle2x layout): {e}"
            )
        })
    }

    /// Affine2x recovery buffers for the AVX2+GFNI kernel.
    ///
    /// Falls back to [`Self::try_new`] when GFNI is unavailable so `finish`
    /// never returns unprocessed (zero) parity.
    pub fn try_new_affine2x(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Result<Self, TryReserveError> {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(32),
            "Affine2x encoder requires slice_size to be a positive multiple of 32 bytes, got {slice_size}"
        );

        #[cfg(not(target_arch = "x86_64"))]
        return Self::try_new(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        );

        #[cfg(target_arch = "x86_64")]
        {
            if !std::is_x86_feature_detected!("avx2") || !std::is_x86_feature_detected!("gfni") {
                return Self::try_new(
                    slice_size,
                    total_input_slices,
                    exponent_start,
                    recovery_count,
                );
            }
            let slice_words = slice_size / 2;
            let buf_bytes = affine2x_buffer_size(slice_words);
            Ok(Self {
                gf: Gf16::new(),
                slice_words,
                logbases: input_logbases(total_input_slices),
                exponent_start,
                buffers: RecoveryBufferSet::Affine2x(try_zeroed_buffers(
                    0u8,
                    buf_bytes,
                    recovery_count,
                )?),
                next_index: 0,
                queued_slices: Vec::with_capacity(64),
                free_buffers: Vec::new(),
                affine_prepare: Vec::new(),
                flush_limit_bytes: 256 * 1024 * 1024,
                compute_checksums: false,
                pending_checksums: Vec::new(),
                simd_path: SimdPath::Auto,
                #[cfg(feature = "bench-internals")]
                forced_path: None,
                dep_tables: None,
            })
        }
    }

    /// Affine2x recovery buffers for the AVX2+GFNI kernel.
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 32, or allocation fails.
    pub fn new_affine2x(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        Self::try_new_affine2x(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        )
        .unwrap_or_else(|e| {
            panic!(
                "PAR2 recovery buffer allocation failed ({recovery_count} blocks × \
                     {slice_size} bytes, Affine2x layout): {e}"
            )
        })
    }

    /// Parpar Affine layout (shuffle-prepare). Falls back to [`Self::try_new`]
    /// without AVX2+GFNI.
    pub fn try_new_affine(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Result<Self, TryReserveError> {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(64),
            "Affine encoder requires slice_size multiple of 64, got {slice_size}"
        );
        #[cfg(not(target_arch = "x86_64"))]
        return Self::try_new(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        );
        #[cfg(target_arch = "x86_64")]
        {
            if !std::is_x86_feature_detected!("avx2") || !std::is_x86_feature_detected!("gfni") {
                return Self::try_new(
                    slice_size,
                    total_input_slices,
                    exponent_start,
                    recovery_count,
                );
            }
            let slice_words = slice_size / 2;
            let buf_bytes = affine_buffer_size(slice_words);
            Ok(Self {
                gf: Gf16::new(),
                slice_words,
                logbases: input_logbases(total_input_slices),
                exponent_start,
                buffers: RecoveryBufferSet::Affine(try_zeroed_buffers(
                    0u8,
                    buf_bytes,
                    recovery_count,
                )?),
                next_index: 0,
                queued_slices: Vec::with_capacity(64),
                free_buffers: Vec::new(),
                affine_prepare: Vec::new(),
                flush_limit_bytes: 256 * 1024 * 1024,
                compute_checksums: false,
                pending_checksums: Vec::new(),
                simd_path: SimdPath::Auto,
                #[cfg(feature = "bench-internals")]
                forced_path: None,
                dep_tables: None,
            })
        }
    }

    /// Parpar Affine layout. See [`Self::try_new_affine`].
    pub fn new_affine(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        Self::try_new_affine(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        )
        .unwrap_or_else(|e| {
            panic!(
                "PAR2 recovery buffer allocation failed ({recovery_count} blocks × \
                     {slice_size} bytes, Affine layout): {e}"
            )
        })
    }

    /// Affine AVX-512+GFNI layout (128-byte prepare groups).
    pub fn try_new_affine512(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Result<Self, TryReserveError> {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(128),
            "Affine512 encoder requires slice_size multiple of 128, got {slice_size}"
        );
        #[cfg(not(target_arch = "x86_64"))]
        return Self::try_new(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        );
        #[cfg(target_arch = "x86_64")]
        {
            if !affine512_kernel_available() {
                return Self::try_new_affine(
                    slice_size,
                    total_input_slices,
                    exponent_start,
                    recovery_count,
                );
            }
            let slice_words = slice_size / 2;
            let n_tiles = slice_size.div_ceil(AFFINE512_CHUNK);
            let mut data = Vec::new();
            data.try_reserve_exact(n_tiles * AFFINE512_CHUNK * recovery_count)?;
            data.resize(n_tiles * AFFINE512_CHUNK * recovery_count, 0u8);
            Ok(Self {
                gf: Gf16::new(),
                slice_words,
                logbases: input_logbases(total_input_slices),
                exponent_start,
                buffers: RecoveryBufferSet::Affine512(Affine512Acc {
                    data,
                    n_rec: recovery_count,
                    slice_len: slice_size,
                }),
                next_index: 0,
                queued_slices: Vec::with_capacity(64),
                free_buffers: Vec::new(),
                affine_prepare: Vec::new(),
                flush_limit_bytes: 256 * 1024 * 1024,
                compute_checksums: false,
                pending_checksums: Vec::new(),
                simd_path: SimdPath::Auto,
                #[cfg(feature = "bench-internals")]
                forced_path: None,
                dep_tables: None,
            })
        }
    }

    /// Affine AVX-512+GFNI. See [`Self::try_new_affine512`].
    pub fn new_affine512(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        Self::try_new_affine512(
            slice_size,
            total_input_slices,
            exponent_start,
            recovery_count,
        )
        .unwrap_or_else(|e| {
            panic!(
                "PAR2 recovery buffer allocation failed ({recovery_count} blocks × \
                     {slice_size} bytes, Affine512 layout): {e}"
            )
        })
    }

    /// Set the maximum bytes to queue before flushing.
    pub fn with_flush_limit(mut self, bytes: usize) -> Self {
        self.flush_limit_bytes = bytes;
        self
    }

    /// Force a specific SIMD flush path, bypassing runtime auto-detection.
    /// Only available with the `bench-internals` Cargo feature.
    #[cfg(feature = "bench-internals")]
    pub fn with_forced_path(mut self, path: BenchPath) -> Self {
        self.forced_path = Some(path);
        self
    }

    /// Enable parallel per-slice MD5+CRC32 checksum computation.
    /// Each flush will compute checksums alongside RS recovery using `rayon::join`.
    /// Call [`Self::drain_checksums`] after [`Self::finish`] to retrieve them in slice order.
    pub fn with_checksums(mut self) -> Self {
        self.compute_checksums = true;
        self
    }

    /// Set a manual override for the SIMD multiplication backend.
    pub fn with_simd_path(mut self, path: SimdPath) -> Self {
        self.simd_path = path;
        self
    }

    /// Return and clear all checksums accumulated so far (in input-slice order).
    pub fn drain_checksums(&mut self) -> Vec<SliceChecksum> {
        std::mem::take(&mut self.pending_checksums)
    }

    /// Hand the producer an empty, slice-sized `Vec<u8>` with fallible allocation —
    /// either a buffer recycled from a previous flush or a fresh allocation.
    /// Returning the buffer to the encoder via [`Self::add_slice`] keeps the pool refilled.
    ///
    /// # Errors
    ///
    /// Returns `TryReserveError` if buffer allocation or expansion fails.
    pub fn try_take_buffer(&mut self) -> Result<Vec<u8>, TryReserveError> {
        let slice_size = self.slice_words * 2;
        if let Some(mut buf) = self.free_buffers.pop() {
            buf.clear();
            if buf.capacity() < slice_size {
                buf.try_reserve_exact(slice_size - buf.capacity())?;
            }
            Ok(buf)
        } else {
            let mut buf = Vec::new();
            buf.try_reserve_exact(slice_size)?;
            Ok(buf)
        }
    }

    /// Hand the producer an empty, slice-sized `Vec<u8>` — either a buffer
    /// recycled from a previous flush or a fresh allocation. Returning the
    /// buffer to the encoder via [`Self::add_slice`] keeps the pool refilled.
    ///
    /// # Panics
    ///
    /// Panics if buffer allocation fails.
    pub fn take_buffer(&mut self) -> Vec<u8> {
        self.try_take_buffer().unwrap_or_else(|e| {
            panic!(
                "PAR2 slice buffer allocation failed ({} bytes): {e}",
                self.slice_words * 2
            )
        })
    }
}
