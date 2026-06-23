from __future__ import annotations

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.runtime import TraceRuntime


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


def test_model_usage_summary_handles_redacted_legacy_token_counts(tmp_path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_1", session_id="session_1")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        runtime="model",
        summary="model response",
        ids={"task_id": "task_1"},
        payload={
            "usage": {
                "input_tokens": 10,
                "output_tokens": "<redacted>",
                "total_tokens": 10,
            }
        },
    )

    report_summary = trace.final_report_summary(task_id="task_1")

    assert report_summary["model_usage_summary"]["responses"] == 1
    assert report_summary["model_usage_summary"]["input_tokens"] == 10
    assert report_summary["model_usage_summary"]["output_tokens"] == 0
    assert report_summary["model_usage_summary"]["total_tokens"] == 10


def test_model_usage_summary_reports_request_and_run_cache_hit_rates(tmp_path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_1", session_id="session_1")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        runtime="model",
        summary="first response",
        ids={"task_id": "task_1"},
        payload={
            "request_id": "req_1",
            "usage": {
                "input_tokens": 100,
                "cached_input_tokens": 25,
            },
        },
    )
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        runtime="model",
        summary="second response",
        ids={"task_id": "task_1"},
        payload={
            "request_id": "req_2",
            "usage": {
                "input_tokens": 100,
                "cached_input_tokens": 75,
            },
        },
    )

    usage = trace.final_report_summary(task_id="task_1")["model_usage_summary"]

    assert usage["cached_input_tokens"] == 100
    assert usage["request_cache_hit_rates"] == {
        "req_1": 0.25,
        "req_2": 0.75,
    }
    assert usage["run_cache_hit_rate"] == 0.5
