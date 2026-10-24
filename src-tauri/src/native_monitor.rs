//! Shared native monitor model and bounded IPC/decode workers. No browser UI.
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use sysx_ui::protocol::{BackendState, LiveFrame};

pub const MAX_JPEG: usize = 64 * 1024 * 1024;
pub const MAX_META: usize = 1024 * 1024;
const MAX_FRAME_PIXELS: u64 = crate::jpeg::MAX_PIXELS;
const MAX_COLOR_MASK: usize = (MAX_FRAME_PIXELS as usize + 7) / 8;
const MAX_FRAME_MESSAGE: usize = MAX_JPEG + MAX_META + MAX_COLOR_MASK + 4;
pub struct PendingFrame {
    pub metadata: LiveFrame,
    /// Original IPC message; `payload_start` and `mask_bytes` split its payload
    /// without copying the JPEG or retaining an oversized mask allocation.
    pub payload: Vec<u8>,
    pub payload_start: usize,
    pub mask_bytes: usize,
    pub received: Instant,
}
pub struct Frame {
    pub metadata: LiveFrame,
    /// Moved into Slint on first display; decoding writes directly here.
    pub rgba: Mutex<Option<SharedPixelBuffer<Rgba8Pixel>>>,
    pub received: Instant,
}

#[derive(Default)]
pub struct DrawGate {
    pending: Mutex<bool>,
    changed: Condvar,
}

impl DrawGate {
    pub fn mark_pending(&self) {
        *lock_recover(&self.pending) = true;
    }

    pub fn mark_drawn(&self) {
        *lock_recover(&self.pending) = false;
        self.changed.notify_one();
    }

    fn wait(&self, stop: &AtomicBool) {
        let pending = lock_recover(&self.pending);
        if *pending && !stop.load(Ordering::Acquire) {
            let _guard = self
                .changed
                .wait_while(pending, |pending| *pending && !stop.load(Ordering::Acquire))
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

fn catch_worker_panic<T>(run: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic");
        Err(format!("preview decoder recovered from panic: {message}"))
    })
}

#[derive(Default, Clone)]
pub struct View {
    pub state: Option<BackendState>,
    pub frame: Option<Arc<Frame>>,
    pub connected: bool,
    pub error: String,
    pub revision: u64,
    /// Set when a decoded frame is published, cleared once the window has
    /// drawn it: frames replaced before a redraw are never seen, so the decoder
    /// waits instead of decoding them.
    pub awaiting_draw: Arc<DrawGate>,
}

pub(crate) fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(error) => {
            let guard = error.into_inner();
            mutex.clear_poison();
            guard
        }
    }
}

fn lock_view(view: &Mutex<View>) -> MutexGuard<'_, View> {
    lock_recover(view)
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
pub fn parse_frame(bytes: Vec<u8>) -> Result<PendingFrame, String> {
    let prefix: [u8; 4] = bytes
        .get(..4)
        .ok_or("short preview message")?
        .try_into()
        .map_err(|_| "short preview message")?;
    let len = u32::from_be_bytes(prefix) as usize;
    if len == 0 || len > MAX_META {
        return Err("invalid preview metadata size".into());
    }
    let end = 4usize.checked_add(len).ok_or("preview size overflow")?;
    let metadata: LiveFrame =
        serde_json::from_slice(bytes.get(4..end).ok_or("truncated preview metadata")?)
            .map_err(|e| e.to_string())?;
    let (w, h) = (metadata.source_width, metadata.source_height);
    validate_dimensions(w, h)?;
    let payload = bytes.get(end..).ok_or("truncated preview payload")?;
    let mask_bytes = validated_mask_bytes(&metadata)?;
    if payload.len() < mask_bytes {
        return Err("truncated color filter mask".into());
    }
    let jpeg = payload
        .get(mask_bytes..)
        .ok_or("truncated color filter mask")?;
    if jpeg.len() > MAX_JPEG || !jpeg.starts_with(&[255, 216]) || !jpeg.ends_with(&[255, 217]) {
        return Err("invalid preview JPEG".into());
    }
    Ok(PendingFrame {
        metadata,
        payload: bytes,
        payload_start: end,
        mask_bytes,
        received: Instant::now(),
    })
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > crate::jpeg::MAX_SIDE
        || height > crate::jpeg::MAX_SIDE
        || u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS
    {
        return Err("preview dimensions exceed limits".into());
    }
    Ok(())
}

fn validated_mask_bytes(metadata: &LiveFrame) -> Result<usize, String> {
    let Some(info) = metadata.color_mask else {
        return Ok(0);
    };
    let inside = u64::from(metadata.crop_x) + u64::from(info.width)
        <= u64::from(metadata.source_width)
        && u64::from(metadata.crop_y) + u64::from(info.height) <= u64::from(metadata.source_height);
    let expected = (info.width as usize)
        .checked_mul(info.height as usize)
        .and_then(|pixels| pixels.checked_add(7))
        .map(|bits| bits / 8);
    if !inside
        || info.width == 0
        || info.height == 0
        || expected.is_none_or(|bytes| bytes > MAX_COLOR_MASK || info.bytes != bytes)
    {
        return Err("invalid color filter mask".into());
    }
    Ok(info.bytes)
}

/// Color filter: keeps the pixels whose mask bit is set and turns every other
/// pixel (including any outside the masked area) black.
fn apply_color_filter(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    metadata: &LiveFrame,
    mask: &[u8],
) -> Result<(), String> {
    let Some(info) = metadata.color_mask else {
        return Ok(());
    };
    let expected_pixels = (info.width as usize)
        .checked_mul(info.height as usize)
        .ok_or_else(|| "color filter mask size overflow".to_string())?;
    let expected_bytes = expected_pixels.div_ceil(8);
    let expected_rgba = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "preview image size overflow".to_string())?;
    let right = metadata
        .crop_x
        .checked_add(info.width)
        .filter(|right| *right <= metadata.source_width)
        .ok_or_else(|| "color filter mask exceeds preview width".to_string())?;
    let bottom = metadata
        .crop_y
        .checked_add(info.height)
        .filter(|bottom| *bottom <= metadata.source_height)
        .ok_or_else(|| "color filter mask exceeds preview height".to_string())?;
    if width == 0
        || height == 0
        || info.width == 0
        || info.height == 0
        || rgba.len() != expected_rgba
        || mask.len() != expected_bytes
    {
        return Err("invalid color filter mask dimensions".into());
    }

    let mask_width = info.width as usize;
    if (width, height) == (metadata.source_width, metadata.source_height) {
        let left = metadata.crop_x as usize;
        let top = metadata.crop_y as usize;
        let right = right as usize;
        let bottom = bottom as usize;
        let row_bytes = width as usize * 4;
        for (y, row) in rgba.chunks_exact_mut(row_bytes).enumerate() {
            if y < top || y >= bottom {
                row.chunks_exact_mut(4).for_each(|pixel| pixel[..3].fill(0));
                continue;
            }
            row[..left * 4]
                .chunks_exact_mut(4)
                .for_each(|pixel| pixel[..3].fill(0));
            row[right * 4..]
                .chunks_exact_mut(4)
                .for_each(|pixel| pixel[..3].fill(0));

            let mask_row = y - top;
            let pixels = &mut row[left * 4..right * 4];
            if mask_width % 8 == 0 {
                let row_mask = &mask[mask_row * (mask_width / 8)..][..mask_width / 8];
                for (group, &bits) in pixels.chunks_exact_mut(8 * 4).zip(row_mask) {
                    match bits {
                        0 => group
                            .chunks_exact_mut(4)
                            .for_each(|pixel| pixel[..3].fill(0)),
                        u8::MAX => {}
                        bits => {
                            for x in 0..8 {
                                if bits >> x & 1 == 0 {
                                    group[x * 4..x * 4 + 3].fill(0);
                                }
                            }
                        }
                    }
                }
            } else {
                for x in 0..mask_width {
                    let bit = mask_row * mask_width + x;
                    if mask[bit / 8] >> (bit % 8) & 1 == 0 {
                        pixels[x * 4..x * 4 + 3].fill(0);
                    }
                }
            }
        }
        return Ok(());
    }

    let source_width = u64::from(metadata.source_width);
    let source_height = u64::from(metadata.source_height);
    let x_denominator = u64::from(width) * 2;
    let y_denominator = u64::from(height) * 2;
    let x_step = source_width * 2;
    let y_step = source_height * 2;
    let (x_increment, x_remainder_step) = (x_step / x_denominator, x_step % x_denominator);
    let (y_increment, y_remainder_step) = (y_step / y_denominator, y_step % y_denominator);
    let (mut source_y, mut y_remainder) =
        (source_height / y_denominator, source_height % y_denominator);
    let row_bytes = width as usize * 4;
    for row in rgba.chunks_exact_mut(row_bytes) {
        let mask_y = source_y.saturating_sub(u64::from(metadata.crop_y));
        if source_y < u64::from(metadata.crop_y) || mask_y >= u64::from(info.height) {
            row.chunks_exact_mut(4).for_each(|pixel| pixel[..3].fill(0));
        } else {
            let mask_row = mask_y as usize;
            let crop_x = u64::from(metadata.crop_x);
            let mask_width = u64::from(info.width);
            let mask_row_start = mask_row * info.width as usize;
            let (mut source_x, mut x_remainder) =
                (source_width / x_denominator, source_width % x_denominator);
            for pixel in row.chunks_exact_mut(4) {
                let mask_x = source_x.saturating_sub(crop_x);
                let kept = source_x >= crop_x && mask_x < mask_width && {
                    let bit = mask_row_start + mask_x as usize;
                    mask[bit / 8] >> (bit % 8) & 1 != 0
                };
                if !kept {
                    pixel[..3].fill(0);
                }

                source_x += x_increment;
                x_remainder += x_remainder_step;
                if x_remainder >= x_denominator {
                    source_x += 1;
                    x_remainder -= x_denominator;
                }
            }
        }

        source_y += y_increment;
        y_remainder += y_remainder_step;
        if y_remainder >= y_denominator {
            source_y += 1;
            y_remainder -= y_denominator;
        }
    }
    Ok(())
}
pub fn decode(frame: PendingFrame) -> Result<Frame, String> {
    let PendingFrame {
        metadata,
        payload,
        payload_start,
        mask_bytes,
        received,
    } = frame;
    let payload = payload
        .get(payload_start..)
        .ok_or_else(|| "invalid preview payload offset".to_owned())?;
    let mask = if metadata.color_mask.is_some() {
        Some(
            payload
                .get(..mask_bytes)
                .ok_or_else(|| "truncated color filter mask".to_owned())?,
        )
    } else {
        None
    };
    let jpeg = payload
        .get(mask_bytes..)
        .ok_or_else(|| "truncated preview JPEG".to_owned())?;
    if jpeg.len() < 4
        || jpeg.len() > MAX_JPEG
        || !jpeg.starts_with(&[255, 216])
        || !jpeg.ends_with(&[255, 217])
    {
        return Err("invalid preview JPEG".into());
    }
    let (width, height) = (metadata.source_width, metadata.source_height);
    validate_dimensions(width, height)?;
    if mask_bytes != validated_mask_bytes(&metadata)? {
        return Err("invalid color filter mask".into());
    }
    const MAX_PREVIEW_SIDE: u32 = 1280;
    let (preview_width, preview_height) =
        crate::jpeg::preview_dimensions(width, height, MAX_PREVIEW_SIDE);
    let mut rgba = SharedPixelBuffer::<Rgba8Pixel>::new(preview_width, preview_height);
    let decoded_size = crate::jpeg::decode_preview_into(
        jpeg,
        width,
        height,
        MAX_PREVIEW_SIDE,
        true,
        rgba.make_mut_bytes(),
    )
    .map_err(|error| match error {
        crate::jpeg::JpegError::Dimensions { .. } => {
            "preview JPEG dimensions do not match metadata".to_owned()
        }
        crate::jpeg::JpegError::Decode(message) => message,
    })?;
    if decoded_size != (preview_width, preview_height) {
        return Err("preview decoder returned unexpected dimensions".into());
    }
    if let Some(mask) = mask {
        apply_color_filter(
            rgba.make_mut_bytes(),
            preview_width,
            preview_height,
            &metadata,
            mask,
        )?;
    }
    Ok(Frame {
        metadata,
        rgba: Mutex::new(Some(rgba)),
        received,
    })
}
#[derive(Default)]
pub struct Inbox {
    pending: Mutex<Option<PendingFrame>>,
    wake: Condvar,
}
impl Inbox {
    fn lock_pending(&self) -> MutexGuard<'_, Option<PendingFrame>> {
        lock_recover(&self.pending)
    }

    fn put(&self, frame: PendingFrame) {
        *self.lock_pending() = Some(frame);
        self.wake.notify_one();
    }
}

fn newer_frame_waiting(pending: &Option<PendingFrame>, received: Instant) -> bool {
    pending
        .as_ref()
        .is_some_and(|pending| pending.received >= received)
}

pub struct Workers {
    pub view: Arc<Mutex<View>>,
    stop: Arc<AtomicBool>,
    inbox: Arc<Inbox>,
    threads: Vec<std::thread::JoinHandle<()>>,
}
impl Drop for Workers {
    fn drop(&mut self) {
        {
            // Pair the stop flag with the inbox mutex so the decoder cannot
            // miss this notification between checking its predicate and wait.
            let _pending = self.inbox.lock_pending();
            self.stop.store(true, Ordering::Release);
            self.inbox.wake.notify_all();
        }
        Arc::clone(&lock_view(&self.view).awaiting_draw).mark_drawn();
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
                    let awaiting = Arc::clone(&lock_view(&view).awaiting_draw);
                    while !stop.load(Ordering::Acquire) {
                        let iteration = catch_worker_panic(|| {
                            awaiting.wait(&stop);
                            let frame = {
                                let slot = inbox.lock_pending();
                                let (mut slot, _) = inbox
                                    .wake
                                    .wait_timeout_while(slot, Duration::from_millis(100), |s| {
                                        s.is_none() && !stop.load(Ordering::Acquire)
                                    })
                                    .unwrap_or_else(|error| error.into_inner());
                                slot.take()
                            };
                            if let Some(frame) = frame {
                                if !newer_frame_waiting(&inbox.lock_pending(), frame.received) {
                                    match catch_worker_panic(|| decode(frame)) {
                                        Ok(frame)
                                            if frame.received.elapsed()
                                                < Duration::from_secs(1) =>
                                        {
                                            let published = {
                                                // Keep intake from replacing the slot between the
                                                // supersession check and publishing this frame.
                                                let pending = inbox.lock_pending();
                                                if newer_frame_waiting(&pending, frame.received) {
                                                    false
                                                } else {
                                                    let mut current = lock_view(&view);
                                                    current.frame = Some(Arc::new(frame));
                                                    current.error.clear();
                                                    current.revision += 1;
                                                    awaiting.mark_pending();
                                                    true
                                                }
                                            };
                                            if published {
                                                let _ = catch_worker_panic(|| {
                                                    notify_decode();
                                                    Ok(())
                                                });
                                            }
                                        }
                                        Ok(_) => {}
                                        Err(error) => {
                                            let mut current = lock_view(&view);
                                            current.error = error;
                                            current.revision += 1;
                                            drop(current);
                                            let _ = catch_worker_panic(|| {
                                                notify_decode();
                                                Ok(())
                                            });
                                        }
                                    }
                                }
                            }
                            Ok(())
                        });
                        if let Err(error) = iteration {
                            let mut current = lock_view(&view);
                            current.error = error;
                            current.revision += 1;
                            drop(current);
                            let _ = catch_worker_panic(|| {
                                notify_decode();
                                Ok(())
                            });
                            std::thread::sleep(Duration::from_millis(50));
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
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
                                max_message_size: Some(MAX_FRAME_MESSAGE),
                                max_frame_size: Some(MAX_FRAME_MESSAGE),
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
                                // Block until data arrives; periodically wake to observe shutdown.
                                let _ = socket
                                    .get_mut()
                                    .set_read_timeout(Some(Duration::from_millis(50)));
                                let mut current = lock_view(&view);
                                current.connected = true;
                                current.error.clear();
                                current.revision += 1;
                                drop(current);
                                notify();
                                while !stop.load(Ordering::Acquire) {
                                    match socket.read() {
                                        Ok(Message::Text(text)) => {
                                            if let Ok(value) =
                                                serde_json::from_str::<serde_json::Value>(&text)
                                            {
                                                if value["event"] == "state" {
                                                    if let Ok(state) =
                                                        serde_json::from_value::<BackendState>(
                                                            value,
                                                        )
                                                    {
                                                        let mut current = lock_view(&view);
                                                        if state.status == "waiting" {
                                                            current.frame = None;
                                                        }
                                                        current.state = Some(state);
                                                        current.revision += 1;
                                                        drop(current);
                                                        notify();
                                                    }
                                                }
                                            }
                                        }
                                        Ok(Message::Binary(bytes)) => match parse_frame(bytes) {
                                            Ok(frame) => inbox.put(frame),
                                            Err(error) => {
                                                let mut current = lock_view(&view);
                                                current.error = error;
                                                current.revision += 1;
                                                drop(current);
                                                notify();
                                            }
                                        },
                                        Ok(Message::Close(_)) => break,
                                        Ok(_) => {}
                                        Err(tungstenite::Error::Io(e))
                                            if matches!(
                                                e.kind(),
                                                std::io::ErrorKind::WouldBlock
                                                    | std::io::ErrorKind::TimedOut
                                            ) =>
                                        {
                                            continue;
                                        }
                                        Err(_) => break,
                                    }
                                }
                                let _ = socket.close(None);
                            }
                            let mut current = lock_view(&view);
                            current.connected = false;
                            current.frame = None;
                            current.state = None;
                            current.error = "Backend connection lost".into();
                            current.revision += 1;
                            drop(current);
                            notify();
                            // Interruptible retry, including shutdown after a failed handshake.
                            for _ in 0..25 {
                                if stop.load(Ordering::Acquire) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(10));
                            }
                        }));
                        if let Err(panic) = result {
                            let message = panic
                                .downcast_ref::<String>()
                                .map(String::as_str)
                                .or_else(|| panic.downcast_ref::<&str>().copied())
                                .unwrap_or("unknown panic");
                            let error = format!("Preview worker recovered from panic: {message}");
                            let mut current = lock_view(&view);
                            current.connected = false;
                            current.frame = None;
                            current.state = None;
                            current.error = error;
                            current.revision += 1;
                            drop(current);
                            let _ =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify()));
                            std::thread::sleep(Duration::from_millis(250));
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
    fn view_lock_recovers_after_a_worker_panics_while_holding_it() {
        let view = Arc::new(Mutex::new(View::default()));
        let poisoned = Arc::clone(&view);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.lock().unwrap();
            panic!("simulated preview worker panic");
        }));

        {
            let mut current = lock_view(&view);
            current.error = "recovered".into();
        }
        assert!(!view.is_poisoned());
        assert_eq!(lock_view(&view).error, "recovered");
    }

    #[test]
    fn inbox_recovers_after_a_worker_panics_while_holding_its_queue() {
        let inbox = Inbox::default();
        let poisoned = &inbox.pending;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.lock().unwrap();
            panic!("simulated preview worker panic");
        }));

        inbox.put(PendingFrame {
            metadata: sample_metadata(),
            payload: vec![255, 216, 255, 217],
            payload_start: 0,
            mask_bytes: 0,
            received: Instant::now(),
        });
        assert!(!inbox.pending.is_poisoned());
        assert!(inbox.lock_pending().is_some());
    }

    #[test]
    fn preview_decode_panic_is_returned_as_a_worker_error() {
        let error = catch_worker_panic(|| -> Result<(), String> {
            panic!("simulated JPEG decoder panic");
        })
        .unwrap_err();
        assert!(error.contains("preview decoder recovered from panic"));
        assert!(error.contains("simulated JPEG decoder panic"));
    }

    #[test]
    fn draw_gate_releases_decoder_when_ui_finishes_rendering() {
        let gate = Arc::new(DrawGate::default());
        gate.mark_pending();
        let waiting = Arc::clone(&gate);
        let (finished, done) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            waiting.wait(&AtomicBool::new(false));
            let _ = finished.send(());
        });
        assert!(done.recv_timeout(Duration::from_millis(80)).is_err());
        gate.mark_drawn();
        assert!(done.recv_timeout(Duration::from_millis(500)).is_ok());
        worker.join().unwrap();
    }

    #[test]
    fn draw_gate_recovers_after_its_mutex_is_poisoned() {
        let gate = DrawGate::default();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = gate.pending.lock().unwrap();
            panic!("simulated preview worker panic");
        }));
        assert!(poisoned.is_err());

        gate.mark_pending();
        assert!(!gate.pending.is_poisoned());
        assert!(*gate
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner()));
        gate.mark_drawn();
        assert!(!*gate
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner()));
    }

    #[test]
    fn rejects_bad_preview_headers_before_decode() {
        assert!(parse_frame(vec![]).is_err());
        assert!(parse_frame(((MAX_META + 1) as u32).to_be_bytes().to_vec()).is_err());
        assert!(parse_frame(vec![0, 0, 0, 5, b'{']).is_err());
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

    #[test]
    fn pending_newer_preview_supersedes_an_in_flight_decode() {
        let inbox = Inbox::default();
        let received = Instant::now();
        inbox.put(PendingFrame {
            metadata: sample_metadata(),
            payload: vec![],
            payload_start: 0,
            mask_bytes: 0,
            received: received + Duration::from_millis(1),
        });

        let pending = inbox.lock_pending();
        assert!(newer_frame_waiting(&pending, received));
        assert!(!newer_frame_waiting(
            &pending,
            received + Duration::from_millis(2)
        ));
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
        let pending = parse_frame(packet).unwrap();
        let frame = decode(pending).unwrap();
        assert_eq!(
            frame
                .rgba
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_bytes()
                .len(),
            192 * 192 * 4
        );
        let mut wrong = sample_metadata();
        wrong.source_width = 193;
        assert!(decode(PendingFrame {
            metadata: wrong,
            payload: jpeg.to_vec(),
            payload_start: 0,
            mask_bytes: 0,
            received: Instant::now()
        })
        .is_err());
    }

    #[test]
    fn direct_decode_rejects_invalid_dimensions_before_allocating_preview() {
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        for (width, height) in [
            (0, 192),
            (192, 0),
            (crate::jpeg::MAX_SIDE + 1, 1),
            (crate::jpeg::MAX_SIDE, crate::jpeg::MAX_SIDE),
        ] {
            let mut metadata = sample_metadata();
            metadata.source_width = width;
            metadata.source_height = height;
            assert!(decode(PendingFrame {
                metadata,
                payload: jpeg.to_vec(),
                payload_start: 0,
                mask_bytes: 0,
                received: Instant::now(),
            })
            .is_err());
        }
    }

    #[test]
    fn direct_decode_rejects_bad_jpeg_boundaries_before_allocating_preview() {
        let mut payload = include_bytes!("../../src/assets/red-cube-192.jpg").to_vec();
        payload[0] = 0;
        assert!(decode(PendingFrame {
            metadata: sample_metadata(),
            payload,
            payload_start: 0,
            mask_bytes: 0,
            received: Instant::now(),
        })
        .is_err());
    }

    #[test]
    fn direct_decode_rejects_invalid_mask_before_allocating_preview() {
        let mut metadata = sample_metadata();
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: u32::MAX,
            height: u32::MAX,
            bytes: usize::MAX,
        });
        assert!(decode(PendingFrame {
            metadata,
            payload: include_bytes!("../../src/assets/red-cube-192.jpg").to_vec(),
            payload_start: 0,
            mask_bytes: 0,
            received: Instant::now(),
        })
        .is_err());
    }

    #[test]
    fn direct_decode_rejects_mask_offset_mismatched_with_metadata() {
        let mut metadata = sample_metadata();
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: 1,
            height: 1,
            bytes: 1,
        });
        assert!(decode(PendingFrame {
            metadata,
            payload: vec![255, 216, 255, 217],
            payload_start: 0,
            mask_bytes: 0,
            received: Instant::now(),
        })
        .is_err());
    }

    fn packet(metadata: &LiveFrame, mask: &[u8]) -> Vec<u8> {
        let json = serde_json::to_vec(metadata).unwrap();
        let mut packet = (json.len() as u32).to_be_bytes().to_vec();
        packet.extend(json);
        packet.extend(mask);
        packet.extend(include_bytes!("../../src/assets/red-cube-192.jpg"));
        packet
    }

    #[test]
    fn color_filter_keeps_masked_pixels_and_blacks_out_the_rest() {
        let plain = decode(parse_frame(packet(&sample_metadata(), &[])).unwrap()).unwrap();
        let plain_rgba = plain.rgba.lock().unwrap();
        let plain_rgba = plain_rgba.as_ref().unwrap();
        // Bits set for the left half of every row (pixel i = bit i % 8 of byte i / 8).
        let mut mask = vec![0u8; 192 * 192 / 8];
        for y in 0..192 {
            for x in 0..96 {
                let bit = y * 192 + x;
                mask[bit / 8] |= 1 << (bit % 8);
            }
        }
        let mut metadata = sample_metadata();
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: 192,
            height: 192,
            bytes: mask.len(),
        });
        let filtered = decode(parse_frame(packet(&metadata, &mask)).unwrap()).unwrap();
        let filtered_rgba = filtered.rgba.lock().unwrap();
        let filtered_rgba = filtered_rgba.as_ref().unwrap();
        for y in [0usize, 95, 191] {
            for x in [0usize, 50, 95, 96, 150, 191] {
                let i = (y * 192 + x) * 4;
                if x < 96 {
                    assert_eq!(
                        filtered_rgba.as_bytes()[i..i + 4],
                        plain_rgba.as_bytes()[i..i + 4],
                        "({x},{y}) kept"
                    );
                } else {
                    assert_eq!(
                        filtered_rgba.as_bytes()[i..i + 4],
                        [0, 0, 0, 255],
                        "({x},{y}) black"
                    );
                }
            }
        }
        // A wrong byte count or a mask outside the frame is rejected.
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: 192,
            height: 192,
            bytes: mask.len() - 1,
        });
        assert!(parse_frame(packet(&metadata, &mask[1..])).is_err());
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: 193,
            height: 192,
            bytes: (193usize * 192).div_ceil(8),
        });
        assert!(parse_frame(packet(&metadata, &vec![0; (193usize * 192).div_ceil(8)])).is_err());
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: u32::MAX,
            height: u32::MAX,
            bytes: usize::MAX,
        });
        assert!(parse_frame(packet(&metadata, &[])).is_err());
    }

    #[test]
    fn color_filter_handles_unaligned_crop_rows_and_rejects_short_masks() {
        let mut metadata = sample_metadata();
        metadata.source_width = 5;
        metadata.source_height = 3;
        metadata.crop_x = 1;
        metadata.crop_y = 1;
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: 3,
            height: 2,
            bytes: 1,
        });
        let original = [10, 20, 30, 255];
        let mut rgba = original.repeat(15);
        apply_color_filter(&mut rgba, 5, 3, &metadata, &[0b0001_0101]).unwrap();
        for y in 0..3 {
            for x in 0..5 {
                let index = (y * 5 + x) * 4;
                let kept = matches!((x, y), (1, 1) | (3, 1) | (2, 2));
                assert_eq!(
                    &rgba[index..index + 4],
                    if kept { &original } else { &[0, 0, 0, 255] },
                    "pixel ({x},{y})"
                );
            }
        }
        assert!(apply_color_filter(&mut rgba, 5, 3, &metadata, &[]).is_err());
    }

    #[test]
    fn color_filter_maps_source_mask_to_downscaled_preview_pixels() {
        let mut metadata = sample_metadata();
        metadata.source_width = 4;
        metadata.source_height = 4;
        metadata.crop_x = 1;
        metadata.crop_y = 1;
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: 2,
            height: 2,
            bytes: 1,
        });
        let original = [10, 20, 30, 255];
        let mut rgba = original.repeat(4);
        apply_color_filter(&mut rgba, 2, 2, &metadata, &[0b0000_1111]).unwrap();
        for (index, pixel) in rgba.chunks_exact(4).enumerate() {
            assert_eq!(
                pixel,
                if index == 0 {
                    &original
                } else {
                    &[0, 0, 0, 255]
                }
            );
        }
    }

    #[test]
    fn color_filter_downscale_keeps_fractional_center_sample_mapping() {
        let mut metadata = sample_metadata();
        metadata.source_width = 7;
        metadata.source_height = 5;
        metadata.crop_x = 0;
        metadata.crop_y = 0;
        metadata.color_mask = Some(sysx_ui::protocol::ColorMaskInfo {
            width: 7,
            height: 5,
            bytes: 5,
        });
        let mut mask = [0u8; 5];
        for y in 0..3usize {
            let source_y = (y * 2 + 1) * 5 / (3 * 2);
            for x in 0..4usize {
                let source_x = (x * 2 + 1) * 7 / (4 * 2);
                let bit = source_y * 7 + source_x;
                mask[bit / 8] |= 1 << (bit % 8);
            }
        }
        let original = [10, 20, 30, 255];
        let mut rgba = original.repeat(12);

        apply_color_filter(&mut rgba, 4, 3, &metadata, &mask).unwrap();

        assert!(rgba.chunks_exact(4).all(|pixel| pixel == original));
    }

    #[test]
    fn mailbox_keeps_latest_and_stale_preview_is_hidden() {
        let inbox = Inbox::default();
        let mut metadata = sample_metadata();
        for id in 0..10 {
            metadata.frame_id = id;
            inbox.put(PendingFrame {
                metadata: metadata.clone(),
                payload: vec![255, 216, 255, 217],
                payload_start: 0,
                mask_bytes: 0,
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
            rgba: Mutex::new(Some(SharedPixelBuffer::new(0, 0))),
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
            assert_eq!(
                view.live_frame()
                    .unwrap()
                    .rgba
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .as_bytes()
                    .len(),
                192 * 192 * 4
            );
        }
        drop(workers);
        server.join().unwrap();
    }
}
