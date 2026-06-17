from __future__ import annotations

import time
from contextlib import contextmanager
from datetime import UTC, datetime
from typing import Any, Iterator
from uuid import uuid4

from miniharness.observability.models import TraceSpan, TraceStatus
from miniharness.observability.store import TraceStore


class SpanManager:
    def __init__(self, *, store: TraceStore, run_id: str, session_id: str) -> None:
        self.store = store
        self.run_id = run_id
        self.session_id = session_id
        self._stack: list[tuple[str, float]] = []

    def start_span(
        self,
        name: str,
        *,
        runtime: str,
        ids: dict[str, Any] | None = None,
        attributes: dict[str, Any] | None = None,
        parent_span_id: str | None = None,
    ) -> TraceSpan:
        ids = ids or {}
        span = TraceSpan(
            span_id=f"span_{uuid4().hex[:12]}",
            parent_span_id=parent_span_id or (self._stack[-1][0] if self._stack else None),
            run_id=str(ids.get("run_id") or self.run_id),
            session_id=str(ids.get("session_id") or self.session_id),
            task_id=ids.get("task_id"),
            phase_id=ids.get("phase_id"),
            action_id=ids.get("action_id"),
            name=name,
            runtime=runtime,
            started_at=datetime.now(UTC),
            ended_at=None,
            duration_ms=None,
            status=TraceStatus.RUNNING,
            error_type=None,
            error_message=None,
            attributes=attributes or {},
            artifact_refs=list(ids.get("artifact_refs") or []),
        )
        self.store.append_span(span)
        self._stack.append((span.span_id, time.perf_counter()))
        return span

    def end_span(
        self,
        span_id: str,
        *,
        status: TraceStatus | str,
        error: BaseException | None = None,
    ) -> TraceSpan:
        started = None
        for index in range(len(self._stack) - 1, -1, -1):
            if self._stack[index][0] == span_id:
                _, started = self._stack.pop(index)
                break
        duration_ms = None
        if started is not None:
            duration_ms = max(0, int((time.perf_counter() - started) * 1000))
        return self.store.update_span_end(
            span_id,
            status,
            error_type=type(error).__name__ if error else None,
            error_message=str(error) if error else None,
            duration_ms=duration_ms,
        )

    @contextmanager
    def span(
        self,
        name: str,
        *,
        runtime: str,
        ids: dict[str, Any] | None = None,
        attributes: dict[str, Any] | None = None,
        parent_span_id: str | None = None,
    ) -> Iterator[TraceSpan]:
        span = self.start_span(
            name,
            runtime=runtime,
            ids=ids,
            attributes=attributes,
            parent_span_id=parent_span_id,
        )
        try:
            yield span
        except Exception as exc:
            self.end_span(span.span_id, status=TraceStatus.FAILED, error=exc)
            raise
        else:
            self.end_span(span.span_id, status=TraceStatus.SUCCESS)
