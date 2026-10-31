//! All Medius output uses the vendor Rust SDK, never hand-written wire frames.
use medius::{Button, Device};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

/// Press-to-release time of one Auto click, drawn per click. A press and
/// release sent back to back can land in the same mouse report, and the game
/// then sees no click at all; a real click lasts tens of milliseconds.
const CLICK_PRESS_MS: (u64, u64) = (25, 40);

fn click_press() -> Duration {
    let mut byte = [0u8; 1];
    let spread = CLICK_PRESS_MS.1 - CLICK_PRESS_MS.0;
    let offset =
        getrandom::fill(&mut byte).map_or(spread / 2, |_| u64::from(byte[0]) * spread / 255);
    Duration::from_millis(CLICK_PRESS_MS.0 + offset)
}

fn release_left(device: &Device) -> Result<(), String> {
    let release = || {
        catch_unwind(AssertUnwindSafe(|| device.release(Button::LEFT)))
            .map_err(|_| "Medius button release panicked".to_owned())?
            .map_err(|error| error.to_string())
    };
    release().or_else(|_| release())
}

pub fn dispatch(device: &Device, record: &[u8]) -> Result<(), String> {
    if let Some(pressed) = crate::serial_queue::decode_medius_left_press(record) {
        if pressed {
            catch_unwind(AssertUnwindSafe(|| device.press(Button::LEFT)))
                .map_err(|_| "Medius button press panicked".to_owned())?
                .map_err(|error| error.to_string())
        } else {
            release_left(device)
        }
    } else if crate::serial_queue::is_medius_left_click(record) {
        let pressed = catch_unwind(AssertUnwindSafe(|| device.press(Button::LEFT)))
            .map_err(|_| "Medius button press panicked".to_owned())
            .and_then(|result| result.map_err(|error| error.to_string()));
        // Runs on the output worker, so button telemetry keeps flowing.
        if pressed.is_ok() {
            std::thread::sleep(click_press());
        }
        let released = release_left(device);
        pressed.and(released)
    } else {
        let (dx, dy) = crate::serial_queue::decode_medius_move(record)
            .ok_or_else(|| "unsupported Medius queue record".to_string())?;
        device.move_rel(dx, dy).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use medius::{FrameType, MockBox};
    #[test]
    fn sdk_receives_moves_and_one_press_release_pair() {
        let mock = MockBox::new();
        let device = Device::with_mock(mock.clone());
        let expected = MockBox::new();
        let reference = Device::with_mock(expected.clone());
        mock.clear_recorded();
        expected.clear_recorded();
        dispatch(&device, &crate::serial_queue::encode_medius_move(-23, 17)).unwrap();
        let started = std::time::Instant::now();
        dispatch(&device, &crate::serial_queue::encode_medius_left_click()).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(CLICK_PRESS_MS.0));
        for _ in 0..50 {
            let press = click_press().as_millis() as u64;
            assert!((CLICK_PRESS_MS.0..=CLICK_PRESS_MS.1).contains(&press));
        }
        reference.move_rel(-23, 17).unwrap();
        reference.press(Button::LEFT).unwrap();
        reference.release(Button::LEFT).unwrap();
        let output = |mock: &MockBox| {
            mock.recorded_frames()
                .into_iter()
                .filter(|f| matches!(f.ty, FrameType::Move | FrameType::Inject))
                .map(|f| (f.ty, f.payload))
                .collect::<Vec<_>>()
        };
        assert_eq!(output(&mock), output(&expected));
        assert_eq!(output(&mock).len(), 3);
        let before = output(&mock);
        assert!(dispatch(&device, b"unsupported").is_err());
        assert_eq!(output(&mock), before);
    }
}
