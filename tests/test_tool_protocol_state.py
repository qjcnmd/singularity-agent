from __future__ import annotations

import sqlite3
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

from singularity.runtime.defaults import SQLITE_BUSY_TIMEOUT_MS
from singularity.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolCallPhase,
    ToolProtocolResultEnvelope,
)
from singularity.tool_protocol.result import ToolProtocolResultBuilder
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tools.models import ToolResult, ToolSideEffectKind


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


def test_state_store_persists_batch_records_events_and_results(tmp_path: Path) -> None:
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
    assert all(event.tool_call_id == "call_1" for event in store.events_for_batch(batch.batch_id))


def test_state_store_replay_protection_and_conflicts(tmp_path: Path) -> None:
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
    assert side_effect_decision.status == "side_effect_replay"


def test_state_store_result_appended_preserves_waiting_approval_phase(tmp_path: Path) -> None:
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")
    batch = store.create_batch(make_batch())
    record = store.upsert_record(batch.tool_calls[0], phase=ToolCallPhase.WAITING_APPROVAL)
    result = ToolProtocolResultBuilder().build(
        envelope=record.envelope,
        result=ToolResult.failure(code="approval_required", message="needs approval"),
    )
    store.bind_result(record.record_id, result=result)
    store.transition(record.envelope.tool_call_id, ToolCallPhase.WAITING_APPROVAL)

    store.mark_result_appended(record.record_id, context_message_id="tool_msg_approval")

    recovered = store.get_record(record.record_id)
    assert recovered.phase == ToolCallPhase.WAITING_APPROVAL
    assert recovered.context_message_id == "tool_msg_approval"


def test_state_store_queries_pending_by_run_session_task_and_batch_by_assistant_message(tmp_path: Path) -> None:
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


def test_state_store_redacts_protocol_arguments_before_persistence(tmp_path: Path) -> None:
    db_path = tmp_path / "tool_protocol.sqlite3"
    store = ToolProtocolStateStore(db_path)
    secret_call = make_envelope(
        raw_arguments='{"api_key":"sk-secret-value","path":"README.md"}',
        normalized_arguments={"api_key": "sk-secret-value", "path": "README.md"},
    )
    batch = ToolCallBatch(
        batch_id="batch_secret",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={"id": "assistant_secret", "role": "assistant", "content": None},
        tool_calls=[secret_call],
    )

    store.create_batch(batch)
    record = store.upsert_record(secret_call, phase=ToolCallPhase.VALIDATED)
    store.bind_result(
        record.record_id,
        result=ToolProtocolResultEnvelope(
            tool_call_id=secret_call.tool_call_id,
            tool_name=secret_call.tool_name,
            ok=True,
            status="ok",
            content_preview="api_key=sk-secret-value",
            content_digest="digest",
            raw_result_ref="artifact_digest",
            redacted=True,
            metadata={
                "raw_result": {"api_key": "sk-secret-value"},
                "token": "sk-secret-value",
                "safe": "README.md",
            },
        ),
    )

    texts: list[str] = []
    with sqlite3.connect(db_path) as connection:
        for table, columns in {
            "tool_call_batches": ["assistant_message", "tool_calls"],
            "tool_call_records": [
                "raw_arguments",
                "parsed_arguments",
                "normalized_arguments",
                "envelope",
            ],
            "tool_result_bindings": ["result_payload", "metadata"],
            "tool_protocol_events": ["payload"],
        }.items():
            rows = connection.execute(
                f"select {', '.join(columns)} from {table}"
            ).fetchall()
            for row in rows:
                texts.extend(str(value) for value in row if value is not None)

    serialized = "\n".join(texts)
    assert "sk-secret-value" not in serialized
    assert '"raw_result"' not in serialized
    assert "<redacted:" in serialized
    assert "README.md" in serialized


def test_state_store_exposes_independent_tables(tmp_path: Path) -> None:
    db_path = tmp_path / "tool_protocol.sqlite3"
    ToolProtocolStateStore(db_path)

    with sqlite3.connect(db_path) as connection:
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


def test_state_store_close_releases_sqlite_connection(tmp_path: Path) -> None:
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")

    store.close()

    with pytest.raises(sqlite3.ProgrammingError):
        store.connection.execute("select 1")


def test_state_store_sets_busy_timeout_from_runtime_default_for_file_and_memory_db(tmp_path: Path) -> None:
    file_store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")
    memory_store = ToolProtocolStateStore()

    assert file_store.connection.execute("pragma busy_timeout").fetchone()[0] == SQLITE_BUSY_TIMEOUT_MS
    assert memory_store.connection.execute("pragma busy_timeout").fetchone()[0] == SQLITE_BUSY_TIMEOUT_MS

    file_store.close()
    memory_store.close()


def test_state_store_supports_cross_thread_public_reads(tmp_path: Path) -> None:
    store = ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3")
    batch = store.create_batch(make_batch())
    record = store.upsert_record(batch.tool_calls[0], phase=ToolCallPhase.VALIDATED)

    def read_record_id() -> str:
        return store.get_record(record.record_id).record_id

    with ThreadPoolExecutor(max_workers=1) as executor:
        result = executor.submit(read_record_id).result()

    assert result == record.record_id
