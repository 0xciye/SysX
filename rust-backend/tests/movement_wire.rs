// Exercise the actual serializer and host parser together, without device I/O.
#[allow(dead_code)]
#[path = "../../src-tauri/src/movement.rs"]
mod host_movement;

#[allow(dead_code)]
#[path = "../../src-tauri/src/auto.rs"]
mod host_auto;

use sysx_backend::{config::RecoilSettings, movement::MovementEvent};

#[test]
fn backend_wire_reaches_the_host_for_held_trigger_and_always_on() {
    for always_on in [false, true] {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let event = MovementEvent {
            config_revision: 3,
            event: "movement_target".into(),
            frame_id: 7,
            target_valid: true,
            dx: 12,
            dy: -7,
            motion_enabled: true,
            motion_always_on: always_on,
            trigger_button: "right".into(),
            timestamp_ms,
            recoil: RecoilSettings {
                recoil_enabled: true,
                ..RecoilSettings::default()
            },
        };
        let wire = serde_json::to_string(&event).unwrap();
        let value = serde_json::from_str(&wire).unwrap();
        let state = host_movement::MovementState::default();
        host_movement::set_target(&state, &value);
        let parsed = host_movement::take_target(&state).expect("fresh backend target");
        assert_eq!(parsed.config_revision, 3);
        assert!(parsed.target_valid && parsed.motion_enabled);
        assert_eq!(parsed.motion_always_on, always_on);
        assert_eq!(parsed.timestamp_ms, timestamp_ms);
        assert_eq!(parsed.trigger_button, "right");
        assert!(parsed.recoil.recoil_enabled);
        assert_eq!(parsed.device_move(0).0, (12, -7));
        assert!(host_movement::should_apply_target(&parsed, true));
        assert_eq!(
            host_movement::should_apply_target(&parsed, false),
            always_on
        );

        let mut legacy = serde_json::to_value(&event).unwrap();
        let object = legacy.as_object_mut().unwrap();
        for (snake, camel) in [
            ("target_valid", "targetValid"),
            ("motion_enabled", "motionEnabled"),
            ("motion_always_on", "motionAlwaysOn"),
            ("trigger_button", "triggerButton"),
            ("timestamp_ms", "timestampMs"),
        ] {
            let value = object.remove(snake).unwrap();
            object.insert(camel.into(), value);
        }
        let parsed: host_movement::MovementTarget = serde_json::from_value(legacy).unwrap();
        assert_eq!((parsed.dx, parsed.dy), (12, -7));
        assert!(parsed.target_valid && parsed.motion_enabled);
        assert_eq!(parsed.timestamp_ms, timestamp_ms);
    }
}

#[test]
fn malformed_movement_contract_is_rejected_instead_of_defaulted() {
    assert!(serde_json::from_value::<host_movement::MovementTarget>(
        serde_json::json!({"dx":12,"dy":-7})
    )
    .is_err());
}

#[test]
fn host_pending_queue_keeps_only_the_current_revision() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let state = host_movement::MovementState::default();
    for (revision, dx) in [(1, 12), (2, 3)] {
        let event = MovementEvent {
            config_revision: revision,
            event: "movement_target".into(),
            frame_id: revision,
            target_valid: true,
            dx,
            dy: 0,
            motion_enabled: true,
            motion_always_on: false,
            trigger_button: "right".into(),
            timestamp_ms: now,
            recoil: RecoilSettings::default(),
        };
        host_movement::set_target(&state, &serde_json::to_value(event).unwrap());
    }
    let current = host_movement::take_target(&state).unwrap();
    assert_eq!(current.config_revision, 2);
    assert_eq!(current.dx, 3);
}

#[test]
fn auto_controls_cancel_pending_clicks_and_gate_motion_before_another_frame() {
    let now = std::time::Instant::now();
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let active: host_auto::AutoTargetEvent = serde_json::from_value(serde_json::json!({
        "enabled":true,"config_revision":1,"target_valid":true,
        "delay_ms":50,"timestamp_ms":timestamp_ms
    }))
    .unwrap();
    let mut state = host_auto::AutoState::new();
    assert!(active.blocks_motion(true));
    assert!(!active.blocks_motion(false));
    assert_eq!(active.action(&mut state, true, now), None);
    let lost: host_auto::AutoTargetEvent = serde_json::from_value(serde_json::json!({
        "enabled":true,"config_revision":1,"target_valid":false,
        "delay_ms":50,"timestamp_ms":timestamp_ms
    }))
    .unwrap();
    assert!(lost.blocks_motion(true));
    assert_eq!(
        lost.action(&mut state, true, now + std::time::Duration::from_millis(10)),
        None
    );
    let disabled: host_auto::AutoTargetEvent = serde_json::from_value(serde_json::json!({
        "enabled":false,"config_revision":2,"delay_ms":50
    }))
    .unwrap();
    assert!(!disabled.blocks_motion(true));
    assert_eq!(disabled.config_revision(), 2);
    assert_eq!(
        disabled.action(&mut state, true, now + std::time::Duration::from_millis(60)),
        None
    );
}
