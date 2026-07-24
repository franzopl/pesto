use crate::encoder::{FileHashes, RecoveryEncoder, RecoverySlice};
use crate::packet::SliceChecksum;

/// A unit of work for the [`Par2Worker`].
pub enum Par2Work {
    /// A completed input slice to be added to the recovery set.
    Slice {
        /// The slice data (possibly zero-padded to `par2_slice_size`).
        data: Vec<u8>,
        /// The number of real (unpadded) bytes from the source file in this slice.
        actual_len: usize,
        /// True if this is the last slice of a file, triggering hash finalisation.
        is_last_of_file: bool,
    },
}

/// Wraps a [`RecoveryEncoder`] running on a dedicated OS thread so that RS
/// flush work overlaps with the async reader and NNTP posting pipeline.
///
/// The producer sends completed input slices through a bounded channel; the
/// worker calls [`RecoveryEncoder::add_slice`] (which auto-triggers flushes)
/// and returns recycled slice buffers via a second channel so the producer
/// avoids re-allocating slice-sized buffers on every iteration.
pub struct Par2Worker {
    /// Bounded send end — blocks naturally when the worker is mid-flush.
    tx: std::sync::mpsc::SyncSender<Par2Work>,
    /// Receives buffers the worker recycled after internal flushes.
    /// Wrapped in `Mutex` so `&Par2Worker` is `Sync` (required by async tasks).
    free_rx: std::sync::Mutex<std::sync::mpsc::Receiver<Vec<u8>>>,
    /// The worker thread; held until [`Par2Worker::finish`] is called.
    handle: std::thread::JoinHandle<(Vec<RecoverySlice>, Vec<SliceChecksum>, Vec<FileHashes>)>,
}

/// Default depth (in slices) for the producer→hasher→encoder pipeline
/// channels. Each in-flight slot holds a full `par2_slice_size` buffer, so
/// this is a memory/throughput trade-off, not just a queue length: at a
/// large slice size (tens of MB, common on big files) a deep channel can
/// hold several GB across the three pipeline stages, uncapped by and
/// invisible to whatever memory budget the caller sized the encoder's own
/// buffers against. Small on purpose — enough to let the async reader race
/// a little ahead of the RS/hash threads without becoming its own unbounded
/// memory sink. Pass a caller-computed depth via [`Par2Worker::spawn`] when
/// the slice size is large enough that this default would itself be
/// significant.
pub const DEFAULT_CHANNEL_DEPTH: usize = 64;

impl Par2Worker {
    /// `channel_depth` bounds how many in-flight slices (each a full
    /// `par2_slice_size` buffer) the producer→hasher→encoder pipeline may
    /// buffer at once — see [`DEFAULT_CHANNEL_DEPTH`]. Callers that already
    /// size the encoder's buffers against a memory budget should keep this
    /// small (it's pipelining slack, not part of that budget).
    pub fn spawn(enc: RecoveryEncoder, compute_hashes: bool, channel_depth: usize) -> Self {
        let channel_depth = channel_depth.max(2); // at least double-buffered
        let (tx, rx) = std::sync::mpsc::sync_channel::<Par2Work>(channel_depth);
        // Return channel for recycled buffers.
        let (free_tx, free_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(channel_depth);

        let handle = std::thread::spawn(move || {
            let (rs_tx, rs_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(channel_depth);
            let (hash_tx, hash_rx) = std::sync::mpsc::sync_channel::<Vec<FileHashes>>(1);

            // Step 1: Spawn the MD5 hasher thread. It consumes Par2Work and
            // feeds raw Vec<u8> buffers to the RS encoder thread.
            //
            // Its `JoinHandle` is kept (not fire-and-forget) so a panic in
            // here propagates instead of silently dropping `hash_tx`: without
            // this, `hash_rx.recv()` below would just see a disconnected
            // channel and `unwrap_or_default()` would return an empty
            // `Vec<FileHashes>` — surfacing, at the call site, as "worker
            // returned fewer hashes than non-empty files" with no hint that a
            // panic (the real cause) ever happened.
            let hasher_handle = if compute_hashes {
                Some(std::thread::spawn(move || {
                    let mut hashes = Vec::new();
                    let mut current_hasher = crate::encoder::FileHasher::new();
                    while let Ok(work) = rx.recv() {
                        match work {
                            Par2Work::Slice {
                                data,
                                actual_len,
                                is_last_of_file,
                            } => {
                                current_hasher.update(&data[..actual_len]);
                                if is_last_of_file {
                                    hashes.push(current_hasher.finish());
                                    current_hasher = crate::encoder::FileHasher::new();
                                }
                                let _ = rs_tx.send(data);
                            }
                        }
                    }
                    let _ = hash_tx.send(hashes);
                }))
            } else {
                std::thread::spawn(move || {
                    while let Ok(work) = rx.recv() {
                        match work {
                            Par2Work::Slice { data, .. } => {
                                let _ = rs_tx.send(data);
                            }
                        }
                    }
                });
                None
            };

            // Step 2: This thread acts as the RS encoder. It consumes slices
            // from the hasher thread and performs Reed-Solomon flushes.
            let mut enc = enc;
            while let Ok(slice) = rs_rx.recv() {
                enc.add_slice(slice);
                // After add_slice, if a flush was triggered, free_buffers holds
                // the recycled slices. Ferry them back to the producer.
                for buf in enc.drain_free_buffers() {
                    let _ = free_tx.try_send(buf); // drop if return channel is full
                }
            }
            let (slices, checksums) = enc.finish();
            let hashes = if compute_hashes {
                match hash_rx.recv() {
                    Ok(hashes) => hashes,
                    Err(_) => {
                        // `hash_tx` was dropped without sending — the hasher
                        // thread panicked before reaching its final send.
                        // Join it to propagate the real panic instead of
                        // silently returning an empty `Vec<FileHashes>`.
                        if let Some(h) = hasher_handle {
                            match h.join() {
                                Err(payload) => std::panic::resume_unwind(payload),
                                Ok(()) => unreachable!(
                                    "hasher thread returned normally but never sent hashes"
                                ),
                            }
                        }
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            (slices, checksums, hashes)
        });

        Self {
            tx,
            free_rx: std::sync::Mutex::new(free_rx),
            handle,
        }
    }

    /// Return a reused buffer from the pool, or allocate a fresh one.
    pub fn take_buffer(&self, slice_size: usize) -> Vec<u8> {
        match self.free_rx.lock().unwrap().try_recv() {
            Ok(mut buf) => {
                buf.clear();
                buf
            }
            Err(_) => Vec::with_capacity(slice_size),
        }
    }

    /// Send a completed, zero-padded slice to the worker.
    ///
    /// Blocks when the channel is full (worker mid-flush). Callers from async
    /// context must wrap with `block_in_place` so tokio can park the thread.
    pub fn send_slice(&self, slice: Vec<u8>, actual_len: usize, is_last_of_file: bool) {
        // Only errors if the worker panicked; propagate as a panic here too.
        self.tx
            .send(Par2Work::Slice {
                data: slice,
                actual_len,
                is_last_of_file,
            })
            .expect("par2 worker thread died");
    }

    /// Signal end-of-input, wait for the worker to finish, and return results.
    pub fn finish(self) -> (Vec<RecoverySlice>, Vec<SliceChecksum>, Vec<FileHashes>) {
        drop(self.tx); // closing the channel causes rx.recv() to return Err
        self.handle.join().expect("par2 worker thread panicked")
    }
}
