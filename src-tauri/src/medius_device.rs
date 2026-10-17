//! All Medius output uses the vendor Rust SDK, never hand-written wire frames.
use medius::{Button, Device};
pub fn dispatch(device: &Device, record: &[u8]) -> Result<(), String> {
    if crate::serial_queue::is_medius_left_click(record) {
        device
            .press(Button::LEFT)
            .map_err(|error| error.to_string())?;
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
        dispatch(&device, &crate::serial_queue::encode_medius_left_click()).unwrap();
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
