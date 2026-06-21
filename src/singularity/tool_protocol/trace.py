from __future__ import annotations

from typing import Any

from singularity.observability.models import TraceSeverity


class ToolProtocolTrace:
    def __init__(self, trace: Any | None) -> None:
        self.trace = trace

    def emit(
        self,
        event: str,
        *,
        summary: str,
        payload: dict[str, Any] | None = None,
        ids: dict[str, Any] | None = None,
        severity: TraceSeverity | str = TraceSeverity.INFO,
    ) -> None:
        if self.trace is None:
            return
        safe_payload = _safe_payload(payload or {})
        if hasattr(self.trace, "emit"):
            self.trace.emit(
                event,
                runtime="tool_protocol",
                summary=summary,
                payload=safe_payload,
                ids=ids or {},
                severity=severity,
            )
            return
        if hasattr(self.trace, "record"):
            self.trace.record(event, {**safe_payload, **(ids or {})})


def _safe_payload(payload: dict[str, Any]) -> dict[str, Any]:
    blocked = {"raw_arguments", "parsed_arguments", "normalized_arguments", "raw_result", "result", "content"}
    return {key: value for key, value in payload.items() if key not in blocked}
