//! Parallel JPEG decoding for received frames.
//!
//! At 240 FPS a frame has ~4.2 ms; one thread that both reassembled UDP and
//! decoded JPEG fell behind on slow CPUs (processing FPS dipped to 1xx while
//! the sender held 240). Reassembly stays on the receiver thread and hands
//! each complete frame to this pool; several workers decode successive
//! frames at once. Only the newest waiting frame is kept, and a frame that
//! finishes after a newer one is discarded, so output stays in order.

use crate::receiver::{decode_udp_frame, SourceFrame};
use crate::udp::EncodedFrame;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

type Deliver = Arc<dyn Fn(SourceFrame) + Send + Sync>;

#[derive(Default)]
struct Shared {
    pending: Mutex<Option<(u64, EncodedFrame, Instant)>>,
    wake: Condvar,
    stopping: AtomicBool,
    next_seq: AtomicU64,
    newest_out: AtomicU64,
    superseded: AtomicU64,
    failed: AtomicU64,
}

/// Frames the pool dropped: replaced while waiting or finished after a
/// newer one (`superseded`), and corrupt/mismatched JPEGs (`failed`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeStats {
    pub superseded: u64,
    pub failed: u64,
}

pub struct DecodePool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

/// Decoders for this machine: one per two logical CPUs, 1 to 3, leaving
/// room for the receiver, processing, movement and the native Preview.
pub fn default_workers() -> usize {
    let cpus = thread::available_parallelism().map_or(2, usize::from);
    (cpus / 2).clamp(1, 3)
}

impl DecodePool {
    pub fn new(workers: usize, deliver: Deliver) -> Self {
        let shared = Arc::new(Shared::default());
        let workers = (0..workers.max(1))
            .map(|index| {
                let shared = Arc::clone(&shared);
                let deliver = Arc::clone(&deliver);
                thread::Builder::new()
                    .name(format!("sysx-jpeg-decode-{index}"))
                    .spawn(move || worker(&shared, &*deliver))
                    .expect("JPEG decode thread")
            })
            .collect();
        Self { shared, workers }
    }

    /// Queues a complete frame, replacing one no worker has taken yet.
    /// `received` is when the receiver had it complete.
    pub fn submit(&self, frame: EncodedFrame, received: Instant) {
        let seq = self.shared.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut pending) = self.shared.pending.lock() {
            if pending.replace((seq, frame, received)).is_some() {
                self.shared.superseded.fetch_add(1, Ordering::Relaxed);
            }
            self.shared.wake.notify_one();
        }
    }

    pub fn stats(&self) -> DecodeStats {
        DecodeStats {
            superseded: self.shared.superseded.load(Ordering::Relaxed),
            failed: self.shared.failed.load(Ordering::Relaxed),
        }
    }
}

impl Drop for DecodePool {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker(shared: &Shared, deliver: &(dyn Fn(SourceFrame) + Send + Sync)) {
    loop {
        let job = {
            let Ok(mut pending) = shared.pending.lock() else {
                return;
            };
            loop {
                if shared.stopping.load(Ordering::Acquire) {
                    return;
                }
                if let Some(job) = pending.take() {
                    break job;
                }
                pending = match shared.wake.wait(pending) {
                    Ok(next) => next,
                    Err(_) => return,
                };
            }
        };
        let (seq, frame, received) = job;
        match decode_udp_frame(frame) {
            Ok(mut frame) => {
                // Deliver only if no newer frame came out first.
                if shared.newest_out.fetch_max(seq, Ordering::AcqRel) < seq {
                    frame.received = received;
                    deliver(frame);
                } else {
                    shared.superseded.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(_) => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn frame(id: u64, jpeg: &[u8]) -> EncodedFrame {
        EncodedFrame {
            frame_id: id,
            width: 192,
            height: 192,
            timestamp_ns: id,
            jpeg: jpeg.to_vec(),
        }
    }

    #[test]
    fn frames_come_out_in_order_and_corrupt_ones_are_counted() {
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&out);
        let pool = DecodePool::new(
            3,
            Arc::new(move |frame: SourceFrame| sink.lock().unwrap().push(frame.frame_id)),
        );
        for id in 1..=200 {
            pool.submit(frame(id, jpeg), Instant::now());
            if id % 7 == 0 {
                pool.submit(frame(1000 + id, b"\xff\xd8bad\xff\xd9"), Instant::now());
            }
            thread::sleep(Duration::from_micros(300));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && out.lock().unwrap().len() as u64 + pool.stats().superseded + pool.stats().failed
                < 228
        {
            thread::sleep(Duration::from_millis(5));
        }
        let ids = out.lock().unwrap().clone();
        assert!(!ids.is_empty());
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
        let stats = pool.stats();
        assert_eq!(ids.len() as u64 + stats.superseded + stats.failed, 228);
        assert!(stats.failed <= 28);
    }

    #[test]
    fn the_decoders_fit_the_machine() {
        assert!((1..=3).contains(&default_workers()));
    }
}
