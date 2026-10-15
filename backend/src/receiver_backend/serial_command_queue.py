"""Type-0 async serial command handoff reconstructed from supplied evidence.

The platform writer owns the actual overlapped I/O. This consumer retains each
write buffer until that writer reports completion; it does not open a device.
"""

from __future__ import annotations

from dataclasses import dataclass
from threading import Lock
from typing import Callable, Protocol


@dataclass(frozen=True, slots=True)
class QueuedSerialCommand:
    kind: int
    bytes: bytes
    request_ms: int = 0
    completion_state: object | None = None


@dataclass(frozen=True, slots=True)
class AsyncWriteState:
    operation_id: int
    command: QueuedSerialCommand
    buffer: bytes


class OverlappedSerialWriter(Protocol):
    def submit_write(
        self,
        operation_id: int,
        command: QueuedSerialCommand,
        buffer: bytes,
        completed: Callable[[bool, str | None], None],
    ) -> bool:
        """Return true for accepted/synchronous writes and report completion once."""


class SerialCommandQueueConsumer:
    """Submit one type-0 request and retain its bytes until async completion."""

    def __init__(
        self,
        writer: OverlappedSerialWriter,
        on_complete: Callable[[QueuedSerialCommand, bool, str | None], None] | None = None,
    ) -> None:
        self._writer = writer
        self._on_complete = on_complete or (lambda _command, _ok, _error: None)
        self._lock = Lock()
        self._next_operation_id = 1
        self._in_flight: dict[int, AsyncWriteState] = {}

    @property
    def in_flight_count(self) -> int:
        with self._lock:
            return len(self._in_flight)

    def consume_one(self, pop_front: Callable[[], QueuedSerialCommand | None]) -> bool:
        command = pop_front()
        if command is None or command.kind != 0 or not command.bytes:
            return False

        with self._lock:
            operation_id = self._next_operation_id
            self._next_operation_id += 1
            state = AsyncWriteState(operation_id, command, bytes(command.bytes))
            self._in_flight[operation_id] = state

        def completed(ok: bool, error: str | None = None) -> None:
            with self._lock:
                retained = self._in_flight.pop(operation_id, None)
            if retained is not None:
                self._on_complete(retained.command, ok, error)

        try:
            accepted = self._writer.submit_write(operation_id, command, state.buffer, completed)
        except Exception as error:
            completed(False, str(error))
            return False
        if not accepted:
            completed(False, "serial write submission failed")
        return accepted
