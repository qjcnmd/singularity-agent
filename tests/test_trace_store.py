from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path

from singularity.observability.models import (
    TraceArtifact,
    TraceArtifactKind,
    TraceEvent,
    TraceEventType,
    TraceSeverity,
    TraceSpan,
    TraceStatus,
)
from singularity.observability.store import TraceStore


def _event(
    event_id: str,
    event_type: TraceEventType,
    *,
    task_id: str = "task_1",
    severity: TraceSeverity = TraceSeverity.INFO,
    seconds: int = 0,
) -> TraceEvent:
    return TraceEvent(
        event_id=event_id,
        event_type=event_type,
        run_id="run_1",
        session_id="session_1",
        task_id=task_id,
        phase_id=None,
        action_id=None,
        parent_event_id=None,
        timestamp=datetime(2026, 1, 1, tzinfo=UTC) + timedelta(seconds=seconds),
        monotonic_ms=seconds * 1000,
        runtime="test",
        severity=severity,
        summary=event_type.value,
        payload={},
        artifact_refs=[],
        policy_decision_id=None,
        approval_grant_id=None,
        sandbox_id=None,
        command_id=None,
        transaction_id=None,
        verification_id=None,
        span_id=None,
        redaction_applied=True,
        payload_hash="hash",
    )


def test_trace_store_append_query_timeline_summary_and_recovery(tmp_path: Path) -> None:
    store = TraceStore(tmp_path, run_id="run_1")
    store.append_event(_event("event_2", TraceEventType.COMMAND_COMPLETED, seconds=2))
    store.append_event(
        _event(
            "event_1",
            TraceEventType.ACTION_FAILED,
            severity=TraceSeverity.ERROR,
            seconds=1,
        )
    )
    running = TraceSpan(
        span_id="span_1",
        parent_span_id=None,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id=None,
        action_id=None,
        name="unfinished",
        runtime="test",
        started_at=datetime(2026, 1, 1, tzinfo=UTC),
        ended_at=None,
        duration_ms=None,
        status=TraceStatus.RUNNING,
        error_type=None,
        error_message=None,
        attributes={},
        artifact_refs=[],
    )
    store.append_span(running)
    artifact = TraceArtifact(
        artifact_id="artifact_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        kind=TraceArtifactKind.STDOUT,
        path=tmp_path / "artifact.txt",
        relative_path="artifact.txt",
        size_bytes=1,
        sha256="hash",
        content_type="text/plain",
        redacted=True,
        sensitive=False,
        summary="stdout",
        metadata={},
    )
    store.append_artifact(artifact)

    assert len(store.events_path.read_text(encoding="utf-8").splitlines()) == 2
    assert [event.event_id for event in store.query_events(task_id="task_1")] == [
        "event_2",
        "event_1",
    ]
    assert store.query_events(event_type=TraceEventType.ACTION_FAILED)[0].severity == TraceSeverity.ERROR
    assert [item.event_id for item in store.get_timeline(run_id="run_1")] == ["event_1", "event_2"]

    summary = store.summarize(run_id="run_1")
    assert summary.total_events == 2
    assert summary.total_spans == 1
    assert summary.total_artifacts == 1
    assert summary.failed_action_count == 1
    assert summary.error_count == 1

    recovered = store.recover_incomplete_spans()
    assert recovered == ["span_1"]
    assert store.latest_spans()["span_1"].status == TraceStatus.FAILED
