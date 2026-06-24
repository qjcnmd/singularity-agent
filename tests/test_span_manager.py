from __future__ import annotations

import pytest

from singularity.observability.models import TraceStatus
from singularity.observability.recorder import TraceRecorder


def test_span_manager_start_end_and_context_manager_success(tmp_path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")

    with trace.span("outer", component="planner", ids={"task_id": "task_1"}) as outer:
        with trace.span("inner", component="command", ids={"task_id": "task_1"}) as inner:
            assert inner.parent_span_id == outer.span_id

    spans = trace.store.latest_spans()
    assert spans[outer.span_id].status == TraceStatus.SUCCESS
    assert spans[inner.span_id].status == TraceStatus.SUCCESS
    assert spans[inner.span_id].duration_ms is not None
    assert spans[inner.span_id].duration_ms >= 0


def test_span_manager_context_manager_records_failed_status(tmp_path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")

    with pytest.raises(ValueError):
        with trace.span("broken", component="tool", ids={"task_id": "task_1"}):
            raise ValueError("boom")

    span = next(iter(trace.store.latest_spans().values()))
    assert span.status == TraceStatus.FAILED
    assert span.error_type == "ValueError"
    assert span.error_message == "boom"
