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

impl Par2Worker {
    pub fn spawn(enc: RecoveryEncoder, compute_hashes: bool) -> Self {
        // Channel depth: 256 slices — enough to hold ~2 flush batches.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Par2Work>(256);
        // Return channel for recycled buffers.
        let (free_tx, free_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(128);

        let handle = std::thread::spawn(move || {
            let (rs_tx, rs_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
            let (hash_tx, hash_rx) = std::sync::mpsc::sync_channel::<Vec<FileHashes>>(1);

            // Step 1: Spawn the MD5 hasher thread. It consumes Par2Work and
            // feeds raw Vec<u8> buffers to the RS encoder thread.
            if compute_hashes {
                std::thread::spawn(move || {
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
                });
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
            }

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
                hash_rx.recv().unwrap_or_default()
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
