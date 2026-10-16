use crate::results::DetectionResult;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

enum StoreMessage {
    Result(DetectionResult),
    Stop(mpsc::Sender<Result<(), String>>),
}

pub struct DetectionDataStore {
    sender: SyncSender<StoreMessage>,
    worker: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl DetectionDataStore {
    pub fn new(session_dir: impl Into<PathBuf>, queue_size: usize) -> io::Result<Self> {
        let session_dir = session_dir.into();
        fs::create_dir_all(&session_dir)?;
        let file_path = session_dir.join("detections.jsonl");
        let (sender, receiver) = mpsc::sync_channel::<StoreMessage>(queue_size.max(1));
        let worker = thread::Builder::new()
            .name("sysx-detection-store".into())
            .spawn(move || {
                let result = writer_loop(&file_path, receiver);
                if let Err(error) = result {
                    eprintln!("Could not write detection data: {error}");
                }
            })?;
        Ok(Self {
            sender,
            worker: std::sync::Mutex::new(Some(worker)),
        })
    }
    pub fn submit(&self, result: DetectionResult) -> bool {
        self.sender.try_send(StoreMessage::Result(result)).is_ok()
    }
    pub fn close(&self, timeout: Duration) -> Result<(), String> {
        let mut slot = self
            .worker
            .lock()
            .map_err(|_| "detection writer lock poisoned")?;
        if let Some(worker) = slot.take() {
            let (tx, rx) = mpsc::channel();
            self.sender
                .send(StoreMessage::Stop(tx))
                .map_err(|e| e.to_string())?;
            let result = rx.recv_timeout(timeout).map_err(|e| e.to_string())?;
            if !worker.is_finished() {
                return Err("detection writer did not stop before timeout".into());
            }
            worker
                .join()
                .map_err(|_| "detection writer panicked".to_string())?;
            result
        } else {
            Ok(())
        }
    }
}

fn writer_loop(path: &Path, receiver: mpsc::Receiver<StoreMessage>) -> io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut writer = BufWriter::new(file);
    while let Ok(message) = receiver.recv() {
        match message {
            StoreMessage::Result(result) => {
                if result.is_valid() {
                    serde_json::to_writer(&mut writer, &result)?;
                    writer.write_all(b"\n")?;
                }
            }
            StoreMessage::Stop(done) => {
                let status = writer.flush().map_err(|e| e.to_string());
                let _ = done.send(status);
                return Ok(());
            }
        }
    }
    writer.flush()
}
