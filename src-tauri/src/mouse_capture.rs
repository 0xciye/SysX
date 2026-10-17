//! Physical-only sampler. It owns a separate SDK subscription; the button and
//! injection monitor never drains a 1 kHz motion queue or writes sample files.
use crate::motion_capture::{CaptureRequest, CapturedProfile, PhysicalCapture};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Bridge {
    request: Option<CaptureRequest>,
    complete: Option<CapturedProfile>,
    count: usize,
    status: &'static str,
    dirty: bool,
    progress_at: Option<Instant>,
    retry_at: Option<Instant>,
}
impl Bridge {
    pub fn observe_state(&mut self, message: &Value) {
        let state = if message["event"] == "state" {
            Some(message)
        } else {
            message.get("state")
        };
        if let Some(state) = state {
            let next =
                serde_json::from_value::<Option<CaptureRequest>>(state["motion_capture"].clone())
                    .unwrap_or(None);
            self.configure(next);
        }
    }
    pub fn configure(&mut self, request: Option<CaptureRequest>) {
        if self.request != request {
            *self = Self {
                request,
                dirty: true,
                ..Self::default()
            };
        }
    }
    fn progress(&mut self, id: &str, count: usize, status: &'static str) {
        if self
            .request
            .as_ref()
            .is_some_and(|request| request.id == id)
            && (self.count != count || self.status != status)
        {
            self.count = count;
            self.status = status;
            self.dirty = true;
        }
    }
    fn completed(&mut self, result: CapturedProfile) {
        if self
            .request
            .as_ref()
            .is_some_and(|request| request.id == result.capture_id)
        {
            self.progress(&result.capture_id, 256, "saving");
            self.complete = Some(result);
            self.retry_at = None;
        }
    }
    pub fn commands(&mut self, now: Instant) -> Vec<Value> {
        let Some(request) = &self.request else {
            return Vec::new();
        };
        let mut commands = Vec::new();
        if self.dirty
            && self.progress_at.is_none_or(|last| {
                now.saturating_duration_since(last) >= Duration::from_millis(100)
            })
        {
            commands.push(json!({"version":1,"type":"motion_capture_progress",
                "capture_id":request.id,"count":self.count,"status":self.status}));
            self.dirty = false;
            self.progress_at = Some(now);
        }
        if let Some(complete) = &self.complete {
            if self.retry_at.is_none_or(|deadline| now >= deadline) {
                commands.push(json!({"version":1,"type":"complete_motion_capture",
                    "request_id":format!("capture:{}", request.id),"capture":complete}));
                self.retry_at = Some(now + Duration::from_secs(2));
            }
        }
        commands
    }
}

pub struct Worker {
    stop: Arc<AtomicBool>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
impl Worker {
    pub fn start(device: medius::Device, bridge: Arc<Mutex<Bridge>>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        thread::Builder::new()
            .name("sysx-physical-profile".into())
            .spawn(move || {
                let mut active: Option<CaptureRequest> = None;
                let mut stream: Option<medius::InputStream> = None;
                let mut capture = PhysicalCapture::default();
                let mut finished = false;
                while !stopping.load(Ordering::Relaxed) {
                    let request = bridge.lock().ok().and_then(|state| state.request.clone());
                    if request != active {
                        stream = None;
                        capture = PhysicalCapture::default();
                        finished = false;
                        active = request;
                        if let Some(request) = &active {
                            match device.input_events([
                                medius::CatchFilter::watch_axis(medius::Axis::X),
                                medius::CatchFilter::watch_axis(medius::Axis::Y),
                            ]) {
                                Ok(events) => stream = Some(events),
                                Err(error) => {
                                    crate::diagnostics::record(&format!(
                                        "Physical profile subscription failed: {error}"
                                    ));
                                    if let Ok(mut state) = bridge.lock() {
                                        state.progress(&request.id, 0, "sdk_error");
                                    }
                                    finished = true;
                                }
                            }
                        }
                    }
                    let Some(request) = &active else {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    if finished {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    let Some(events) = stream.as_ref() else {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    let event = events.recv_timeout(Duration::from_millis(10));
                    if !events.is_connected() {
                        if let Ok(mut state) = bridge.lock() {
                            state.progress(&request.id, 0, "sdk_error");
                        }
                        break;
                    }
                    if let Some(medius::InputEvent {
                        ts_us,
                        clock,
                        input: medius::Input::Motion { dx, dy, .. },
                    }) = event
                    {
                        let before = capture.count();
                        let ready = capture.push(
                            ts_us,
                            [dx, dy],
                            clock == medius::ClockDomain::HostChip,
                            events.dropped(),
                        );
                        if let Ok(mut state) = bridge.lock() {
                            state.progress(
                                &request.id,
                                capture.count(),
                                if before > 0 && capture.count() <= 1 {
                                    "restart"
                                } else {
                                    "collecting"
                                },
                            );
                            if ready {
                                let result =
                                    std::mem::take(&mut capture).finish(request.id.clone());
                                if result.validate().is_ok() {
                                    state.completed(result);
                                    finished = true;
                                }
                            }
                        }
                        if finished {
                            stream = None;
                        }
                    }
                }
                // Dropping the subscription/device clone is worker-owned as well.
            })
            .map_err(|error| error.to_string())?;
        Ok(Self { stop })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_physical_reports_are_captured_without_flooding_the_button_queue() {
        use medius::{Button, CatchFilter, Device, Input, MockBox};
        let mock = MockBox::new();
        let device = Device::with_mock(mock.clone());
        let buttons = device
            .input_events([CatchFilter::watch(Button::RIGHT)])
            .unwrap();
        let physical = device
            .input_events([
                CatchFilter::watch_axis(medius::Axis::X),
                CatchFilter::watch_axis(medius::Axis::Y),
            ])
            .unwrap();
        let mut capture = PhysicalCapture::default();
        for i in 0..256u32 {
            // Synthetic hardware frames in MockBox ONLY; no real device or OS hook.
            mock.push_motion(i as u8, 1000 + i * 1000, 1, -2, 0, 0);
            let event = physical.recv_timeout(Duration::from_secs(1)).unwrap();
            let Input::Motion { dx, dy, .. } = event.input else {
                panic!("expected physical motion");
            };
            assert_eq!(
                capture.push(
                    event.ts_us,
                    [dx, dy],
                    event.clock == medius::ClockDomain::HostChip,
                    physical.dropped()
                ),
                i == 255
            );
        }
        capture.finish("mock".into()).validate().unwrap();
        assert!(buttons.try_recv().is_none());
        assert_eq!(buttons.dropped(), 0);
        assert!(!mock.recorded_frames().iter().any(|frame| matches!(
            frame.ty,
            medius::FrameType::Move | medius::FrameType::Inject
        )));
    }

    #[test]
    fn cancelled_or_changed_requests_cannot_publish_samples_and_saves_retry_slowly() {
        let mut bridge = Bridge::default();
        bridge.configure(Some(CaptureRequest { id: "old".into() }));
        let sample = CapturedProfile {
            capture_id: "old".into(),
            reports: vec![[1, 2]; 256],
            timestamps_us: (0..256).map(|i| i * 1000).collect(),
        };
        bridge.completed(sample.clone());
        let now = Instant::now();
        assert!(bridge
            .commands(now)
            .iter()
            .any(|v| v["type"] == "complete_motion_capture"));
        assert!(bridge.commands(now + Duration::from_millis(10)).is_empty());
        assert!(bridge
            .commands(now + Duration::from_secs(2))
            .iter()
            .any(|v| v["type"] == "complete_motion_capture"));
        bridge.configure(Some(CaptureRequest { id: "new".into() }));
        bridge.completed(sample);
        assert!(bridge.complete.is_none());
        bridge.configure(None);
        assert!(bridge.commands(now).is_empty());
    }
    #[test]
    fn backend_state_and_success_ack_stop_collection_without_a_video_frame() {
        let mut bridge = Bridge::default();
        bridge.observe_state(&json!({"event":"state","motion_capture":{"id":"session:1"}}));
        assert_eq!(bridge.request.as_ref().unwrap().id, "session:1");
        bridge.observe_state(&json!({"event":"ack","type":"complete_motion_capture",
            "state":{"motion_capture":null}}));
        assert!(bridge.request.is_none());
    }
}
