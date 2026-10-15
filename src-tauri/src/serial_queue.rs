use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};

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
    fn submit(&mut self, bytes: &[u8]) -> Result<Self::Ticket, String>;
    fn poll_completion(&mut self, ticket: &mut Self::Ticket) -> Option<Result<(), String>>;
}

struct PendingCommand {
    command: QueuedSerialCommand,
    completion: Sender<Result<(), String>>,
}

struct InFlight<Ticket> {
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
        let bytes = format!("km.move({dx},{dy})\r\n").into_bytes();
        self.submit(QueuedSerialCommand {
            kind: 0,
            bytes,
            request_ms: MOVE_REQUEST_MS,
        })
    }

    // Advance at most one request. `request_ms` is preserved as request metadata;
    // it is not interpreted as a timeout for submission or completion.
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
        match self.writer.submit(&pending.command.bytes) {
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

    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    pub fn in_flight_bytes(&self) -> Option<&[u8]> {
        self.in_flight
            .as_ref()
            .map(|request| request.command.bytes.as_slice())
    }

    pub fn in_flight_request_ms(&self) -> Option<u32> {
        self.in_flight
            .as_ref()
            .map(|request| request.command.request_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::{AsyncSerialWriter, QueuedSerialCommand, SerialCommandQueue, MOVE_REQUEST_MS};
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeWriter {
        submitted: Vec<Vec<u8>>,
        completions: VecDeque<Option<Result<(), String>>>,
    }

    impl AsyncSerialWriter for FakeWriter {
        type Ticket = usize;

        fn submit(&mut self, bytes: &[u8]) -> Result<Self::Ticket, String> {
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
            Some(b"km.move(12,-7)\r\n".as_slice())
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
            Some(b"km.move(-3,5)\r\n".as_slice())
        );
        assert!(second.try_recv().is_err());
    }

    #[test]
    fn reports_submission_errors_and_rejects_unimplemented_kinds() {
        struct FailingWriter;
        impl AsyncSerialWriter for FailingWriter {
            type Ticket = ();
            fn submit(&mut self, _: &[u8]) -> Result<Self::Ticket, String> {
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
}
