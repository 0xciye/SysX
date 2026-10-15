from types import SimpleNamespace

import numpy as np
import pytest

from receiver_backend.config import ModelDefinition, ModelRegistry
from receiver_backend.service import PIXEL_COLOR_PROFILES
from receiver_backend.strategies import ObjectDetectionStrategy, PixelColorRule, PixelSeekStrategy, StrategyFactory, validate_inference_device


def test_model_registry_persists_activation_and_rectangular_resolution(tmp_path):
    path = tmp_path / "models.yaml"
    registry = ModelRegistry(path)
    model = ModelDefinition("rect", "Rectangular", "/user/model.pt", 329, 320)
    registry.add(model)
    registry.activate(model.id)

    loaded = ModelRegistry(path)
    assert loaded.models["rect"].input_width == 329
    assert loaded.models["rect"].input_height == 320
    assert loaded.active_model_id == "rect"


def test_model_registry_normalizes_yaml_class_name_keys(tmp_path):
    registry = ModelRegistry(tmp_path / "models.yaml")
    model = ModelDefinition("custom", "Custom", "/user/model.pt", 64, 32, class_names={91: "Target"})
    registry.add(model)

    loaded = ModelRegistry(tmp_path / "models.yaml")
    assert loaded.models["custom"].class_names == {91: "Target"}


@pytest.mark.parametrize("width,height", [(31, 192), (192, 8193), (0, 320)])
def test_model_resolution_is_validated_per_dimension(width, height):
    with pytest.raises(ValueError, match="dimensions"):
        ModelDefinition("m", "Model", "model.pt", width, height)


@pytest.mark.parametrize("width,height", [(192, 192), (329, 320), (640, 480)])
def test_pixel_seek_accepts_arbitrary_valid_resolutions(width, height):
    frame = np.zeros((height, width, 3), dtype=np.uint8)
    result = PixelSeekStrategy(()).process(frame, frame_id=1)
    assert (result.source_width, result.source_height) == (width, height)
    assert result.objects == ()


def test_pixel_seek_returns_box_and_deterministic_pixel_count():
    frame = np.zeros((80, 120, 3), dtype=np.uint8)
    frame[20:40, 30:55] = (0, 0, 255)  # BGR red
    rule = PixelColorRule(
        class_id=7,
        class_name="Target",
        hsv_ranges=(((0, 200, 200), (10, 255, 255)),),
        min_area=10,
        min_pixels=10,
    )

    result = PixelSeekStrategy((rule,)).process(frame, frame_id=42, timestamp_ns=123)

    assert result.frame_id == 42
    assert result.timestamp_ns == 123
    assert result.objects[0].class_id == 7
    assert result.objects[0].bbox_xyxy == (30.0, 20.0, 55.0, 40.0)
    assert result.objects[0].pixel_count == 500
    assert result.objects[0].confidence is None
    assert result.inference_device == "cpu"


def test_pixel_seek_detects_full_frame_then_selects_topmost_valid_pixel_inside_fov():
    frame = np.zeros((100, 100, 3), dtype=np.uint8)
    frame[0:4, 0:4] = (0, 0, 255)       # Detected but outside the FOV.
    frame[45:49, 50:54] = (0, 0, 255)   # Topmost detected pixels inside the FOV.
    rule = PixelColorRule(1, "Target", (((0, 200, 200), (10, 255, 255)),))

    result = PixelSeekStrategy((rule,)).process(
        frame, frame_id=1, source_width=120, source_height=120,
        crop_x=10, crop_y=10, fov_degrees=180,
    )

    assert len(result.objects) == 2
    assert result.target_xy == (60.5, 55.5)


def test_pixel_seek_keeps_detection_running_when_no_target_is_inside_fov():
    frame = np.zeros((100, 100, 3), dtype=np.uint8)
    frame[0:4, 0:4] = (0, 0, 255)
    rule = PixelColorRule(1, "Target", (((0, 200, 200), (10, 255, 255)),))

    result = PixelSeekStrategy((rule,)).process(frame, frame_id=1, fov_degrees=180)

    assert len(result.objects) == 1
    assert result.target_xy is None


@pytest.mark.parametrize(
    "color,bgr",
    [("red", (0, 0, 255)), ("yellow", (0, 255, 255)), ("purple", (255, 0, 170))],
)
def test_pixel_seek_profiles_detect_rgb_colors_from_decoded_bgr_frames(color, bgr):
    frame = np.zeros((32, 32, 3), dtype=np.uint8)
    frame[8:24, 10:22] = bgr

    result = PixelSeekStrategy(PIXEL_COLOR_PROFILES[color]).process(frame, frame_id=1)

    assert len(result.objects) == 1
    assert result.objects[0].class_name == color.capitalize()
    assert result.objects[0].bbox_xyxy == (10.0, 8.0, 22.0, 24.0)


def test_pixel_seek_profiles_use_requested_hsv_bounds():
    expected = {
        "red": (((0, 50, 180), (10, 255, 255)),),
        "yellow": (((20, 157, 252), (40, 255, 255)),),
        "purple": (((140, 60, 240), (160, 255, 255)),),
    }

    assert {color: tuple(rule.hsv_ranges[0] for rule in rules) for color, rules in PIXEL_COLOR_PROFILES.items()} == expected


def test_pixel_seek_roi_filters_outside_regions_and_keeps_frame_coordinates():
    frame = np.zeros((80, 120, 3), dtype=np.uint8)
    frame[5:15, 5:15] = (0, 0, 255)
    frame[30:40, 35:45] = (0, 0, 255)
    rule = PixelColorRule(
        class_id=1,
        class_name="Target",
        hsv_ranges=(((0, 200, 200), (10, 255, 255)),),
        roi=(0.25, 0.25, 0.75, 0.75),
    )

    result = PixelSeekStrategy((rule,)).process(frame, frame_id=1)

    assert len(result.objects) == 1
    assert result.objects[0].bbox_xyxy == (35.0, 30.0, 45.0, 40.0)


def test_pixel_seek_roi_stays_relative_to_original_frame_after_crop():
    frame = np.zeros((32, 32, 3), dtype=np.uint8)
    frame[4:12, 4:12] = (0, 0, 255)
    rule = PixelColorRule(
        class_id=1,
        class_name="Target",
        hsv_ranges=(((0, 200, 200), (10, 255, 255)),),
        roi=(0.5, 0.25, 0.75, 0.75),
    )

    result = PixelSeekStrategy((rule,)).process(
        frame, frame_id=1, source_width=64, source_height=48, crop_x=16, crop_y=8,
    )

    assert result.objects == ()


@pytest.mark.parametrize("roi", [(-0.1, 0, 0.5, 1), (0, 0, 1, 1.1), (0.7, 0, 0.3, 1), (0, 0, 1, float("nan"))])
def test_pixel_seek_rejects_invalid_roi(roi):
    with pytest.raises(ValueError, match="ROI must be normalized"):
        PixelColorRule(1, "Target", (((0, 0, 0), (179, 255, 255)),), roi=roi)


def test_factory_requires_model_for_object_detection():
    with pytest.raises(ValueError, match="active user model"):
        StrategyFactory.create("object_detection")


def test_mps_is_rejected_on_windows_with_recovery_guidance():
    with pytest.raises(RuntimeError, match="only on macOS.*Automatic or CPU"):
        validate_inference_device("mps", platform_name="win32")


def test_cuda_reports_unavailable_runtime_and_accepts_available_runtime():
    unavailable = SimpleNamespace(cuda=SimpleNamespace(is_available=lambda: False))
    available = SimpleNamespace(cuda=SimpleNamespace(is_available=lambda: True))

    with pytest.raises(RuntimeError, match="cannot access an NVIDIA CUDA device.*Automatic or CPU"):
        validate_inference_device("cuda", torch_module=unavailable, platform_name="win32")
    validate_inference_device("cuda", torch_module=available, platform_name="win32")


def test_mps_reports_unavailable_runtime_on_macos():
    unavailable = SimpleNamespace(backends=SimpleNamespace(mps=SimpleNamespace(is_available=lambda: False)))
    with pytest.raises(RuntimeError, match="reports it unavailable.*Automatic or CPU"):
        validate_inference_device("mps", torch_module=unavailable, platform_name="darwin")


def test_object_detection_preserves_model_class_id_and_custom_name(tmp_path):
    model = ModelDefinition(
        "custom",
        "Custom detector",
        str(tmp_path / "unused.pt"),
        64,
        32,
        class_names={91: "User label"},
    )

    class Boxes:
        xyxy = np.array([[1, 2, 12, 18]])
        cls = np.array([91])
        conf = np.array([0.875])

    class Prediction:
        names = {0: "unrelated"}
        boxes = Boxes()

    predict_options = {}

    class FakeModel:
        predictor = SimpleNamespace(device="cuda:0")

        @staticmethod
        def predict(**kwargs):
            predict_options.update(kwargs)
            return [Prediction()]

    strategy = ObjectDetectionStrategy(model)
    strategy._model = FakeModel()
    result = strategy.process(np.zeros((32, 64, 3), dtype=np.uint8), frame_id=9, timestamp_ns=11,
                              source_width=100, source_height=80, crop_x=18, crop_y=24)

    assert result.objects[0].class_id == 91
    assert result.objects[0].class_name == "User label"
    assert result.objects[0].confidence == pytest.approx(0.875)
    assert result.objects[0].bbox_xyxy == (19.0, 26.0, 30.0, 42.0)
    assert result.model_id == "custom"
    assert (result.source_width, result.source_height) == (100, 80)
    assert (result.processed_width, result.processed_height) == (64, 32)
    assert result.inference_device == "cuda:0"
    assert predict_options["imgsz"] == (32, 64)
    assert predict_options["conf"] == model.confidence
    assert predict_options["iou"] == model.iou
    assert predict_options["agnostic_nms"] is False


def test_object_detection_rejects_udp_resolution_mismatch(tmp_path):
    model = ModelDefinition("m", "Model", str(tmp_path / "missing.pt"), 64, 32)
    strategy = ObjectDetectionStrategy(model)
    with pytest.raises(ValueError, match="expects 64x32"):
        strategy.process(np.zeros((31, 64, 3), dtype=np.uint8), frame_id=1)


def test_object_detection_strategy_publishes_selected_fov_target_with_crop_origin():
    model = ModelDefinition("m", "Model", "/user/model.pt", 64, 64, confidence=0.5)

    class Boxes:
        xyxy = np.array([[28, 28, 36, 36]])
        cls = np.array([2])
        conf = np.array([0.8])

    class Prediction:
        names = {2: "target"}
        boxes = Boxes()

    strategy = ObjectDetectionStrategy(model)
    strategy._model = SimpleNamespace(predict=lambda **_kwargs: [Prediction()], predictor=SimpleNamespace(device="cpu"))
    result = strategy.process(np.zeros((64, 64, 3), dtype=np.uint8), frame_id=7,
                              source_width=100, source_height=90, crop_x=18, crop_y=13, fov_degrees=90)

    assert result.target_xy == pytest.approx((50, 41.4))
    assert result.objects[0].bbox_xyxy == (46, 41, 54, 49)
