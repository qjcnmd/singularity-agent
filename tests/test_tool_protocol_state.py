from __future__ import annotations

import sqlite3
from pathlib import Path
from uuid import uuid4

from miniharness.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolCallPhase,
    ToolProtocolResultEnvelope,
)
from miniharness.tool_protocol.result import ToolProtocolResultBuilder
from miniharness.tool_protocol.state import ToolProtocolStateStore
from miniharness.tools.models import ToolResult, ToolSideEffectKind


def _workspace_tmp(name: str) -> Path:
    path = Path("work/pytest-tmp") / f"{name}-{uuid4().hex}"
    path.mkdir(parents=True, exist_ok=True)
    return path


def make_envelope(
    tool_call_id: str = "call_1",
    *,
    raw_arguments: str = '{"path": "README.md"}',
    normalized_arguments: dict | None = None,
    tool_name: str = "read_file",
) -> ToolCallEnvelope:
    arguments = normalized_arguments or {"path": "README.md"}
    return ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="msg_1",
        tool_call_id=tool_call_id,
        tool_name=tool_name,
        raw_arguments=raw_arguments,
        parsed_arguments=dict(arguments),
        normalized_arguments=dict(arguments),
    )


def make_batch() -> ToolCallBatch:
    return ToolCallBatch(
        batch_id="batch_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={"id": "assistant_1", "role": "assistant", "content": None},
        tool_calls=[make_envelope()],
    )


def test_state_store_persists_batch_records_events_and_results() -> None:
    tmp_path = _workspace_tmp("tool-protocol-state")
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")

    batch = store.create_batch(make_batch())
    record = store.upsert_record(batch.tool_calls[0], phase=ToolCallPhase.VALIDATED)
    store.upsert_record(record.envelope, phase=ToolCallPhase.SCHEDULED)
    store.upsert_record(record.envelope, phase=ToolCallPhase.RUNNING)

    result = ToolProtocolResultBuilder().build(
        envelope=record.envelope,
        result=ToolResult.success(content={"path": "README.md"}),
        observation_id="obs_1",
        raw_result_ref="raw_1",
    )
    binding = store.bind_result(record.record_id, result=result)
    store.mark_result_appended(record.record_id, context_message_id="tool_msg_1")

    assert store.get_batch(batch.batch_id).batch_id == batch.batch_id
    assert store.get_record(record.record_id).phase == ToolCallPhase.RESULT_APPENDED
    assert store.get_record_by_call_id("run_1", "call_1").record_id == record.record_id
    assert [item.tool_call_id for item in store.pending_records(batch_id=batch.batch_id)] == []
    assert [item.tool_call_id for item in store.completed_records(batch_id=batch.batch_id)] == [
        "call_1"
    ]
    assert store.failed_records(batch_id=batch.batch_id) == []
    assert binding.appended is False
    assert store.result_binding(record.record_id).result.content_digest == result.content_digest
    assert [event.event_type for event in store.events_for_batch(batch.batch_id)] == [
        "proposed",
        "validated",
        "scheduled",
        "running",
        "succeeded",
        "result_appended",
    ]


def test_state_store_replay_protection_and_conflicts() -> None:
    tmp_path = _workspace_tmp("tool-protocol-replay")
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")
    batch = store.create_batch(make_batch())
    record = store.upsert_record(batch.tool_calls[0], phase=ToolCallPhase.VALIDATED)
    result = ToolProtocolResultBuilder().build(
        envelope=record.envelope,
        result=ToolResult.success(content={"path": "README.md"}),
    )
    store.bind_result(record.record_id, result=result)

    replay = store.resolve_replay(
        make_envelope(),
        side_effects=ToolSideEffectKind.READ_WORKSPACE,
        idempotent=True,
    )

    assert replay is not None
    assert replay.content_digest == result.content_digest

    assert (
        store.resolve_replay(
            make_envelope(tool_call_id="call_2", normalized_arguments={"path": "docs.md"}),
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            idempotent=True,
        )
        is None
    )

    decision = store.check_replay(
        make_envelope(tool_call_id="call_1", normalized_arguments={"path": "OTHER.md"}),
        idempotent=True,
        side_effects=ToolSideEffectKind.READ_WORKSPACE,
    )
    assert decision.status == ToolCallFailureKind.conflicting_replay.value
    assert decision.allowed is False

    assert (
        store.resolve_replay(
            make_envelope(tool_call_id="call_side_effect", tool_name="write_file"),
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            idempotent=True,
        )
        is None
    )

    side_effect_decision = store.check_replay(
        make_envelope(tool_call_id="call_1"),
        side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
        idempotent=True,
    )
    assert side_effect_decision.allowed is False
    assert side_effect_decision.status == "replay_not_allowed"


def test_state_store_queries_pending_by_run_session_task_and_batch_by_assistant_message() -> None:
    tmp_path = _workspace_tmp("tool-protocol-query")
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")
    batch = store.create_batch(make_batch())
    store.upsert_record(batch.tool_calls[0], phase=ToolCallPhase.VALIDATED)

    assert [record.tool_call_id for record in store.pending_calls(run_id="run_1")] == ["call_1"]
    assert [
        record.tool_call_id
        for record in store.pending_calls(session_id="session_1", task_id="task_1")
    ] == ["call_1"]
    assert store.batch_by_assistant_message_id("assistant_1").batch_id == "batch_1"
    assert store.batch_by_assistant_message_id("missing") is None


def test_state_store_exposes_independent_tables() -> None:
    db_path = _workspace_tmp("tool-protocol-schema") / "tool_protocol.sqlite3"
    ToolProtocolStateStore(db_path)

    connection = sqlite3.connect(db_path)
    rows = connection.execute(
        "select name from sqlite_master where type = 'table' order by name"
    ).fetchall()
    table_names = {row[0] for row in rows}

    assert {
        "tool_call_batches",
        "tool_call_records",
        "tool_protocol_events",
        "tool_result_bindings",
    }.issubset(table_names)
