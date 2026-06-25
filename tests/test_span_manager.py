from __future__ import annotations

import threading

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


def test_span_manager_stack_is_thread_local(tmp_path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    errors: list[BaseException] = []
    barrier = threading.Barrier(4)
    per_thread_top_spans: dict[int, str] = {}
    per_thread_parent_ids: dict[int, str | None] = {}

    def worker(thread_index: int) -> None:
        try:
            barrier.wait()
            top = trace.spans.start_span(
                f"top-{thread_index}",
                component="test",
                ids={"task_id": f"task_{thread_index}"},
            )
            # Each thread should see its own top span as the only stack entry.
            assert len(trace.spans._stack) == 1
            per_thread_top_spans[thread_index] = trace.spans._stack[-1][0]
            with trace.spans.span(
                f"child-{thread_index}",
                component="test",
                ids={"task_id": f"task_{thread_index}"},
            ) as child:
                assert len(trace.spans._stack) == 2
                per_thread_parent_ids[thread_index] = child.parent_span_id
            assert len(trace.spans._stack) == 1
            trace.spans.end_span(top.span_id, status=TraceStatus.SUCCESS)
            assert len(trace.spans._stack) == 0
        except BaseException as exc:  # noqa: BLE001
            errors.append(exc)

    threads = [
        threading.Thread(target=worker, args=(i,)) for i in range(4)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == []
    # Each thread observed its own distinct top span id.
    assert len(set(per_thread_top_spans.values())) == 4
    # Each child's parent is its own thread's top span.
    for thread_index, top_span_id in per_thread_top_spans.items():
        assert per_thread_parent_ids[thread_index] == top_span_id


def test_span_manager_concurrent_spans_do_not_interfere(tmp_path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    errors: list[BaseException] = []
    barrier = threading.Barrier(8)
    iterations = 50

    def worker(thread_index: int) -> None:
        try:
            barrier.wait()
            for _ in range(iterations):
                with trace.spans.span(
                    f"span-{thread_index}",
                    component="test",
                    ids={"task_id": f"task_{thread_index}"},
                ) as span:
                    # Stack must only contain this thread's spans.
                    assert all(
                        entry[0] == span.span_id for entry in trace.spans._stack
                    )
        except BaseException as exc:  # noqa: BLE001
            errors.append(exc)

    threads = [
        threading.Thread(target=worker, args=(i,)) for i in range(8)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == []
    spans = trace.store.latest_spans()
    # 8 threads * 50 iterations = 400 spans, all successful.
    successful = [s for s in spans.values() if s.status == TraceStatus.SUCCESS]
    assert len(successful) == 8 * iterations
