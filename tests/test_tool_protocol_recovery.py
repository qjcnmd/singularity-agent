from __future__ import annotations

from pathlib import Path
from typing import Any
from uuid import uuid4

from miniharness.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallPhase,
    ToolProtocolRecoveryReport,
    ToolProtocolTurnStatus,
)
from miniharness.tool_protocol.recovery import ToolProtocolRecoveryManager
from miniharness.tool_protocol.result import ToolProtocolResultBuilder
from miniharness.tool_protocol.state import ToolProtocolStateStore
from miniharness.tools.models import ToolResult


def _workspace_tmp(name: str) -> Path:
    path = Path("work/pytest-tmp") / f"{name}-{uuid4().hex}"
    path.mkdir(parents=True, exist_ok=True)
    return path


def _envelope(assistant_message_id: str = "assistant_1") -> ToolCallEnvelope:
    return ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id=assistant_message_id,
        tool_call_id="call_1",
        tool_name="read_file",
        raw_arguments='{"path":"README.md"}',
        parsed_arguments={"path": "README.md"},
        normalized_arguments={"path": "README.md"},
    )


def _batch() -> ToolCallBatch:
    return ToolCallBatch(
        batch_id="batch_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={"id": "assistant_1", "role": "assistant", "content": None},
        tool_calls=[_envelope()],
    )


def test_recovery_reports_pending_running_succeeded_and_missing_tool_messages(
) -> None:
    tmp_path = _workspace_tmp("tool-protocol-recovery")
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")
    batch = store.create_batch(_batch())
    envelope = batch.tool_calls[0]

    store.upsert_record(envelope, phase=ToolCallPhase.PROPOSED)
    pending = ToolProtocolRecoveryManager(store).recover(run_id="run_1")
    assert pending.next_action == "execute_pending_tool"
    assert pending.recovery_report["pending_call_ids"] == ["call_1"]

    running = store.upsert_record(envelope, phase=ToolCallPhase.RUNNING)
    report = ToolProtocolRecoveryManager(store).recover(run_id="run_1")
    assert report.recovery_report["running_call_ids"] == ["call_1"]
    assert report.next_action == "await_tool_result"

    result = ToolProtocolResultBuilder().build(
        envelope=envelope,
        result=ToolResult.success(content={"path": "README.md"}),
    )
    store.bind_result(running.record_id, result=result)
    report = ToolProtocolRecoveryManager(store).recover(run_id="run_1")
    assert report.recovery_report["succeeded_but_not_appended_call_ids"] == ["call_1"]
    assert report.next_action == "append_tool_message"


def test_recovery_report_serializes_and_defaults_to_request_model() -> None:
    report = ToolProtocolRecoveryReport()
    assert report.next_action == "request_model"
    assert ToolProtocolTurnStatus.RECOVERED.value == "recovered"
