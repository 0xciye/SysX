import numpy as np
import pytest

from receiver_backend.results import DetectionObject, DetectionResult
from receiver_backend.targeting import (
    FrameCoordinate,
    FractionalDeltaAccumulator,
    PriorTarget,
    TargetCandidate,
    TopmostPixelSeekSelector,
    apply_axis_lock,
    object_detection_candidates,
    object_detection_target,
    select_target,
    validate_pixel_mask,
)


def _result(mode="object_detection", objects=()):
    return DetectionResult(
        frame_id=1, timestamp_ns=1, mode=mode, model_id="model" if mode == "object_detection" else None,
        source_width=100, source_height=80, processed_width=100, processed_height=80,
        objects=tuple(objects), preprocess_ms=0, detection_ms=0, postprocess_ms=0,
        total_processing_ms=0,
    )


def _object(class_id, box, score):
    return DetectionObject(class_id, f"class-{class_id}", box, score, None)


def test_object_detection_gates_class_and_score_and_uses_box_center():
    result = _result(objects=(
        _object(2, (10, 20, 30, 40), 0.8),
        _object(3, (0, 0, 10, 10), 0.99),
        _object(2, (2, 4, 8, 10), 0.4),
    ))

    candidates = object_detection_candidates(result, allowed_class_ids=frozenset({2}), minimum_score=0.5)

    assert candidates == (TargetCandidate(FrameCoordinate(20, 30), (10, 20, 30, 40), 2, 0.8),)


def test_object_detection_score_gate_drops_missing_confidence():
    result = _result(objects=(_object(1, (0, 0, 4, 4), None),))
    assert object_detection_candidates(result, minimum_score=0.1) == ()


def test_object_detection_gates_reject_wrong_mode_and_invalid_score():
    with pytest.raises(ValueError, match="Object Detection result"):
        object_detection_candidates(_result("pixel_seek"))
    with pytest.raises(ValueError, match="between 0 and 1"):
        object_detection_candidates(_result(), minimum_score=1.1)


def test_object_detection_target_uses_nearest_fov_box_and_class_anchor():
    candidates = (
        TargetCandidate(FrameCoordinate(50, 40), (45, 30, 55, 50), 1, 0.9),
        TargetCandidate(FrameCoordinate(64, 40), (60, 30, 68, 50), 2, 0.95),
        TargetCandidate(FrameCoordinate(90, 40), (88, 30, 92, 50), 3, 0.99),
    )

    selected = object_detection_target(candidates, frame_width=100, frame_height=80, fov_degrees=90,
                                       group_a_class_ids=frozenset({1}))

    assert selected is not None
    assert selected.class_id == 1
    assert selected.coordinate == FrameCoordinate(50, 40)


def test_object_detection_target_prefers_fresh_overlapping_same_class_box():
    nearest = TargetCandidate(FrameCoordinate(60, 40), (56, 36, 64, 44), 1, 0.95)
    tracked = TargetCandidate(FrameCoordinate(64, 40), (60, 36, 68, 44), 2, 0.8)
    selected = object_detection_target(
        (nearest, tracked), frame_width=100, frame_height=80, fov_degrees=180,
        previous=PriorTarget(tracked.bbox_xyxy, tracked.class_id), previous_is_fresh=True,
    )
    assert selected is not None and selected.class_id == 2


def test_object_detection_target_handles_zero_radius_by_requiring_box_at_center():
    centered = TargetCandidate(FrameCoordinate(0.5, 0.5), (0, 0, 1, 1), 1, 0.9)
    outside = TargetCandidate(FrameCoordinate(2, 2), (1, 1, 3, 3), 2, 0.9)
    selected = object_detection_target((centered, outside), frame_width=1, frame_height=1, fov_degrees=1)
    assert selected is not None and selected.class_id == centered.class_id
    assert selected.coordinate == FrameCoordinate(0.5, 0.05)


def test_pixel_mask_contract_checks_type_and_dimensions():
    mask = np.zeros((8, 10), dtype=np.uint8)
    validate_pixel_mask(mask, 10, 8)
    with pytest.raises(ValueError, match="uint8 array"):
        validate_pixel_mask(mask.astype(np.float32), 10, 8)
    with pytest.raises(ValueError, match="uint8 array"):
        validate_pixel_mask(mask, 9, 8)


def test_topmost_pixel_selector_ignores_detected_pixels_outside_fov():
    mask = np.zeros((80, 100), dtype=np.uint8)
    mask[0, 50] = 255
    mask[35, 50] = 255
    mask[38, 49] = 255

    selected = TopmostPixelSeekSelector().select(mask, 100, 80, 180)

    assert selected == FrameCoordinate(50.5, 35.5)


def test_select_target_routes_pixel_mask_and_object_detection_candidates():
    seen = {}

    class Pixel:
        def select(self, mask, width, height, fov_degrees):
            seen["pixel"] = (mask.shape, width, height, fov_degrees)
            return FrameCoordinate(4, 5)

    class Object:
        def select(self, candidates, width, height, prior):
            seen["object"] = (candidates, width, height, prior)
            return candidates[0]

    pixel_mask = np.zeros((80, 100), dtype=np.uint8)
    assert select_target(_result("pixel_seek"), pixel_mask=pixel_mask, frame_width=100, frame_height=80,
                         pixel_selector=Pixel(), object_selector=Object(), fov_degrees=90) == FrameCoordinate(4, 5)
    assert seen["pixel"] == ((80, 100), 100, 80, 90)

    detection = _result(objects=(_object(4, (12, 12, 22, 32), 0.9),))
    prior = PriorTarget((9, 9, 19, 29), 4)
    assert select_target(detection, pixel_mask=None, frame_width=100, frame_height=80,
                         pixel_selector=Pixel(), object_selector=Object(), prior=prior,
                         fov_degrees=360) == FrameCoordinate(17, 22)
    assert seen["object"][0][0].class_id == 4
    assert seen["object"][3] == prior


def test_fov_gates_object_candidates_after_detection_and_accounts_for_crop_origin():
    seen = {}

    class Selector:
        def select(self, candidates, width, height, prior):
            seen["candidates"] = candidates
            return candidates[0] if candidates else None

    result = _result(objects=(
        _object(1, (58, 43, 62, 47), 0.9),
        _object(2, (88, 43, 92, 47), 0.95),
    ))
    selected = select_target(
        result, pixel_mask=None, frame_width=100, frame_height=80,
        pixel_selector=object(), object_selector=Selector(), fov_degrees=180,
        crop_x=10, crop_y=5,
    )

    assert selected == FrameCoordinate(60, 45)
    assert [candidate.class_id for candidate in seen["candidates"]] == [1]
    assert len(result.objects) == 2  # Full-frame detections remain available for display.


def test_fractional_delta_carries_fractional_remainder():
    accumulator = FractionalDeltaAccumulator()
    assert accumulator.consume(0.6, -0.6) == (0, 0)
    assert accumulator.consume(0.6, -0.6) == (1, -1)
    assert accumulator.consume(-0.2, 0.2) == (0, 0)


@pytest.mark.parametrize(
    "locks, expected",
    [({}, (3, -2)), ({"lock_x": True}, (0, -2)), ({"lock_y": True}, (3, 0)),
     ({"lock_x": True, "lock_y": True}, (0, 0))],
)
def test_axis_lock_only_zeros_locked_delta(locks, expected):
    assert apply_axis_lock(3, -2, **locks) == expected
