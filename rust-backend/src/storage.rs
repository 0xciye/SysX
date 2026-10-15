use crate::results::DetectionResult;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

enum StoreMessage {
    Result(DetectionResult),
    Flush(mpsc::Sender<Result<(), String>>),
    Stop(mpsc::Sender<Result<(), String>>),
}

pub struct DetectionDataStore {
    session_dir: PathBuf,
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
            session_dir,
            sender,
            worker: std::sync::Mutex::new(Some(worker)),
        })
    }
    pub fn submit(&self, result: DetectionResult) -> bool {
        self.sender.try_send(StoreMessage::Result(result)).is_ok()
    }
    pub fn flush(&self, timeout: Duration) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.sender
            .send(StoreMessage::Flush(tx))
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(timeout).map_err(|e| e.to_string())?
    }
    pub fn export_csv(&self, path: impl AsRef<Path>, timeout: Duration) -> Result<(), String> {
        self.flush(timeout)?;
        let input =
            File::open(self.session_dir.join("detections.jsonl")).map_err(|e| e.to_string())?;
        let output_path = path.as_ref();
        if output_path.canonicalize().ok()
            == self
                .session_dir
                .join("detections.jsonl")
                .canonicalize()
                .ok()
        {
            return Err("CSV destination cannot replace JSONL data".into());
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut writer = BufWriter::new(File::create(output_path).map_err(|e| e.to_string())?);
        writeln!(writer, "frame_id,timestamp_ns,mode,model_id,inference_device,source_width,source_height,class_id,class_name,x1,y1,x2,y2,confidence,pixel_count,total_processing_ms").map_err(|e| e.to_string())?;
        for (index, line) in BufReader::new(input).lines().enumerate() {
            let line = line.map_err(|e| e.to_string())?;
            let value: serde_json::Value = serde_json::from_str(&line)
                .map_err(|e| format!("invalid JSONL line {}: {e}", index + 1))?;
            if let Some(objects) = value.get("objects").and_then(|v| v.as_array()) {
                for object in objects {
                    let bbox = object
                        .get("bbox_xyxy")
                        .and_then(|v| v.as_array())
                        .ok_or("invalid bbox in stored result")?;
                    let fields = [
                        value["frame_id"].to_string(),
                        value["timestamp_ns"].to_string(),
                        csv_field(value["mode"].as_str().unwrap_or("")),
                        csv_field(value["model_id"].as_str().unwrap_or("")),
                        csv_field(value["inference_device"].as_str().unwrap_or("")),
                        value["source_width"].to_string(),
                        value["source_height"].to_string(),
                        object["class_id"].to_string(),
                        csv_field(object["class_name"].as_str().unwrap_or("")),
                        bbox[0].to_string(),
                        bbox[1].to_string(),
                        bbox[2].to_string(),
                        bbox[3].to_string(),
                        object["confidence"].to_string(),
                        object["pixel_count"].to_string(),
                        value["total_processing_ms"].to_string(),
                    ];
                    writeln!(writer, "{}", fields.join(",")).map_err(|e| e.to_string())?;
                }
            }
        }
        writer.flush().map_err(|e| e.to_string())
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
            StoreMessage::Flush(done) => {
                let status = writer.flush().map_err(|e| e.to_string());
                let _ = done.send(status);
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

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}
