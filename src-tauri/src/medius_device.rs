//! All Medius output uses the vendor Rust SDK, never hand-written wire frames.
use medius::{Button, Device};
use std::time::Duration;

/// How long an Auto click holds the left button, drawn per click. A press
/// released in the same instant can fall inside one mouse report or one game
/// input poll and is then never seen as a click.
const CLICK_HOLD_MS: (u64, u64) = (45, 75);

fn click_hold() -> Duration {
    let mut byte = [0u8; 1];
    let spread = CLICK_HOLD_MS.1 - CLICK_HOLD_MS.0;
    let offset =
        getrandom::fill(&mut byte).map_or(spread / 2, |_| u64::from(byte[0]) * spread / 255);
    Duration::from_millis(CLICK_HOLD_MS.0 + offset)
}

pub fn dispatch(device: &Device, record: &[u8]) -> Result<(), String> {
    if crate::serial_queue::is_medius_left_click(record) {
        device
            .press(Button::LEFT)
            .map_err(|error| error.to_string())?;
        // This runs on the output worker: button telemetry keeps flowing.
        std::thread::sleep(click_hold());
        // Retry once: a failed release would leave the button held.
        device
            .release(Button::LEFT)
            .or_else(|_| device.release(Button::LEFT))
            .map_err(|error| error.to_string())
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
        let held = started.elapsed();
        assert!(
            held >= Duration::from_millis(CLICK_HOLD_MS.0),
            "the button must stay down long enough to be seen: {held:?}"
        );
        for _ in 0..50 {
            let hold = click_hold();
            assert!((CLICK_HOLD_MS.0..=CLICK_HOLD_MS.1).contains(&(hold.as_millis() as u64)));
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
