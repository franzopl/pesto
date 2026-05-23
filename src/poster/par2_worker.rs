use pesto_par2::encoder::{RecoveryEncoder, RecoverySlice};
use pesto_par2::packet::SliceChecksum;

/// A unit of work for the [`Par2Worker`].
pub enum Par2Work {
    /// A completed input slice to be added to the recovery set.
    Slice(Vec<u8>),
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
    handle: std::thread::JoinHandle<(Vec<RecoverySlice>, Vec<SliceChecksum>)>,
}

impl Par2Worker {
    pub fn spawn(enc: RecoveryEncoder) -> Self {
        // Channel depth: 256 slices — enough to hold ~2 flush batches.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Par2Work>(256);
        // Return channel for recycled buffers.
        let (free_tx, free_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(128);

        let handle = std::thread::spawn(move || {
            let mut enc = enc;
            while let Ok(work) = rx.recv() {
                match work {
                    Par2Work::Slice(slice) => {
                        enc.add_slice(slice);
                        // After add_slice, if a flush was triggered, free_buffers holds
                        // the recycled slices. Ferry them back to the producer.
                        for buf in enc.drain_free_buffers() {
                            let _ = free_tx.try_send(buf); // drop if return channel is full
                        }
                    }
                }
            }
            enc.finish()
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
    pub fn send_slice(&self, slice: Vec<u8>) {
        // Only errors if the worker panicked; propagate as a panic here too.
        self.tx
            .send(Par2Work::Slice(slice))
            .expect("par2 worker thread died");
    }

    /// Signal end-of-input, wait for the worker to finish, and return results.
    pub fn finish(self) -> (Vec<RecoverySlice>, Vec<SliceChecksum>) {
        drop(self.tx); // closing the channel causes rx.recv() to return Err
        self.handle.join().expect("par2 worker thread panicked")
    }
}
