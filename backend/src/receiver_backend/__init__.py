"""Computer-vision backend for the Receiver desktop application."""

from .config import ModelDefinition, ModelRegistry
from .pipeline import LatestFramePipeline, PipelineStats, ProcessedFrame
from .results import DetectionObject, DetectionResult
from .sources import FrameSource, LocalImageSource, LocalVideoSource, SourceFrame, UdpDecodedFrameSource
from .storage import DetectionDataStore
from .strategies import PixelColorRule, PixelSeekStrategy, StrategyFactory
from .udp import EncodedFrame, UdpFrameReassembler, UdpFrameSource

__all__ = [
    "DetectionObject",
    "DetectionDataStore",
    "DetectionResult",
    "EncodedFrame",
    "FrameSource",
    "LatestFramePipeline",
    "LocalImageSource",
    "LocalVideoSource",
    "ModelDefinition",
    "ModelRegistry",
    "PipelineStats",
    "PixelColorRule",
    "PixelSeekStrategy",
    "ProcessedFrame",
    "SourceFrame",
    "StrategyFactory",
    "UdpDecodedFrameSource",
    "UdpFrameReassembler",
    "UdpFrameSource",
]
