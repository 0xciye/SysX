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
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type Deliver = Arc<dyn Fn(SourceFrame) + Send + Sync>;

/// Limit active decoding to 8 MP (24 MiB of RGB output); larger frames decode
/// exclusively. Small production crops still use all available workers.
const DECODE_PIXEL_BUDGET: u64 = 8 * 1024 * 1024;
/// An in-flight decode may finish after a newer packet arrives, but never
/// publish a frame old enough to drive visibly stale preview or movement.
const MAX_DECODE_AGE: Duration = Duration::from_millis(100);

#[derive(Default)]
struct DecodeBudgetState {
    active_pixels: u64,
    waiting_exclusive: usize,
}

fn budget_available(state: &DecodeBudgetState, pixels: u64, exclusive: bool) -> bool {
    if exclusive {
        state.active_pixels == 0
    } else {
        state.waiting_exclusive == 0
            && state.active_pixels.saturating_add(pixels) <= DECODE_PIXEL_BUDGET
    }
}

fn is_obsolete(seq: u64, newest: u64) -> bool {
    seq < newest
}

fn can_deliver(seq: u64, newest_delivered: u64) -> bool {
    newest_delivered < seq
}

fn decode_is_fresh(received: Instant, now: Instant) -> bool {
    now.saturating_duration_since(received) <= MAX_DECODE_AGE
}

fn budget_wait_cancelled(seq: u64, newest_submitted: u64, stopping: bool) -> bool {
    stopping || is_obsolete(seq, newest_submitted)
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn recover_worker<T>(run: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).map_err(|panic| {
        panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic")
            .to_owned()
    })
}

#[derive(Default)]
struct Shared {
    pending: Mutex<Option<(u64, EncodedFrame, Instant)>>,
    wake: Condvar,
    stopping: AtomicBool,
    next_seq: AtomicU64,
    /// Sequence number of the newest frame delivered. Held while checking and
    /// delivering, so two workers cannot pass the check in order and then
    /// deliver out of order.
    newest_out: Mutex<u64>,
    decode_budget: Mutex<DecodeBudgetState>,
    decode_budget_wake: Condvar,
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
    pub fn new(workers: usize, deliver: Deliver) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let mut handles = Vec::with_capacity(workers.max(1));
        for index in 0..workers.max(1) {
            let worker_shared = Arc::clone(&shared);
            let worker_deliver = Arc::clone(&deliver);
            match thread::Builder::new()
                .name(format!("sysx-jpeg-decode-{index}"))
                .spawn(move || worker_supervisor(&worker_shared, &*worker_deliver))
            {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    {
                        let _pending = lock_recover(&shared.pending);
                        shared.stopping.store(true, Ordering::Release);
                        shared.wake.notify_all();
                    }
                    for handle in handles {
                        let _ = handle.join();
                    }
                    return Err(error);
                }
            }
        }
        Ok(Self {
            shared,
            workers: handles,
        })
    }

    /// Queues a complete frame, replacing one no worker has taken yet.
    /// `received` is when the receiver had it complete.
    pub fn submit(&self, frame: EncodedFrame, received: Instant) {
        let seq = self.shared.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut pending = lock_recover(&self.shared.pending);
        if pending.replace((seq, frame, received)).is_some() {
            self.shared.superseded.fetch_add(1, Ordering::Relaxed);
        }
        drop(pending);
        self.shared.wake.notify_one();
        // Pair with the wait mutex so a newer frame cannot lose this wake-up.
        let _budget = lock_recover(&self.shared.decode_budget);
        self.shared.decode_budget_wake.notify_all();
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
        {
            // Pair the stop predicate with the wait mutex so a worker cannot
            // miss the notification between checking `stopping` and sleeping.
            let _pending = lock_recover(&self.shared.pending);
            self.shared.stopping.store(true, Ordering::Release);
            self.shared.wake.notify_all();
        }
        {
            // Pair with the wait mutex so shutdown cannot be lost before sleep.
            let _budget = lock_recover(&self.shared.decode_budget);
            self.shared.decode_budget_wake.notify_all();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

struct DecodeBudgetPermit<'a> {
    shared: &'a Shared,
    pixels: u64,
}

impl Drop for DecodeBudgetPermit<'_> {
    fn drop(&mut self) {
        let mut budget = self
            .shared
            .decode_budget
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        budget.active_pixels = budget.active_pixels.saturating_sub(self.pixels);
        drop(budget);
        self.shared.decode_budget_wake.notify_all();
    }
}

fn acquire_decode_budget(shared: &Shared, pixels: u64, seq: u64) -> Option<DecodeBudgetPermit<'_>> {
    let exclusive = pixels > DECODE_PIXEL_BUDGET;
    let mut budget = shared
        .decode_budget
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if exclusive {
        budget.waiting_exclusive += 1;
    }
    while !budget_wait_cancelled(
        seq,
        shared.next_seq.load(Ordering::Acquire),
        shared.stopping.load(Ordering::Acquire),
    ) && !budget_available(&budget, pixels, exclusive)
    {
        budget = shared
            .decode_budget_wake
            .wait(budget)
            .unwrap_or_else(|error| error.into_inner());
    }
    if exclusive {
        budget.waiting_exclusive = budget.waiting_exclusive.saturating_sub(1);
    }
    if budget_wait_cancelled(
        seq,
        shared.next_seq.load(Ordering::Acquire),
        shared.stopping.load(Ordering::Acquire),
    ) {
        drop(budget);
        shared.decode_budget_wake.notify_all();
        return None;
    }
    budget.active_pixels = budget.active_pixels.saturating_add(pixels);
    Some(DecodeBudgetPermit { shared, pixels })
}

fn worker(shared: &Shared, deliver: &(dyn Fn(SourceFrame) + Send + Sync)) {
    loop {
        let job = {
            let mut pending = lock_recover(&shared.pending);
            loop {
                if shared.stopping.load(Ordering::Acquire) {
                    return;
                }
                if let Some(job) = pending.take() {
                    break job;
                }
                pending = match shared.wake.wait(pending) {
                    Ok(next) => next,
                    Err(error) => error.into_inner(),
                };
            }
        };
        let (seq, frame, received) = job;
        if is_obsolete(seq, shared.next_seq.load(Ordering::Acquire)) {
            shared.superseded.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let pixels = u64::from(frame.width) * u64::from(frame.height);
        let Some(_budget_permit) = acquire_decode_budget(shared, pixels, seq) else {
            if shared.stopping.load(Ordering::Acquire) {
                return;
            }
            shared.superseded.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        // A newer frame may arrive while this worker waits for the pixel budget.
        if is_obsolete(seq, shared.next_seq.load(Ordering::Acquire)) {
            shared.superseded.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let decoded =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_udp_frame(frame)));
        match decoded {
            Ok(Ok(mut frame)) => {
                // Keep already-running decoders useful under load: only the
                // waiting queue is latest-only; reject results overtaken by a
                // newer delivery or too old to be useful.
                let mut newest = shared
                    .newest_out
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if !can_deliver(seq, *newest) || !decode_is_fresh(received, Instant::now()) {
                    shared.superseded.fetch_add(1, Ordering::Relaxed);
                } else {
                    *newest = seq;
                    frame.received = received;
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| deliver(frame)))
                        .is_err()
                    {
                        shared.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Ok(Err(_)) | Err(_) => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn worker_supervisor(shared: &Shared, deliver: &(dyn Fn(SourceFrame) + Send + Sync)) {
    loop {
        if let Err(message) = recover_worker(|| worker(shared, deliver)) {
            if shared.stopping.load(Ordering::Acquire) {
                return;
            }
            shared.failed.fetch_add(1, Ordering::Relaxed);
            crate::diagnostics::record(&format!(
                "JPEG decode worker recovered from panic: {message}"
            ));
            thread::sleep(Duration::from_millis(50));
        } else {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn large_decodes_are_exclusive_and_prevent_new_small_work_from_starving_them() {
        let mut budget = DecodeBudgetState {
            active_pixels: DECODE_PIXEL_BUDGET / 2,
            waiting_exclusive: 1,
        };
        assert!(!budget_available(&budget, 1, false));
        assert!(!budget_available(&budget, DECODE_PIXEL_BUDGET + 1, true));

        budget.active_pixels = 0;
        assert!(budget_available(&budget, DECODE_PIXEL_BUDGET + 1, true));
        budget.active_pixels = DECODE_PIXEL_BUDGET + 1;
        budget.waiting_exclusive = 0;
        assert!(!budget_available(&budget, 1, false));
    }

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
        )
        .unwrap();
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

    #[test]
    fn queued_frame_is_obsolete_when_a_newer_frame_has_been_submitted() {
        assert!(is_obsolete(1, 2));
        assert!(!is_obsolete(2, 2));
    }

    #[test]
    fn delivery_keeps_fresh_in_flight_frames_and_rejects_overtaken_results() {
        assert!(can_deliver(7, 6), "newer frame submitted during decode");
        assert!(!can_deliver(7, 8), "newer frame already delivered");
        assert!(can_deliver(8, 7), "latest frame remains deliverable");
    }

    #[test]
    fn decoder_worker_panic_is_converted_to_a_recoverable_error() {
        let error = recover_worker(|| -> () { panic!("simulated worker panic") }).unwrap_err();
        assert_eq!(error, "simulated worker panic");
    }

    #[test]
    fn decode_age_gate_rejects_stale_frames_without_rejecting_fresh_work() {
        let received = Instant::now();
        assert!(decode_is_fresh(
            received,
            received + Duration::from_millis(99)
        ));
        assert!(!decode_is_fresh(
            received,
            received + Duration::from_millis(101)
        ));
    }

    #[test]
    fn obsolete_or_stopping_frames_cancel_decode_budget_waits() {
        assert!(budget_wait_cancelled(7, 8, false));
        assert!(budget_wait_cancelled(8, 8, true));
        assert!(!budget_wait_cancelled(8, 8, false));
    }

    #[test]
    fn pending_queue_lock_recovers_after_poisoning() {
        let pending = Mutex::new(None::<u64>);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = pending.lock().unwrap();
            panic!("simulate worker panic while holding pending queue");
        }));
        assert!(poisoned.is_err());
        *lock_recover(&pending) = Some(7);
        assert_eq!(*lock_recover(&pending), Some(7));
    }

    #[test]
    fn a_panicking_frame_callback_does_not_kill_the_decode_worker() {
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        let panic_once = Arc::new(AtomicBool::new(true));
        let panic_flag = Arc::clone(&panic_once);
        let (delivered, received) = std::sync::mpsc::channel();
        let pool = DecodePool::new(
            1,
            Arc::new(move |frame: SourceFrame| {
                if panic_flag.swap(false, Ordering::AcqRel) {
                    panic!("simulated frame callback panic");
                }
                let _ = delivered.send(frame.frame_id);
            }),
        )
        .unwrap();

        pool.submit(frame(1, jpeg), Instant::now());
        let deadline = Instant::now() + Duration::from_secs(2);
        while pool.stats().failed == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(pool.stats().failed, 1);

        pool.submit(frame(2, jpeg), Instant::now());
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
    }
}
