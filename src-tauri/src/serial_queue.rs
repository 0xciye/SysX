use std::collections::VecDeque;
use std::io::{ErrorKind, Write};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

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
    // `request_ms` bounds how long the writer may spend getting the bytes into the
    // port; it is not an end-to-end deadline (the device never acknowledges moves).
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

    pub fn submit_move(
        &mut self,
        dx: i32,
        dy: i32,
    ) -> Result<Receiver<Result<(), String>>, String> {
        // Same line ending as the Ferrum handshake and button commands.
        let bytes = format!("km.move({dx},{dy})\n").into_bytes();
        self.submit(QueuedSerialCommand {
            kind: 0,
            bytes,
            request_ms: MOVE_REQUEST_MS,
        })
    }

    pub fn submit_left_click(&mut self) -> Result<Receiver<Result<(), String>>, String> {
        self.submit(QueuedSerialCommand {
            kind: 0,
            bytes: b"km.click()\n".to_vec(),
            request_ms: MOVE_REQUEST_MS,
        })
    }

    // Advance at most one request. `request_ms` is handed to the writer as its
    // per-command write budget.
    pub fn pump(&mut self) {
        if let Some(in_flight) = self.in_flight.as_mut() {
            let Some(result) = self.writer.poll_completion(&mut in_flight.ticket) else {
                return;
            };
            let in_flight = self.in_flight.take().expect("in-flight request exists");
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
/// retained bytes and its `request_ms` budget.
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
                    let _ = job.completion.send(sink(&job.bytes, job.budget));
                }
            })
            .map_err(|error| format!("serial writer thread failed to start: {error}"))?;
        Ok(Self { jobs })
    }

    /// Ferrum: write the text command to the port, retrying port write timeouts
    /// until the budget is spent and resuming partial writes.
    pub fn for_port<P: Write + Send + 'static>(mut port: P) -> Result<Self, String> {
        Self::spawn(move |bytes, budget| write_within(&mut port, bytes, budget))
    }
}

const MEDIUS_MOVE_TAG: u8 = b'M';

/// Medius moves go through the vendor crate's binary protocol, so the queue
/// retains a compact `[tag, dx_le, dy_le]` record that the Medius sink decodes.
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

impl<W: AsyncSerialWriter> SerialCommandQueue<W> {
    pub fn submit_medius_left_click(&mut self) -> Result<Receiver<Result<(), String>>, String> {
        self.submit(QueuedSerialCommand {
            kind: 0,
            bytes: encode_medius_left_click(),
            request_ms: MOVE_REQUEST_MS,
        })
    }
}

fn write_within<P: Write>(port: &mut P, bytes: &[u8], budget: Duration) -> Result<(), String> {
    let deadline = Instant::now() + budget;
    let mut written = 0;
    while written < bytes.len() {
        match port.write(&bytes[written..]) {
            Ok(0) => return Err("serial port accepted no bytes".into()),
            Ok(count) => written += count,
            Err(error)
                if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted)
                    && Instant::now() < deadline => {}
            Err(error) => {
                return Err(format!(
                    "serial write failed after {written}/{} bytes: {error}",
                    bytes.len()
                ))
            }
        }
    }
    Ok(())
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
        decode_medius_move, encode_medius_move, write_within, AsyncSerialWriter,
        QueuedSerialCommand, SerialCommandQueue, ThreadedSerialWriter, MOVE_REQUEST_MS,
    };
    use std::collections::VecDeque;
    use std::io::{self, ErrorKind, Write};
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
        let first = queue.submit_move(12, -7).unwrap();
        let second = queue.submit_move(-3, 5).unwrap();
        queue.pump();

        assert_eq!(
            queue.in_flight_bytes(),
            Some(b"km.move(12,-7)\n".as_slice())
        );
        assert_eq!(queue.in_flight_request_ms(), Some(MOVE_REQUEST_MS));
        assert_eq!(queue.queued_len(), 1);
        assert!(first.try_recv().is_err());
        assert_eq!(queue.writer.submitted.len(), 1);

        queue.writer.completions[0] = Some(Ok(()));
        queue.pump();
        assert_eq!(first.try_recv().unwrap(), Ok(()));
        assert_eq!(queue.writer.submitted.len(), 2);
        assert_eq!(queue.in_flight_bytes(), Some(b"km.move(-3,5)\n".as_slice()));
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
        let result = queue.submit_move(1, 2).unwrap();
        queue.pump();
        assert_eq!(
            result.try_recv().unwrap(),
            Err("writer rejected request".into())
        );
    }

    /// Accepts at most `chunk` bytes per call and times out on every other call.
    struct SlowPort {
        written: Arc<Mutex<Vec<u8>>>,
        chunk: usize,
        calls: usize,
    }

    impl Write for SlowPort {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls % 2 == 1 {
                return Err(io::Error::new(ErrorKind::TimedOut, "busy"));
            }
            let count = bytes.len().min(self.chunk);
            self.written
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_retries_timeouts_and_resumes_partial_writes_within_budget() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut port = SlowPort {
            written: written.clone(),
            chunk: 3,
            calls: 0,
        };
        write_within(&mut port, b"km.move(1,2)\n", Duration::from_millis(100)).unwrap();
        assert_eq!(written.lock().unwrap().as_slice(), b"km.move(1,2)\n");
    }

    #[test]
    fn write_fails_once_the_request_budget_is_spent() {
        struct StuckPort;
        impl Write for StuckPort {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                std::thread::sleep(Duration::from_millis(2));
                Err(io::Error::new(ErrorKind::TimedOut, "stuck"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let started = Instant::now();
        let error =
            write_within(&mut StuckPort, b"km.move(1,2)\n", Duration::from_millis(20)).unwrap_err();
        assert!(error.contains("0/13 bytes"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn threaded_writer_delivers_moves_in_order_and_reports_completion() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let port = SlowPort {
            written: written.clone(),
            chunk: 4,
            calls: 0,
        };
        let mut queue = SerialCommandQueue::new(ThreadedSerialWriter::for_port(port).unwrap());
        let first = queue.submit_move(5, -6).unwrap();
        let second = queue.submit_move(-32, 32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !queue.is_idle() && Instant::now() < deadline {
            queue.pump();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(queue.is_idle());
        assert_eq!(first.try_recv().unwrap(), Ok(()));
        assert_eq!(second.try_recv().unwrap(), Ok(()));
        assert_eq!(
            written.lock().unwrap().as_slice(),
            b"km.move(5,-6)\nkm.move(-32,32)\n"
        );
    }

    #[test]
    fn medius_moves_round_trip_through_the_queue_record() {
        for (dx, dy) in [(0, 0), (32, -32), (-1, 1), (i16::MIN, i16::MAX)] {
            assert_eq!(
                decode_medius_move(&encode_medius_move(dx, dy)),
                Some((dx, dy))
            );
        }
        assert_eq!(decode_medius_move(b"km.move(1,2)\n"), None);
        assert_eq!(decode_medius_move(&[b'M', 1, 0, 2]), None);
        assert!(super::is_medius_left_click(
            &super::encode_medius_left_click()
        ));
        assert!(!super::is_medius_left_click(b"km.click()\n"));

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
        let wrong = queue.submit_move(1, 1).unwrap();
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
    fn ferrum_left_click_uses_the_documented_km_command() {
        let mut queue = SerialCommandQueue::new(FakeWriter::default());
        let completion = queue.submit_left_click().unwrap();
        queue.pump();
        assert_eq!(queue.in_flight_bytes(), Some(b"km.click()\n".as_slice()));
        queue.writer.completions[0] = Some(Ok(()));
        queue.pump();
        assert_eq!(completion.try_recv().unwrap(), Ok(()));
    }
}
