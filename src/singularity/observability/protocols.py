from __future__ import annotations

from pathlib import Path
from typing import Any, Protocol

from singularity.observability.models import TraceEventType, TraceSeverity


class TraceRecorderProtocol(Protocol):
    run_id: str

    def record(self, event: str, data: dict[str, Any]) -> Any:
        ...


class TraceEmitterProtocol(TraceRecorderProtocol, Protocol):
    def emit(
        self,
        event_type: TraceEventType | str,
        *,
        component: str,
        summary: str,
        payload: dict[str, Any] | None = None,
        ids: dict[str, Any] | None = None,
        severity: TraceSeverity | str = TraceSeverity.INFO,
        artifact_refs: list[str] | None = None,
        related_refs: dict[str, Any] | None = None,
    ) -> Any:
        ...


class TraceStorageProtocol(TraceRecorderProtocol, Protocol):
    path: Path

