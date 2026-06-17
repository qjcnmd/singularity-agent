from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

from miniharness.observability.models import (
    TraceArtifact,
    TraceArtifactKind,
    TraceEvent,
    TraceEventType,
    TraceSeverity,
    TraceSpan,
    TraceStatus,
    TraceSummary,
)


def test_trace_models_round_trip_with_stable_serialization() -> None:
    timestamp = datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC)
    event = TraceEvent(
        event_id="event_1",
        event_type=TraceEventType.COMMAND_COMPLETED,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        parent_event_id=None,
        timestamp=timestamp,
        monotonic_ms=123,
        runtime="command",
        severity=TraceSeverity.INFO,
        summary="Command completed.",
        payload={"exit_code": 0},
        artifact_refs=["artifact_1"],
        policy_decision_id="decision_1",
        approval_grant_id=None,
        sandbox_id="sandbox_1",
        command_id="command_1",
        transaction_id="txn_1",
        verification_id="verification_1",
        span_id="span_1",
        redaction_applied=True,
        payload_hash="hash_1",
    )

    span = TraceSpan(
        span_id="span_1",
        parent_span_id=None,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        name="command.execute",
        runtime="command",
        started_at=timestamp,
        ended_at=timestamp,
        duration_ms=0,
        status=TraceStatus.SUCCESS,
        error_type=None,
        error_message=None,
        attributes={"command_id": "command_1"},
        artifact_refs=["artifact_1"],
    )

    artifact = TraceArtifact(
        artifact_id="artifact_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        kind=TraceArtifactKind.STDOUT,
        path=Path("work/traces/runs/run_1/artifacts/artifact_1.txt"),
        relative_path="artifacts/artifact_1.txt",
        size_bytes=12,
        sha256="abc",
        content_type="text/plain",
        redacted=True,
        sensitive=False,
        summary="stdout",
        metadata={"stream": "stdout"},
    )

    summary = TraceSummary(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        total_events=1,
        total_spans=1,
        total_artifacts=1,
        action_count=1,
        failed_action_count=0,
        command_count=1,
        sandboxed_command_count=1,
        mutation_count=0,
        verification_count=1,
        policy_denial_count=0,
        approval_count=0,
        replan_count=0,
        error_count=0,
        critical_events=[],
        key_artifacts=["artifact_1"],
    )

    assert TraceEvent.from_dict(event.to_dict()) == event
    assert TraceSpan.from_dict(span.to_dict()) == span
    assert TraceArtifact.from_dict(artifact.to_dict()) == artifact
    assert TraceSummary.from_dict(summary.to_dict()) == summary
    assert event.to_json() == TraceEvent.from_json(event.to_json()).to_json()


def test_required_event_types_are_available() -> None:
    required = {
        "task.started",
        "task.completed",
        "task.failed",
        "phase.started",
        "phase.completed",
        "action.proposed",
        "action.started",
        "action.completed",
        "action.failed",
        "planner.replan_triggered",
        "planner.completion_assessed",
        "model.request.created",
        "model.response.received",
        "model.tool_call.proposed",
        "model.output.rejected",
        "tool.validation.started",
        "tool.validation.failed",
        "tool.dispatch.started",
        "tool.dispatch.completed",
        "tool.dispatch.failed",
        "policy.requested",
        "policy.decided",
        "policy.blocked",
        "approval.requested",
        "approval.granted",
        "approval.denied",
        "command.requested",
        "command.started",
        "command.output_chunk",
        "command.completed",
        "command.failed",
        "command.timeout",
        "command.killed",
        "sandbox.requested",
        "sandbox.prepared",
        "sandbox.capability_failed",
        "sandbox.started",
        "sandbox.completed",
        "sandbox.violation",
        "sandbox.cleaned",
        "mutation.proposed",
        "mutation.transaction_started",
        "mutation.applied",
        "mutation.failed",
        "mutation.rollback_started",
        "mutation.rollback_completed",
        "verification.plan_created",
        "verification.check_started",
        "verification.check_completed",
        "verification.failed",
        "verification.evidence_recorded",
        "repair.hint_created",
        "context.snapshot_created",
        "context.compacted",
        "context.observation_added",
        "context.rendered_for_model",
        "final_report.created",
        "final_report.section_added",
        "final_report.completed",
    }

    assert required <= {item.value for item in TraceEventType}
