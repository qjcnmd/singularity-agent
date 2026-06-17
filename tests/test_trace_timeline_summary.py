from __future__ import annotations

from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.observability.runtime import TraceRuntime


def test_timeline_and_summary_correlate_runtime_events(tmp_path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_1", session_id="session_1")
    trace.emit(
        TraceEventType.ACTION_STARTED,
        runtime="planner",
        summary="Run tests",
        ids={"task_id": "task_1", "action_id": "action_1"},
    )
    trace.emit(
        TraceEventType.POLICY_BLOCKED,
        runtime="policy",
        summary="Package install blocked",
        ids={"task_id": "task_1", "policy_decision_id": "decision_1"},
        severity=TraceSeverity.WARNING,
    )
    trace.emit(
        TraceEventType.APPROVAL_GRANTED,
        runtime="approval",
        summary="User approved once",
        ids={"task_id": "task_1", "approval_grant_id": "grant_1"},
    )
    trace.emit(
        TraceEventType.COMMAND_COMPLETED,
        runtime="command",
        summary="pytest passed",
        ids={"task_id": "task_1", "command_id": "cmd_1", "sandbox_id": "sandbox_1"},
        payload={"exit_code": 0},
    )
    trace.emit(
        TraceEventType.VERIFICATION_CHECK_COMPLETED,
        runtime="verification",
        summary="unit tests passed",
        ids={"task_id": "task_1", "verification_id": "check_1"},
    )
    trace.emit(
        TraceEventType.PLANNER_REPLAN_TRIGGERED,
        runtime="planner",
        summary="Replanned after failure",
        ids={"task_id": "task_1"},
    )

    timeline = trace.timeline(task_id="task_1")
    summary = trace.summarize(task_id="task_1")

    assert [item.event_type for item in timeline] == [
        TraceEventType.ACTION_STARTED.value,
        TraceEventType.POLICY_BLOCKED.value,
        TraceEventType.APPROVAL_GRANTED.value,
        TraceEventType.COMMAND_COMPLETED.value,
        TraceEventType.VERIFICATION_CHECK_COMPLETED.value,
        TraceEventType.PLANNER_REPLAN_TRIGGERED.value,
    ]
    assert any("decision_1" in item.related_ids for item in timeline)
    assert summary.action_count == 1
    assert summary.command_count == 1
    assert summary.sandboxed_command_count == 1
    assert summary.verification_count == 1
    assert summary.policy_denial_count == 1
    assert summary.approval_count == 1
    assert summary.replan_count == 1


def test_final_report_and_context_summary_are_redacted(tmp_path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_1", session_id="session_1")
    artifact = trace.write_artifact(
        kind="stderr",
        text="failure OPENAI_API_KEY=sk-secret",
        task_id="task_1",
        summary="stderr",
    )
    trace.emit(
        TraceEventType.COMMAND_FAILED,
        runtime="command",
        summary="pytest failed",
        ids={"task_id": "task_1", "command_id": "cmd_1"},
        payload={"stderr": "OPENAI_API_KEY=sk-secret"},
        artifact_refs=[artifact.artifact_id],
        severity="error",
    )

    context_lines = trace.context_summary(task_id="task_1")
    report_summary = trace.final_report_summary(task_id="task_1")

    assert context_lines == [
        f"[trace] Command failed: pytest failed, see artifact {artifact.artifact_id}"
    ]
    assert report_summary["key_failures"] == ["pytest failed"]
    assert "sk-secret" not in str(context_lines)
    assert "sk-secret" not in str(report_summary)
