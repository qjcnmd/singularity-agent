from __future__ import annotations

from pathlib import Path

from miniharness.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolCallPhase,
    ToolProtocolResultEnvelope,
    ToolProtocolRecoveryReport,
    ToolProtocolTurnStatus,
)
from miniharness.tool_protocol.recovery import ToolProtocolRecoveryManager
from miniharness.tool_protocol.result import ToolProtocolResultBuilder
from miniharness.tool_protocol.state import ToolProtocolStateStore
from miniharness.tools.models import ToolResult


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


def _batch(
    *,
    batch_id: str = "batch_1",
    run_id: str = "run_1",
    session_id: str = "session_1",
    task_id: str = "task_1",
    tool_call_id: str = "call_1",
) -> ToolCallBatch:
    return ToolCallBatch(
        batch_id=batch_id,
        run_id=run_id,
        session_id=session_id,
        task_id=task_id,
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={"id": "assistant_1", "role": "assistant", "content": None},
        tool_calls=[
            ToolCallEnvelope(
                **{
                    **_envelope().to_dict(),
                    "run_id": run_id,
                    "session_id": session_id,
                    "task_id": task_id,
                    "tool_call_id": tool_call_id,
                }
            )
        ],
    )


def test_recovery_reports_pending_running_succeeded_and_missing_tool_messages(
    tmp_path: Path,
) -> None:
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


def test_recovery_reports_bound_approval_result_as_pending_approval(tmp_path: Path) -> None:
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")
    batch = store.create_batch(_batch())
    record = store.upsert_record(batch.tool_calls[0], phase=ToolCallPhase.RUNNING)
    store.bind_result(
        record.record_id,
        result=ToolProtocolResultEnvelope(
            tool_call_id=record.tool_call_id,
            tool_name=record.envelope.tool_name,
            ok=False,
            status="waiting_approval",
            error_code="approval_required",
            error_kind=ToolCallFailureKind.approval_required,
            content_preview="approval required",
            content_digest="approval_digest",
        ),
    )

    report = ToolProtocolRecoveryManager(store).recover(run_id="run_1")

    assert report.status == ToolProtocolTurnStatus.PENDING_APPROVAL
    assert report.next_action == "resume_pending_approval"
    assert report.pending_approval_count == 1
    assert "pending approval: call_1" in report.recovery_report["warnings"]


def test_recovery_filters_by_session_and_task_scope(tmp_path: Path) -> None:
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")
    first = store.create_batch(_batch(batch_id="batch_1", session_id="session_a", task_id="task_a", tool_call_id="call_a"))
    second = store.create_batch(_batch(batch_id="batch_2", session_id="session_b", task_id="task_b", tool_call_id="call_b"))
    store.upsert_record(first.tool_calls[0], batch_id=first.batch_id, phase=ToolCallPhase.PROPOSED)
    store.upsert_record(second.tool_calls[0], batch_id=second.batch_id, phase=ToolCallPhase.PROPOSED)

    report = ToolProtocolRecoveryManager(store).recover(
        run_id="run_1",
        session_id="session_b",
        task_id="task_b",
    )

    assert report.recovery_report["pending_call_ids"] == ["call_b"]
