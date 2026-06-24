from __future__ import annotations

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.recorder import TraceRecorder


def test_timeline_and_summary_correlate_interaction_events(tmp_path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    trace.emit(
        TraceEventType.ACTION_STARTED,
        component="planner",
        summary="Run tests",
        ids={"task_id": "task_1", "action_id": "action_1"},
    )
    trace.emit(
        TraceEventType.POLICY_BLOCKED,
        component="policy",
        summary="Package install blocked",
        ids={"task_id": "task_1", "policy_decision_id": "decision_1"},
        severity=TraceSeverity.WARNING,
    )
    trace.emit(
        TraceEventType.APPROVAL_GRANTED,
        component="approval",
        summary="User approved once",
        ids={"task_id": "task_1", "approval_grant_id": "grant_1"},
    )
    trace.emit(
        TraceEventType.COMMAND_COMPLETED,
        component="command",
        summary="pytest passed",
        ids={"task_id": "task_1", "command_id": "cmd_1", "sandbox_id": "sandbox_1"},
        payload={"exit_code": 0},
    )
    trace.emit(
        TraceEventType.VERIFICATION_CHECK_COMPLETED,
        component="verification",
        summary="unit tests passed",
        ids={"task_id": "task_1", "verification_id": "check_1"},
    )
    trace.emit(
        TraceEventType.PLANNER_REPLAN_TRIGGERED,
        component="planner",
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
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    artifact = trace.write_artifact(
        kind="stderr",
        text="failure OPENAI_API_KEY=sk-secret",
        task_id="task_1",
        summary="stderr",
    )
    trace.emit(
        TraceEventType.COMMAND_FAILED,
        component="command",
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
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        component="model",
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
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        component="model",
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
        component="model",
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


def test_model_usage_summary_preserves_cache_attribution_sources(tmp_path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        component="model",
        summary="native cache response",
        ids={"task_id": "task_1"},
        payload={
            "request_id": "req_native",
            "usage": {
                "input_tokens": 100,
                "cached_input_tokens": 40,
            },
            "cache": {
                "cache_miss_reasons": [],
                "cache_attribution": {
                    "source": "provider_native",
                    "confidence": 1.0,
                    "reasons": ["usage.cached_input_tokens"],
                    "evidence": ["usage.cached_input_tokens"],
                },
            },
        },
    )
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        component="model",
        summary="inferred miss response",
        ids={"task_id": "task_1"},
        payload={
            "request_id": "req_inferred",
            "usage": {
                "input_tokens": 100,
                "cached_input_tokens": 0,
            },
            "cache": {
                "cache_miss_reasons": ["context_shape_change"],
                "cache_attribution": {
                    "source": "component_inferred",
                    "confidence": 0.35,
                    "reasons": ["context_shape_change"],
                    "evidence": ["context_shape_hash"],
                },
            },
        },
    )

    usage = trace.final_report_summary(task_id="task_1")["model_usage_summary"]

    assert usage["cache_attribution_sources"] == {
        "req_native": "provider_native",
        "req_inferred": "component_inferred",
    }
    assert usage["cache_attribution_source_counts"] == {
        "provider_native": 1,
        "component_inferred": 1,
        "unknown": 0,
    }
