from __future__ import annotations

from datetime import UTC, datetime

from singularity.evaluation.results import CommandEvalResult, EvaluationTaskResult
from singularity.observability.models import TraceEvent, TraceEventType, TraceSeverity
from singularity.sandbox.windows_models import WindowsSandboxDoctorReport
from singularity.tool_protocol.models import ToolCallFailureKind, ToolProtocolResultEnvelope


def test_tool_protocol_result_envelope_type_adapter_preserves_wire_shape() -> None:
    payload = {
        "tool_call_id": "call_roundtrip",
        "tool_name": "read_file",
        "ok": False,
        "status": "error",
        "error_code": "policy_denied",
        "error_kind": "policy_denied",
        "content_preview": "safe preview",
        "content_digest": "digest_roundtrip",
        "raw_result_ref": "raw_ref",
        "artifact_refs": ["artifact_1"],
        "observation_id": "obs_1",
        "policy_decision_id": "policy_1",
        "approval_grant_id": "grant_1",
        "truncated": True,
        "redacted": True,
        "metadata": {"nested": {"kept": True}},
    }

    restored = ToolProtocolResultEnvelope.from_dict({**payload, "ignored": "value"})

    assert restored.error_kind == ToolCallFailureKind.policy_denied
    assert restored.to_dict() == payload


def test_tool_protocol_result_envelope_type_adapter_keeps_legacy_defaults() -> None:
    restored = ToolProtocolResultEnvelope.from_dict(
        {
            "tool_call_id": None,
            "tool_name": None,
            "ok": "",
            "status": "",
            "content_preview": None,
            "content_digest": None,
            "artifact_refs": None,
            "truncated": "",
            "redacted": "",
            "metadata": None,
            "ignored": {"not": "projected"},
        }
    )

    assert restored.to_dict() == {
        "tool_call_id": "",
        "tool_name": "",
        "ok": False,
        "status": "ok",
        "error_code": None,
        "error_kind": None,
        "content_preview": "",
        "content_digest": "",
        "raw_result_ref": None,
        "artifact_refs": [],
        "observation_id": None,
        "policy_decision_id": None,
        "approval_grant_id": None,
        "truncated": False,
        "redacted": False,
        "metadata": {},
    }


def test_evaluation_task_result_serialization_contract_keeps_current_keys() -> None:
    result = EvaluationTaskResult(
        task_id="task_1",
        status="completed",
        tests_passed=True,
        infrastructure_blocked=False,
        prompt_tokens=10,
        cached_tokens=2,
        request_cache_hit_rate=0.2,
        run_cache_hit_rate=0.2,
        tool_calls=1,
        files_changed=["solution.py"],
        duration_seconds=1.5,
        error_summary="",
        workspace="work/evaluations/run/task/workspace",
        trace="work/traces/runs/run_1",
        verification=CommandEvalResult(
            command="pytest",
            exit_code=0,
            duration_seconds=0.25,
        ),
        agent_completed=True,
        evaluation_passed=True,
        evaluation_metrics={"schema_version": "evaluation.metrics/v1"},
    )

    payload = result.to_dict()

    assert list(payload) == [
        "task_id",
        "status",
        "tests_passed",
        "infrastructure_blocked",
        "turn_count",
        "prompt_tokens",
        "cached_tokens",
        "request_cache_hit_rate",
        "run_cache_hit_rate",
        "tool_calls",
        "files_changed",
        "duration_seconds",
        "error_summary",
        "workspace",
        "trace",
        "verification_workspace",
        "patch",
        "checks",
        "verification",
        "agent_completed",
        "evaluation_passed",
        "patch_applicable",
        "allowed_scope_passed",
        "public_verification_passed",
        "hidden_verification_passed",
        "sandbox_enforcement_passed",
        "evaluator_visibility_audit_passed",
        "local_process_fallback_count",
        "repair_attempt_count",
        "repair_execution_count",
        "miscompletion_count",
        "blocked_reason",
        "failure_category",
        "request_cache_hit_rates",
        "verification_result",
        "contract_satisfaction",
        "final_report_status",
        "policy_blocks",
        "token_usage",
        "cache_usage",
        "trace_artifact_refs",
        "reproducible_environment",
        "capability_summary",
        "capability_sla",
        "timing",
        "baseline_failed",
        "baseline_checks",
        "patch_applied",
        "fail_to_pass_satisfied",
        "verification_misconfiguration_reason",
        "evaluation_metrics",
    ]
    assert payload["verification"] == {
        "command": "pytest",
        "exit_code": 0,
        "duration_seconds": 0.25,
        "timed_out": False,
        "passed": True,
        "error_summary": "",
        "raw_command": "pytest",
        "resolved_argv": [],
        "interpreter_strategy": {},
        "failure_category": "",
    }


def test_windows_doctor_serialization_contract_keeps_schema_v2_shape() -> None:
    payload = WindowsSandboxDoctorReport.ready_for_tests().to_dict()

    assert payload["schema_version"] == "sandbox.windows.doctor/v2"
    assert payload["available"] is True
    assert payload["blocking_requirements"] == []
    assert payload["missing_requirements"] == []
    assert set(payload["primitives"]) == {
        "restricted_token",
        "job_object",
        "low_integrity",
        "acl",
        "firewall",
        "private_desktop",
    }
    assert set(payload["setup"]) == {
        "sandbox_accounts",
        "login_ui_visibility",
        "logon_rights",
        "group_membership",
        "state_dir_acl",
        "acl_boundary",
        "offline_network_filter",
        "private_desktop",
        "execution_backends",
        "legacy_assets",
    }
    assert set(payload["execution"]) == {
        "account_sids",
        "credentials",
        "launchers",
        "runner_smoke",
        "network_probe",
    }


def test_trace_event_serialization_contract_round_trips_current_json_shape() -> None:
    event = TraceEvent(
        event_id="event_1",
        event_type=TraceEventType.TOOL_PROTOCOL_RESULT_BOUND,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        parent_event_id=None,
        timestamp=datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC),
        monotonic_ms=123,
        component="tool_protocol",
        severity=TraceSeverity.INFO,
        summary="Result bound.",
        payload={"tool_call_id": "call_1"},
        artifact_refs=["artifact_1"],
        redaction_applied=True,
        payload_hash="hash_1",
    )

    payload = event.to_dict()

    assert payload == TraceEvent.from_json(event.to_json()).to_dict()
    assert payload["event_type"] == "tool_protocol.result_bound"
    assert payload["timestamp"] == "2026-01-02T03:04:05+00:00"
    assert "path" not in payload
