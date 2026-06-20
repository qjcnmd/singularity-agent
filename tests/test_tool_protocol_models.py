from __future__ import annotations

from miniharness.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolCallPhase,
    ToolExecutionMode,
    ToolExecutionPlan,
    ToolProtocolRecoveryReport,
    ToolProtocolTurnResult,
    ToolProtocolTurnStatus,
    ToolProtocolValidationResult,
    envelope_from_tool_result,
)
from miniharness.tools.models import ToolResult


def test_tool_protocol_envelope_round_trips_and_derives_digest() -> None:
    envelope = ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="msg_1",
        tool_call_id="call_1",
        tool_name="read_file",
        raw_arguments='{"path":"README.md"}',
        parsed_arguments={"path": "README.md"},
        normalized_arguments={"path": "README.md"},
    )

    restored = ToolCallEnvelope.from_dict(envelope.to_dict())

    assert restored.tool_call_id == "call_1"
    assert restored.phase == ToolCallPhase.PROPOSED
    assert restored.argument_digest


def test_tool_protocol_batch_and_plan_round_trip() -> None:
    envelope = ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="msg_1",
        tool_call_id="call_1",
        tool_name="read_file",
        raw_arguments="{}",
        parsed_arguments={},
        normalized_arguments={},
    )
    batch = ToolCallBatch(
        batch_id="batch_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={"role": "assistant"},
        tool_calls=[envelope],
    )
    plan = ToolExecutionPlan(
        plan_id="plan_1",
        batch_id="batch_1",
        execution_mode=ToolExecutionMode.PARALLEL_READONLY,
        ordered_calls=[envelope],
        parallel_groups=[[envelope]],
        blocked_calls=[],
        reasons=["readonly"],
    )

    assert ToolCallBatch.from_dict(batch.to_dict()).batch_id == "batch_1"
    assert ToolExecutionPlan.from_dict(plan.to_dict()).execution_mode == ToolExecutionMode.PARALLEL_READONLY


def test_tool_protocol_result_envelope_builder_captures_ref_and_redaction_flags() -> None:
    tool_call = ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="msg_1",
        tool_call_id="call_1",
        tool_name="read_file",
        raw_arguments="{}",
        parsed_arguments={},
        normalized_arguments={},
    )
    result = ToolResult.success(content={"text": "secret"}, metadata={"artifact_refs": ["artifact_1"]})

    envelope = envelope_from_tool_result(
        tool_call=tool_call,
        result=result,
        status="ok",
        content_preview="[redacted]",
        content_digest="digest_1",
        raw_result_ref="raw_1",
        observation_id="obs_1",
        redacted=True,
        truncated=True,
        error_kind=ToolCallFailureKind.protocol_violation,
    )

    assert envelope.ok is True
    assert envelope.raw_result_ref == "raw_1"
    assert envelope.redacted is True
    assert envelope.truncated is True
    assert envelope.artifact_refs == ["artifact_1"]


def test_protocol_status_and_report_models_are_serializable() -> None:
    turn_result = ToolProtocolTurnResult(
        status=ToolProtocolTurnStatus.PROCESSED,
        batch_id="batch_1",
        executed_count=1,
        recovery_report={"next_action": "request_model"},
    )
    validation = ToolProtocolValidationResult(
        valid=True,
        batch={"batch_id": "batch_1", "run_id": "run_1", "session_id": "session_1", "task_id": "task_1", "phase_id": "phase_1", "model_request_id": "req_1", "model_response_id": "resp_1", "assistant_message": {}, "tool_calls": []},
    )
    report = ToolProtocolRecoveryReport(
        pending_call_ids=["call_1"],
        recovered_call_ids=["call_1"],
    )

    assert turn_result.to_dict()["status"] == "processed"
    assert validation.batch is not None
    assert report.next_action == "request_model"
