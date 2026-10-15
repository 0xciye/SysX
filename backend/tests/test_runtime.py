from __future__ import annotations

import json
import threading
import time

import cv2
import numpy as np
import pytest

from receiver_backend.pipeline import LatestFramePipeline
from receiver_backend.sources import LocalImageSource, LocalVideoSource, SourceFrame, decode_udp_frame
from receiver_backend.storage import DetectionDataStore
from receiver_backend.udp import EncodedFrame


def test_local_image_source_reads_once_and_reports_dimensions(tmp_path):
    path = tmp_path / "frame.png"
    image = np.zeros((13, 27, 3), dtype=np.uint8)
    assert cv2.imwrite(str(path), image)
    source = LocalImageSource(path)

    frame = source.read()
    assert frame is not None
    assert (frame.width, frame.height) == (27, 13)
    assert source.read() is None
    source.close()


def test_local_image_source_reports_invalid_file(tmp_path):
    with pytest.raises(ValueError, match="unable to decode"):
        LocalImageSource(tmp_path / "missing.png").read()


def test_udp_decode_validates_declared_dimensions():
    image = np.zeros((17, 23, 3), dtype=np.uint8)
    ok, data = cv2.imencode(".jpg", image)
    assert ok
    result = decode_udp_frame(EncodedFrame(2, 23, 17, 9, data.tobytes()))
    assert (result.width, result.height, result.frame_id) == (23, 17, 2)
    with pytest.raises(ValueError, match="declares 24x17"):
        decode_udp_frame(EncodedFrame(2, 24, 17, 9, data.tobytes()))


def test_udp_decode_rejects_non_jpeg_and_truncated_jpeg_frames():
    image = np.zeros((17, 23, 3), dtype=np.uint8)
    ok, png = cv2.imencode(".png", image)
    assert ok
    with pytest.raises(ValueError, match="not a complete JPEG frame"):
        decode_udp_frame(EncodedFrame(3, 23, 17, 10, png.tobytes()))

    ok, jpeg = cv2.imencode(".jpg", image)
    assert ok
    with pytest.raises(ValueError, match="not a complete JPEG frame"):
        decode_udp_frame(EncodedFrame(4, 23, 17, 11, jpeg.tobytes()[:-2]))


def test_pipeline_crops_center_and_preserves_original_detection_coordinates():
    from receiver_backend.strategies import PixelColorRule

    rule = PixelColorRule(1, "Target", (((0, 200, 200), (10, 255, 255)),))
    pipeline = LatestFramePipeline(crop_resolution=32, pixel_rules=(rule,))
    image = np.zeros((48, 64, 3), dtype=np.uint8)
    image[20:28, 24:32] = (0, 0, 255)
    try:
        pipeline.submit(SourceFrame(7, 10, image, "test"))
        deadline = time.monotonic() + 2
        while pipeline.latest is None and time.monotonic() < deadline:
            time.sleep(0.005)
        latest = pipeline.latest
        assert latest is not None
        assert (latest.crop_x, latest.crop_y) == (16, 8)
        assert latest.frame.image.shape[:2] == (32, 32)
        assert (latest.result.source_width, latest.result.source_height) == (64, 48)
        assert (latest.result.processed_width, latest.result.processed_height) == (32, 32)
        assert latest.result.objects[0].bbox_xyxy == (24.0, 20.0, 32.0, 28.0)
    finally:
        assert pipeline.close()


def test_pipeline_rejects_crop_larger_than_source_frame():
    pipeline = LatestFramePipeline(crop_resolution=65)
    try:
        pipeline.submit(SourceFrame(1, 1, np.zeros((48, 64, 3), dtype=np.uint8), "test"))
        deadline = time.monotonic() + 2
        while pipeline.stats().rejected_frames == 0 and time.monotonic() < deadline:
            time.sleep(0.005)
        assert pipeline.stats().resolution_mismatches == 1
        assert "crop resolution 65x65 does not fit" in (pipeline.stats().last_error or "")
    finally:
        assert pipeline.close()


def test_latest_frame_pipeline_processes_and_measures_without_fake_values():
    pipeline = LatestFramePipeline()
    try:
        image = np.zeros((64, 96, 3), dtype=np.uint8)
        pipeline.submit(SourceFrame(10, 100, image, "test"))
        deadline = time.monotonic() + 2
        while pipeline.latest is None and time.monotonic() < deadline:
            time.sleep(0.005)
        latest = pipeline.latest
        assert latest is not None
        assert latest.result.frame_id == 10
        assert latest.result.mode == "pixel_seek"
        assert latest.result.source_width == 96
        assert pipeline.stats().processed_frames == 1
    finally:
        assert pipeline.close()


def test_pipeline_processing_fps_uses_only_the_recent_one_second_window():
    pipeline = LatestFramePipeline()
    try:
        now = time.perf_counter_ns()
        pipeline._completed_times.extend((now - 2_000_000_000, now - 900_000_000,
                                          now - 500_000_000, now - 100_000_000))

        stats = pipeline.stats()

        assert stats.processing_fps == pytest.approx(2 / 0.8)
        assert len(pipeline._completed_times) == 3
    finally:
        assert pipeline.close()


def test_pipeline_processing_fps_is_not_capped_at_360_completions_per_second():
    pipeline = LatestFramePipeline()
    try:
        now = time.perf_counter_ns()
        pipeline._completed_times.extend(now - 999_000_000 + index * 1_666_667 for index in range(600))

        stats = pipeline.stats()

        assert stats.processing_fps == pytest.approx(599 / (998_333_333 / 1_000_000_000), rel=0.01)
        assert len(pipeline._completed_times) == 600
    finally:
        assert pipeline.close()


def test_detection_store_persists_jsonl_and_exports_csv(tmp_path):
    pipeline = LatestFramePipeline()
    result_path = tmp_path / "session"
    store = DetectionDataStore(result_path)
    try:
        pipeline.submit(SourceFrame(5, 456, np.zeros((10, 12, 3), dtype=np.uint8), "test"))
        deadline = time.monotonic() + 2
        while pipeline.latest is None and time.monotonic() < deadline:
            time.sleep(0.005)
        assert pipeline.latest is not None
        assert store.submit(pipeline.latest.result)
    finally:
        assert pipeline.close()
        store.close(timeout=2)

    row = json.loads((result_path / "detections.jsonl").read_text().splitlines()[0])
    assert row["frame_id"] == 5
    assert row["inference_device"] == "cpu"
    csv_path = tmp_path / "export.csv"
    store.export_csv(csv_path)
    assert csv_path.read_text().startswith("frame_id,timestamp_ns,mode,model_id,inference_device")


def test_local_video_source_reports_missing_video(tmp_path):
    source = LocalVideoSource(tmp_path / "missing.mp4")
    with pytest.raises(ValueError, match="unable to open video"):
        source.read()
    source.close()


def test_local_video_source_decodes_real_frames_when_codec_is_available(tmp_path):
    path = tmp_path / "sample.avi"
    writer = cv2.VideoWriter(str(path), cv2.VideoWriter_fourcc(*"MJPG"), 10, (24, 18))
    if not writer.isOpened():
        pytest.skip("MJPG encoder is not available in this OpenCV build")
    writer.write(np.full((18, 24, 3), 70, dtype=np.uint8))
    writer.release()
    source = LocalVideoSource(path)
    try:
        frame = source.read()
        assert frame is not None
        assert (frame.width, frame.height) == (24, 18)
        assert source.read() is None
    finally:
        source.close()


def test_pipeline_keeps_one_pending_frame_and_discards_obsolete_strategy_result():
    entered = threading.Event()
    release = threading.Event()
    pipeline = LatestFramePipeline()
    base = pipeline._strategy

    class SlowStrategy:
        def process(self, image, frame_id, timestamp_ns, **kwargs):
            entered.set()
            assert release.wait(2)
            return base.process(image, frame_id, timestamp_ns, **kwargs)

    pipeline._strategy = SlowStrategy()
    image = np.zeros((8, 8, 3), dtype=np.uint8)
    try:
        pipeline.submit(SourceFrame(1, 1, image, "test"))
        assert entered.wait(2)
        pipeline.submit(SourceFrame(2, 2, image, "test"))
        pipeline.submit(SourceFrame(3, 3, image, "test"))
        assert pipeline.stats().dropped_input_frames == 1
        pipeline.configure(strategy="pixel_seek")
        release.set()
        deadline = time.monotonic() + 2
        while pipeline.stats().stale_results == 0 and time.monotonic() < deadline:
            time.sleep(0.005)
        assert pipeline.stats().stale_results == 1
        pipeline.submit(SourceFrame(4, 4, image, "test"))
        deadline = time.monotonic() + 2
        while (pipeline.latest is None or pipeline.latest.frame.frame_id != 4) and time.monotonic() < deadline:
            time.sleep(0.005)
        assert pipeline.latest is not None
        assert pipeline.latest.frame.frame_id == 4
    finally:
        release.set()
        assert pipeline.close()
