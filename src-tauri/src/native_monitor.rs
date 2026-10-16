//! Shared native monitor model and bounded IPC/decode workers. No browser UI.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use sysx_ui::protocol::{BackendState, LiveFrame};

pub const MAX_JPEG: usize = 64 * 1024 * 1024;
pub const MAX_META: usize = 1024 * 1024;
pub struct PendingFrame {
    pub metadata: LiveFrame,
    pub jpeg: Vec<u8>,
    pub received: Instant,
}
pub struct Frame {
    pub metadata: LiveFrame,
    pub rgba: Vec<u8>,
    pub received: Instant,
}
#[derive(Default, Clone)]
pub struct View {
    pub state: Option<BackendState>,
    pub frame: Option<Arc<Frame>>,
    pub connected: bool,
    pub error: String,
    pub revision: u64,
}
impl View {
    pub fn live_frame(&self) -> Option<&Frame> {
        self.frame.as_deref().filter(|f| {
            self.connected && !self.has_error() && f.received.elapsed() < Duration::from_secs(1)
        })
    }
    fn has_error(&self) -> bool {
        !self.error.is_empty()
            || self
                .state
                .as_ref()
                .and_then(|s| s.last_error.as_deref())
                .is_some_and(|e| !e.is_empty())
    }
    pub fn status(&self) -> (&str, u32) {
        if self.has_error() {
            ("Receiver error", 0xef6c6c)
        } else if !self.connected {
            ("Starting receiver", 0xe8b858)
        } else if self.live_frame().is_some() {
            ("Receiving", 0x43cfa0)
        } else {
            ("Waiting for sender", 0xe8b858)
        }
    }
}
pub fn parse_frame(bytes: &[u8]) -> Result<PendingFrame, String> {
    let prefix: [u8; 4] = bytes
        .get(..4)
        .ok_or("short preview message")?
        .try_into()
        .unwrap();
    let len = u32::from_be_bytes(prefix) as usize;
    if len == 0 || len > MAX_META {
        return Err("invalid preview metadata size".into());
    }
    let end = 4usize.checked_add(len).ok_or("preview size overflow")?;
    let metadata: LiveFrame =
        serde_json::from_slice(bytes.get(4..end).ok_or("truncated preview metadata")?)
            .map_err(|e| e.to_string())?;
    let jpeg = bytes.get(end..).ok_or("missing preview JPEG")?;
    let (w, h) = (metadata.source_width, metadata.source_height);
    if w == 0 || h == 0 || w > 8192 || h > 8192 || u64::from(w) * u64::from(h) > 33_554_432 {
        return Err("preview dimensions exceed limits".into());
    }
    if jpeg.len() > MAX_JPEG || !jpeg.starts_with(&[255, 216]) || !jpeg.ends_with(&[255, 217]) {
        return Err("invalid preview JPEG".into());
    }
    Ok(PendingFrame {
        metadata,
        jpeg: jpeg.to_vec(),
        received: Instant::now(),
    })
}
pub fn decode(frame: PendingFrame) -> Result<Frame, String> {
    let mut reader = image::ImageReader::with_format(
        std::io::Cursor::new(&frame.jpeg),
        image::ImageFormat::Jpeg,
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| e.to_string())?;
    if decoded.width() != frame.metadata.source_width
        || decoded.height() != frame.metadata.source_height
    {
        return Err("preview JPEG dimensions do not match metadata".into());
    }
    let rgba = decoded.to_rgba8().into_raw();
    Ok(Frame {
        metadata: frame.metadata,
        rgba,
        received: frame.received,
    })
}
#[derive(Default)]
pub struct Inbox {
    pending: Mutex<Option<PendingFrame>>,
    wake: Condvar,
}
impl Inbox {
    fn put(&self, frame: PendingFrame) {
        if let Ok(mut slot) = self.pending.lock() {
            *slot = Some(frame);
            self.wake.notify_one();
        }
    }
}
pub struct Workers {
    pub view: Arc<Mutex<View>>,
    stop: Arc<AtomicBool>,
    inbox: Arc<Inbox>,
    threads: Vec<std::thread::JoinHandle<()>>,
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.inbox.wake.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
impl Workers {
    pub fn start(
        port: u16,
        token: String,
        view: Arc<Mutex<View>>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let mut workers = Self {
            view,
            stop: Arc::new(AtomicBool::new(false)),
            inbox: Arc::new(Inbox::default()),
            threads: vec![],
        };
        let (view, stop, inbox) = (
            Arc::clone(&workers.view),
            Arc::clone(&workers.stop),
            Arc::clone(&workers.inbox),
        );
        let notify_decode = Arc::clone(&notify);
        workers.threads.push(
            std::thread::Builder::new()
                .name("sysx-native-decode".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let frame = {
                            let Ok(slot) = inbox.pending.lock() else {
                                break;
                            };
                            let Ok((mut slot, _)) = inbox.wake.wait_timeout_while(
                                slot,
                                Duration::from_millis(100),
                                |s| s.is_none() && !stop.load(Ordering::Acquire),
                            ) else {
                                break;
                            };
                            slot.take()
                        };
                        if let Some(frame) = frame {
                            match decode(frame) {
                                Ok(frame) if frame.received.elapsed() < Duration::from_secs(1) => {
                                    if let Ok(mut view) = view.lock() {
                                        view.frame = Some(Arc::new(frame));
                                        view.error.clear();
                                        view.revision += 1;
                                        notify_decode();
                                    }
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    if let Ok(mut view) = view.lock() {
                                        view.error = error;
                                        view.revision += 1;
                                        notify_decode();
                                    }
                                }
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
        let (view, stop, inbox) = (
            Arc::clone(&workers.view),
            Arc::clone(&workers.stop),
            Arc::clone(&workers.inbox),
        );
        workers.threads.push(
            std::thread::Builder::new()
                .name("sysx-native-preview".into())
                .spawn(move || {
                    use tungstenite::{protocol::WebSocketConfig, Message};
                    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                    while !stop.load(Ordering::Acquire) {
                        let socket = std::net::TcpStream::connect_timeout(
                            &address,
                            Duration::from_millis(500),
                        )
                        .and_then(|stream| {
                            stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                            stream.set_write_timeout(Some(Duration::from_millis(500)))?;
                            let _ = stream.set_nodelay(true);
                            Ok(stream)
                        });
                        let config = WebSocketConfig {
                            max_message_size: Some(MAX_JPEG + MAX_META + 4),
                            max_frame_size: Some(MAX_JPEG + MAX_META + 4),
                            ..Default::default()
                        };
                        let socket = socket.ok().and_then(|stream| {
                            tungstenite::client::client_with_config(
                                format!(
                                    "ws://127.0.0.1:{port}/receiver?role=monitor&token={token}"
                                ),
                                stream,
                                Some(config),
                            )
                            .ok()
                        });
                        if let Some((mut socket, _)) = socket {
                            // Use nonblocking reads so partial WebSocket messages survive polling.
                            let _ = socket.get_mut().set_nonblocking(true);
                            if let Ok(mut view) = view.lock() {
                                view.connected = true;
                                view.error.clear();
                                view.revision += 1;
                                notify();
                            }
                            while !stop.load(Ordering::Acquire) {
                                match socket.read() {
                                    Ok(Message::Text(text)) => {
                                        if let Ok(value) =
                                            serde_json::from_str::<serde_json::Value>(&text)
                                        {
                                            if value["event"] == "state" {
                                                if let Ok(state) =
                                                    serde_json::from_value::<BackendState>(value)
                                                {
                                                    if let Ok(mut view) = view.lock() {
                                                        if state.status == "waiting" {
                                                            view.frame = None;
                                                        }
                                                        view.state = Some(state);
                                                        view.revision += 1;
                                                        notify();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Ok(Message::Binary(bytes)) => match parse_frame(&bytes) {
                                        Ok(frame) => inbox.put(frame),
                                        Err(error) => {
                                            if let Ok(mut view) = view.lock() {
                                                view.error = error;
                                                view.revision += 1;
                                                notify();
                                            }
                                        }
                                    },
                                    Ok(Message::Close(_)) => break,
                                    Ok(_) => {}
                                    Err(tungstenite::Error::Io(e))
                                        if e.kind() == std::io::ErrorKind::WouldBlock =>
                                    {
                                        std::thread::sleep(Duration::from_millis(2));
                                    }
                                    Err(_) => break,
                                }
                            }
                            let _ = socket.close(None);
                        }
                        if let Ok(mut view) = view.lock() {
                            view.connected = false;
                            view.frame = None;
                            view.state = None;
                            view.error = "Backend connection lost".into();
                            view.revision += 1;
                            notify();
                        }
                        // Interruptible retry, including shutdown after a failed handshake.
                        for _ in 0..25 {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
        Ok(workers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_preview_headers_before_decode() {
        assert!(parse_frame(&[]).is_err());
        assert!(parse_frame(&((MAX_META + 1) as u32).to_be_bytes()).is_err());
        assert!(parse_frame(&[0, 0, 0, 5, b'{']).is_err());
    }
    #[test]
    fn native_status_distinguishes_start_wait_and_error() {
        let mut view = View::default();
        assert_eq!(view.status().0, "Starting receiver");
        view.connected = true;
        assert_eq!(view.status().0, "Waiting for sender");
        view.error = "decode failed".into();
        assert_eq!(view.status().0, "Receiver error");
    }
}

#[cfg(test)]
pub(crate) fn sample_metadata() -> LiveFrame {
    serde_json::from_value(serde_json::json!({
        "frame_id":1,"source":"udp","source_width":192,"source_height":192,"crop_x":0,"crop_y":0,"mode":"pixel_seek","model_id":null,
        "processing_fps":240.0,"source_fps":240.0,"dropped_input_frames":0,"transport_dropped_frames":0,
        "detection":{"frame_id":1,"timestamp_ns":1,"mode":"pixel_seek","model_id":null,"source_width":192,"source_height":192,"processed_width":192,"processed_height":192,
            "objects":[{"class_id":0,"class_name":"pixel","bbox_xyxy":[80.0,60.0,112.0,130.0],"confidence":null}],"total_processing_ms":2.4,"target_xy":[96.0,60.0]}
    })).unwrap()
}
#[cfg(test)]
mod frame_tests {
    use super::*;
    #[test]
    fn preview_packet_decodes_exact_dimensions_and_rejects_mismatch() {
        let metadata = sample_metadata();
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        let json = serde_json::to_vec(&metadata).unwrap();
        let mut packet = (json.len() as u32).to_be_bytes().to_vec();
        packet.extend(json);
        packet.extend(jpeg);
        let pending = parse_frame(&packet).unwrap();
        let frame = decode(pending).unwrap();
        assert_eq!(frame.rgba.len(), 192 * 192 * 4);
        let mut wrong = sample_metadata();
        wrong.source_width = 193;
        assert!(decode(PendingFrame {
            metadata: wrong,
            jpeg: jpeg.to_vec(),
            received: Instant::now()
        })
        .is_err());
    }
    #[test]
    fn mailbox_keeps_latest_and_stale_preview_is_hidden() {
        let inbox = Inbox::default();
        let mut metadata = sample_metadata();
        for id in 0..10 {
            metadata.frame_id = id;
            inbox.put(PendingFrame {
                metadata: metadata.clone(),
                jpeg: vec![255, 216, 255, 217],
                received: Instant::now(),
            });
        }
        assert_eq!(
            inbox
                .pending
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .metadata
                .frame_id,
            9
        );
        let mut view = View {
            connected: true,
            ..Default::default()
        };
        view.frame = Some(Arc::new(Frame {
            metadata,
            rgba: vec![],
            received: Instant::now() - Duration::from_secs(2),
        }));
        assert!(view.live_frame().is_none());
        assert_eq!(view.status().0, "Waiting for sender");
    }
}

#[cfg(test)]
mod ipc_tests {
    use super::*;
    #[test]
    fn native_client_receives_frame_and_mouse_state_without_a_web_page() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut socket = tungstenite::accept_hdr(
                stream,
                |request: &tungstenite::handshake::server::Request,
                 response: tungstenite::handshake::server::Response| {
                    assert!(request.uri().query().unwrap().contains("role=monitor"));
                    Ok(response)
                },
            )
            .unwrap();
            socket.send(tungstenite::Message::Text(serde_json::json!({"event":"state","status":"streaming","profiles":[{"index":0,"name":"Default"}],"device":{"state":"connected","name":"Medius","buttons":[false,true,false,false,false]}}).to_string())).unwrap();
            let metadata = serde_json::to_vec(&sample_metadata()).unwrap();
            let mut bytes = (metadata.len() as u32).to_be_bytes().to_vec();
            bytes.extend(metadata);
            bytes.extend(include_bytes!("../../src/assets/red-cube-192.jpg"));
            socket.send(tungstenite::Message::Binary(bytes)).unwrap();
            while let Ok(message) = socket.read() {
                if message.is_close() {
                    break;
                }
            }
        });
        let view = Arc::new(Mutex::new(View::default()));
        let workers = Workers::start(
            port,
            "local-test-only".into(),
            Arc::clone(&view),
            Arc::new(|| {}),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if view.lock().unwrap().live_frame().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "native preview did not arrive");
            std::thread::sleep(Duration::from_millis(5));
        }
        {
            let view = view.lock().unwrap();
            assert!(view.connected);
            assert_eq!(
                view.state
                    .as_ref()
                    .unwrap()
                    .device
                    .as_ref()
                    .unwrap()
                    .buttons[1],
                Some(true)
            );
            assert_eq!(view.live_frame().unwrap().rgba.len(), 192 * 192 * 4);
        }
        drop(workers);
        server.join().unwrap();
    }
}
