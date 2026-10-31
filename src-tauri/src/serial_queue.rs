use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

pub const MOVE_REQUEST_MS: u32 = 100;
const MAX_QUEUED_COMMANDS: usize = 256;

#[derive(Debug, PartialEq, Eq)]
pub struct QueuedSerialCommand {
    pub kind: u32,
    pub bytes: Vec<u8>,
    pub request_ms: u32,
}

pub trait AsyncSerialWriter {
    type Ticket;

    // The consumer keeps `bytes` alive until `poll_completion` reports a result.
    // `request_ms` is a writer hint, not an end-to-end deadline. The Medius
    // adapter uses SDK-managed I/O timeouts; this queue cannot cancel SDK calls.
    fn submit(&mut self, bytes: &[u8], request_ms: u32) -> Result<Self::Ticket, String>;
    fn poll_completion(&mut self, ticket: &mut Self::Ticket) -> Option<Result<(), String>>;
}

struct PendingCommand {
    command: QueuedSerialCommand,
    completion: Sender<Result<(), String>>,
}

struct InFlight<Ticket> {
    // Owns the command bytes until the writer reports completion.
    #[cfg_attr(not(test), allow(dead_code))]
    command: QueuedSerialCommand,
    ticket: Ticket,
    completion: Sender<Result<(), String>>,
}

pub struct SerialCommandQueue<W: AsyncSerialWriter> {
    writer: W,
    queue: VecDeque<PendingCommand>,
    in_flight: Option<InFlight<W::Ticket>>,
}

impl<W: AsyncSerialWriter> SerialCommandQueue<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            queue: VecDeque::new(),
            in_flight: None,
        }
    }

    pub fn submit(
        &mut self,
        command: QueuedSerialCommand,
    ) -> Result<Receiver<Result<(), String>>, String> {
        if command.kind != 0 {
            return Err(format!(
                "serial request kind {} is not implemented",
                command.kind
            ));
        }
        if command.bytes.is_empty() {
            return Err("serial command must not be empty".into());
        }
        if self.queue.len() + usize::from(self.in_flight.is_some()) >= MAX_QUEUED_COMMANDS {
            return Err("serial command queue is full".into());
        }
        let (completion, receiver) = mpsc::channel();
        self.queue.push_back(PendingCommand {
            command,
            completion,
        });
        Ok(receiver)
    }

    // Advance at most one request. `request_ms` is handed to the writer as its
    // writer hint.
    pub fn pump(&mut self) {
        if let Some(in_flight) = self.in_flight.as_mut() {
            let Some(result) = self.writer.poll_completion(&mut in_flight.ticket) else {
                return;
            };
            let Some(in_flight) = self.in_flight.take() else {
                return;
            };
            let _ = in_flight.completion.send(result);
        }

        let Some(pending) = self.queue.pop_front() else {
            return;
        };
        match self
            .writer
            .submit(&pending.command.bytes, pending.command.request_ms)
        {
            Ok(ticket) => {
                self.in_flight = Some(InFlight {
                    command: pending.command,
                    ticket,
                    completion: pending.completion,
                });
            }
            Err(error) => {
                let _ = pending.completion.send(Err(error));
            }
        }
    }

    pub fn submit_medius_move(
        &mut self,
        dx: i16,
        dy: i16,
    ) -> Result<Receiver<Result<(), String>>, String> {
        self.submit(QueuedSerialCommand {
            kind: 0,
            bytes: encode_medius_move(dx, dy),
            request_ms: MOVE_REQUEST_MS,
        })
    }

    pub fn is_idle(&self) -> bool {
        self.queue.is_empty() && self.in_flight.is_none()
    }

    #[cfg(test)]
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    #[cfg(test)]
    pub fn in_flight_bytes(&self) -> Option<&[u8]> {
        self.in_flight
            .as_ref()
            .map(|request| request.command.bytes.as_slice())
    }

    #[cfg(test)]
    pub fn in_flight_request_ms(&self) -> Option<u32> {
        self.in_flight
            .as_ref()
            .map(|request| request.command.request_ms)
    }
}

struct WriteJob {
    bytes: Vec<u8>,
    budget: Duration,
    completion: Sender<Result<(), String>>,
}

/// Runs queued commands on a dedicated thread, so the caller's read/telemetry
/// loop never blocks on the device driver. `sink` receives each command's
/// retained bytes and its `request_ms` writer hint (Medius uses SDK timeouts).
pub struct ThreadedSerialWriter {
    jobs: Sender<WriteJob>,
}

impl ThreadedSerialWriter {
    pub fn spawn<S>(mut sink: S) -> Result<Self, String>
    where
        S: FnMut(&[u8], Duration) -> Result<(), String> + Send + 'static,
    {
        let (jobs, receiver) = mpsc::channel::<WriteJob>();
        thread::Builder::new()
            .name("sysx-serial-writer".into())
            .spawn(move || {
                for job in receiver {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        sink(&job.bytes, job.budget)
                    }))
                    .unwrap_or_else(|_| Err("Medius writer panicked".into()));
                    let _ = job.completion.send(result);
                }
            })
            .map_err(|error| format!("serial writer thread failed to start: {error}"))?;
        Ok(Self { jobs })
    }
}

const MEDIUS_MOVE_TAG: u8 = b'M';

/// Internal work record, never a device wire packet. The Medius sink decodes
/// `[tag, dx_le, dy_le]` and invokes the Rust SDK's public `move_rel` method.
pub fn encode_medius_move(dx: i16, dy: i16) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(5);
    bytes.push(MEDIUS_MOVE_TAG);
    bytes.extend_from_slice(&dx.to_le_bytes());
    bytes.extend_from_slice(&dy.to_le_bytes());
    bytes
}

pub fn decode_medius_move(bytes: &[u8]) -> Option<(i16, i16)> {
    match bytes {
        [MEDIUS_MOVE_TAG, x0, x1, y0, y1] => Some((
            i16::from_le_bytes([*x0, *x1]),
            i16::from_le_bytes([*y0, *y1]),
        )),
        _ => None,
    }
}

pub fn encode_medius_left_click() -> Vec<u8> {
    vec![b'C']
}

pub fn is_medius_left_click(bytes: &[u8]) -> bool {
    bytes == [b'C']
}

/// Left button press and release kept separate (Magnet holds the button).
pub fn encode_medius_left_press(pressed: bool) -> Vec<u8> {
    vec![if pressed { b'D' } else { b'U' }]
}

pub fn decode_medius_left_press(bytes: &[u8]) -> Option<bool> {
    match bytes {
        [b'D'] => Some(true),
        [b'U'] => Some(false),
        _ => None,
    }
}

impl<W: AsyncSerialWriter> SerialCommandQueue<W> {
    pub fn submit_medius_left_press(
        &mut self,
        pressed: bool,
    ) -> Result<Receiver<Result<(), String>>, String> {
        self.submit(QueuedSerialCommand {
            kind: 0,
            bytes: encode_medius_left_press(pressed),
            request_ms: MOVE_REQUEST_MS,
        })
    }

    pub fn submit_medius_left_click(&mut self) -> Result<Receiver<Result<(), String>>, String> {
        self.submit(QueuedSerialCommand {
            kind: 0,
            bytes: encode_medius_left_click(),
            request_ms: MOVE_REQUEST_MS,
        })
    }
}

impl AsyncSerialWriter for ThreadedSerialWriter {
    type Ticket = Receiver<Result<(), String>>;

    fn submit(&mut self, bytes: &[u8], request_ms: u32) -> Result<Self::Ticket, String> {
        let (completion, ticket) = mpsc::channel();
        self.jobs
            .send(WriteJob {
                bytes: bytes.to_vec(),
                budget: Duration::from_millis(u64::from(request_ms)),
                completion,
            })
            .map_err(|_| "serial writer thread stopped".to_string())?;
        Ok(ticket)
    }

    fn poll_completion(&mut self, ticket: &mut Self::Ticket) -> Option<Result<(), String>> {
        match ticket.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("serial writer thread stopped".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_medius_move, encode_medius_move, AsyncSerialWriter, QueuedSerialCommand,
        SerialCommandQueue, ThreadedSerialWriter, MOVE_REQUEST_MS,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct FakeWriter {
        submitted: Vec<Vec<u8>>,
        completions: VecDeque<Option<Result<(), String>>>,
    }

    impl AsyncSerialWriter for FakeWriter {
        type Ticket = usize;

        fn submit(&mut self, bytes: &[u8], _: u32) -> Result<Self::Ticket, String> {
            self.submitted.push(bytes.to_vec());
            self.completions.push_back(None);
            Ok(self.submitted.len() - 1)
        }

        fn poll_completion(&mut self, ticket: &mut Self::Ticket) -> Option<Result<(), String>> {
            self.completions.get_mut(*ticket)?.take()
        }
    }

    #[test]
    fn retains_command_bytes_and_advances_only_after_writer_completion() {
        let mut queue = SerialCommandQueue::new(FakeWriter::default());
        let first = queue.submit_medius_move(12, -7).unwrap();
        let second = queue.submit_medius_move(-3, 5).unwrap();
        queue.pump();

        assert_eq!(
            queue.in_flight_bytes(),
            Some(encode_medius_move(12, -7).as_slice())
        );
        assert_eq!(queue.in_flight_request_ms(), Some(MOVE_REQUEST_MS));
        assert_eq!(queue.queued_len(), 1);
        assert!(first.try_recv().is_err());
        assert_eq!(queue.writer.submitted.len(), 1);

        queue.writer.completions[0] = Some(Ok(()));
        queue.pump();
        assert_eq!(first.try_recv().unwrap(), Ok(()));
        assert_eq!(queue.writer.submitted.len(), 2);
        assert_eq!(
            queue.in_flight_bytes(),
            Some(encode_medius_move(-3, 5).as_slice())
        );
        assert!(second.try_recv().is_err());
    }

    #[test]
    fn reports_submission_errors_and_rejects_unimplemented_kinds() {
        struct FailingWriter;
        impl AsyncSerialWriter for FailingWriter {
            type Ticket = ();
            fn submit(&mut self, _: &[u8], _: u32) -> Result<Self::Ticket, String> {
                Err("writer rejected request".into())
            }
            fn poll_completion(&mut self, _: &mut Self::Ticket) -> Option<Result<(), String>> {
                None
            }
        }

        let mut queue = SerialCommandQueue::new(FailingWriter);
        assert!(queue
            .submit(QueuedSerialCommand {
                kind: 1,
                bytes: b"timer".to_vec(),
                request_ms: 100,
            })
            .is_err());
        let result = queue.submit_medius_move(1, 2).unwrap();
        queue.pump();
        assert_eq!(
            result.try_recv().unwrap(),
            Err("writer rejected request".into())
        );
    }

    #[test]
    fn threaded_writer_delivers_moves_in_order_and_reports_completion() {
        let moves = Arc::new(Mutex::new(Vec::new()));
        let seen = moves.clone();
        let writer = ThreadedSerialWriter::spawn(move |bytes, _| {
            seen.lock()
                .unwrap()
                .push(decode_medius_move(bytes).unwrap());
            Ok(())
        })
        .unwrap();
        let mut queue = SerialCommandQueue::new(writer);
        let first = queue.submit_medius_move(5, -6).unwrap();
        let second = queue.submit_medius_move(-32, 32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !queue.is_idle() && Instant::now() < deadline {
            queue.pump();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(queue.is_idle());
        assert_eq!(first.try_recv().unwrap(), Ok(()));
        assert_eq!(second.try_recv().unwrap(), Ok(()));
        assert_eq!(moves.lock().unwrap().as_slice(), &[(5, -6), (-32, 32)]);
    }

    #[test]
    fn threaded_writer_recovers_after_a_sink_panic() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let writer = ThreadedSerialWriter::spawn(move |_, _| {
            if worker_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                panic!("simulated Medius writer panic");
            }
            Ok(())
        })
        .unwrap();
        let mut queue = SerialCommandQueue::new(writer);
        let failed = queue.submit_medius_move(1, 2).unwrap();
        let recovered = queue.submit_medius_move(3, 4).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !queue.is_idle() && Instant::now() < deadline {
            queue.pump();
            std::thread::sleep(Duration::from_millis(1));
        }

        assert!(queue.is_idle());
        assert_eq!(
            failed.try_recv().unwrap(),
            Err("Medius writer panicked".into())
        );
        assert_eq!(recovered.try_recv().unwrap(), Ok(()));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn medius_moves_round_trip_through_the_queue_record() {
        for (dx, dy) in [(0, 0), (32, -32), (-1, 1), (i16::MIN, i16::MAX)] {
            assert_eq!(
                decode_medius_move(&encode_medius_move(dx, dy)),
                Some((dx, dy))
            );
        }
        assert_eq!(decode_medius_move(b"unsupported"), None);
        assert_eq!(decode_medius_move(&[b'M', 1, 0, 2]), None);
        assert!(super::is_medius_left_click(
            &super::encode_medius_left_click()
        ));
        assert!(!super::is_medius_left_click(b"unsupported"));

        let moves = Arc::new(Mutex::new(Vec::new()));
        let seen = moves.clone();
        let writer = ThreadedSerialWriter::spawn(move |bytes, _| {
            let (dx, dy) = decode_medius_move(bytes).ok_or("not a Medius move")?;
            seen.lock().unwrap().push((dx, dy));
            Ok(())
        })
        .unwrap();
        let mut queue = SerialCommandQueue::new(writer);
        let first = queue.submit_medius_move(7, -3).unwrap();
        let wrong = queue
            .submit(QueuedSerialCommand {
                kind: 0,
                bytes: b"unsupported".to_vec(),
                request_ms: MOVE_REQUEST_MS,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !queue.is_idle() && Instant::now() < deadline {
            queue.pump();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(first.try_recv().unwrap(), Ok(()));
        assert_eq!(wrong.try_recv().unwrap(), Err("not a Medius move".into()));
        assert_eq!(moves.lock().unwrap().as_slice(), &[(7, -3)]);
    }

    #[test]
    fn medius_click_record_is_retained_until_completion() {
        let mut queue = SerialCommandQueue::new(FakeWriter::default());
        let completion = queue.submit_medius_left_click().unwrap();
        queue.pump();
        assert_eq!(queue.in_flight_bytes(), Some(b"C".as_slice()));
        queue.writer.completions[0] = Some(Ok(()));
        queue.pump();
        assert_eq!(completion.try_recv().unwrap(), Ok(()));
    }
}
