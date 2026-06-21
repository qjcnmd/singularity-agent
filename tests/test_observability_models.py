from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

from singularity.observability.models import (
    TraceArtifact,
    TraceArtifactKind,
    TraceEvent,
    TraceEventType,
    TraceSeverity,
    TraceSpan,
    TraceStatus,
    TraceSummary,
)
from singularity.sandbox.models import SandboxArtifact


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
    artifact_round_trip = TraceArtifact.from_dict(artifact.to_dict())
    assert artifact_round_trip.artifact_id == artifact.artifact_id
    assert artifact_round_trip.relative_path == artifact.relative_path
    assert "path" not in artifact.to_dict()
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
        "user_decision.recorded",
        "clarification.requested",
        "clarification.answered",
        "control_command.received",
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
        "patch.proposed",
        "mutation.transaction_started",
        "mutation.applied",
        "mutation.failed",
        "mutation.rollback_started",
        "mutation.rollback_completed",
        "edit.plan_created",
        "edit.patch_validated",
        "edit.applied",
        "edit.repair_attempted",
        "edit.failed",
        "review.started",
        "review.finding",
        "review.decision",
        "review.completed",
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


def test_trace_artifact_to_dict_uses_opaque_reference_and_keeps_internal_path() -> None:
    artifact = TraceArtifact(
        artifact_id="artifact_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        kind=TraceArtifactKind.STDOUT,
        path=Path("C:/absolute/work/traces/runs/run_1/artifacts/artifact_1.txt"),
        relative_path="artifacts/artifact_1.txt",
        size_bytes=12,
        sha256="abc",
        content_type="text/plain",
        redacted=True,
        sensitive=False,
        summary="stdout",
        metadata={},
    )

    payload = artifact.to_dict()

    assert artifact.path == Path("C:/absolute/work/traces/runs/run_1/artifacts/artifact_1.txt")
    assert "path" not in payload
    assert payload["artifact_ref"] == "artifact_1"
    assert payload["relative_handle"] == "artifacts/artifact_1.txt"
    assert "C:/absolute" not in str(payload)


def test_sandbox_artifact_to_dict_uses_opaque_reference() -> None:
    artifact = SandboxArtifact(
        artifact_id="sandbox_artifact_1",
        sandbox_id="sandbox_1",
        path=Path("C:/absolute/work/sandboxes/sandbox_1/artifacts/stdout.log"),
        relative_path="artifacts/stdout.log",
        size_bytes=7,
        kind="stdout",
        sha256="abc",
    )

    payload = artifact.to_dict()

    assert "path" not in payload
    assert payload["artifact_ref"] == "sandbox_artifact_1"
    assert payload["relative_handle"] == "artifacts/stdout.log"
    assert "C:/absolute" not in str(payload)


def test_trace_artifact_from_dict_accepts_legacy_path_payload() -> None:
    artifact = TraceArtifact.from_dict(
        {
            "artifact_id": "artifact_1",
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "kind": "stdout",
            "path": "C:/absolute/work/traces/runs/run_1/artifacts/artifact_1.txt",
            "relative_path": "artifacts/artifact_1.txt",
            "size_bytes": 12,
            "sha256": "abc",
            "content_type": "text/plain",
            "redacted": True,
            "sensitive": False,
            "summary": "stdout",
            "metadata": {},
        }
    )

    assert artifact.path == Path("C:/absolute/work/traces/runs/run_1/artifacts/artifact_1.txt")
    assert artifact.to_dict()["artifact_ref"] == "artifact_1"
