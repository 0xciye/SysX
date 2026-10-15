from __future__ import annotations

import os
import tempfile
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import yaml


@dataclass(frozen=True, slots=True)
class ModelDefinition:
    id: str
    name: str
    path: str
    input_width: int
    input_height: int
    confidence: float = 0.5
    iou: float = 0.45
    device: str = "auto"
    enabled: bool = True
    class_names: dict[int, str] | None = None

    def __post_init__(self) -> None:
        if not self.id.strip() or not self.name.strip() or not self.path.strip():
            raise ValueError("model id, name, and path are required")
        if not 32 <= self.input_width <= 8192 or not 32 <= self.input_height <= 8192:
            raise ValueError("model input dimensions must each be between 32 and 8192")
        if not 0 <= self.confidence <= 1 or not 0 <= self.iou <= 1:
            raise ValueError("confidence and IoU thresholds must be between 0 and 1")
        if self.device not in {"auto", "cpu", "mps", "cuda"}:
            raise ValueError("device must be auto, cpu, mps, or cuda")
        if self.class_names is not None:
            labels: dict[int, str] = {}
            for raw_id, raw_name in self.class_names.items():
                class_id = int(raw_id)
                if class_id < 0 or not str(raw_name).strip():
                    raise ValueError("class IDs must be non-negative and class names non-empty")
                labels[class_id] = str(raw_name)
            object.__setattr__(self, "class_names", labels)


class ModelRegistry:
    """Atomic YAML persistence for user-supplied model entries."""

    def __init__(self, path: str | Path):
        self.path = Path(path)
        self.models: dict[str, ModelDefinition] = {}
        self.active_model_id: str | None = None
        if self.path.exists():
            self.load()

    def load(self) -> None:
        data = yaml.safe_load(self.path.read_text(encoding="utf-8")) or {}
        if not isinstance(data, dict) or not isinstance(data.get("models", []), list):
            raise ValueError("models.yaml must contain a models list")
        loaded: dict[str, ModelDefinition] = {}
        for item in data.get("models", []):
            if not isinstance(item, dict):
                raise ValueError("each model entry must be a mapping")
            model = ModelDefinition(**item)
            if model.id in loaded:
                raise ValueError(f"duplicate model id: {model.id}")
            loaded[model.id] = model
        active = data.get("active_model_id")
        if active is not None and active not in loaded:
            raise ValueError("active_model_id does not exist in models list")
        self.models = loaded
        self.active_model_id = active

    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "active_model_id": self.active_model_id,
            "models": [self._serialize(model) for model in self.models.values()],
        }
        fd, tmp_name = tempfile.mkstemp(prefix=f".{self.path.name}.", dir=self.path.parent)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                yaml.safe_dump(payload, stream, sort_keys=False, allow_unicode=True)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(tmp_name, self.path)
        finally:
            if os.path.exists(tmp_name):
                os.unlink(tmp_name)

    def add(self, model: ModelDefinition) -> None:
        if model.id in self.models:
            raise ValueError(f"model id already exists: {model.id}")
        self.models[model.id] = model
        self.save()

    def update(self, model: ModelDefinition) -> None:
        if model.id not in self.models:
            raise KeyError(model.id)
        self.models[model.id] = model
        self.save()

    def remove(self, model_id: str) -> None:
        if model_id not in self.models:
            raise KeyError(model_id)
        del self.models[model_id]
        if self.active_model_id == model_id:
            self.active_model_id = None
        self.save()

    def activate(self, model_id: str | None) -> None:
        if model_id is not None and model_id not in self.models:
            raise KeyError(model_id)
        self.active_model_id = model_id
        self.save()

    @staticmethod
    def _serialize(model: ModelDefinition) -> dict[str, Any]:
        value = asdict(model)
        if model.class_names is not None:
            value["class_names"] = {str(key): label for key, label in model.class_names.items()}
        return value
