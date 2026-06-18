import json
import sqlite3
from pathlib import Path
from typing import Any

import pytest

from miniharness.context import (
    ContextManager,
    ContextVersionConflict,
    ObservationStore,
    RecoveryManager,
    ReferenceResolver,
    TokenCounter,
)
from miniharness.provider import ToolChoiceMode


def tool_call(call_id: str, name: str = "read_file") -> dict[str, Any]:
    return {
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": '{"path": "README.md"}'},
    }


class CompressionProvider:
    def __init__(self) -> None:
        self.calls: list[dict[str, Any]] = []

    def chat(
        self,
        *,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        tool_choice: ToolChoiceMode | str = ToolChoiceMode.AUTO,
    ) -> dict[str, Any]:
        self.calls.append(
            {"messages": messages, "tools": tools, "tool_choice": tool_choice}
        )
        return {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": json.dumps(
                            {
                                "summary": "compressed facts",
                                "goal": "inspect project",
                                "constraints": ["read only"],
                                "verified_facts": ["README was read"],
                                "failed_attempts": [],
                                "reference_ids": ["ref_readme"],
                            }
                        ),
                    }
                }
            ]
        }


def test_token_counter_counts_messages_and_tools_with_exact_tokenizer() -> None:
    counter = TokenCounter(model="gpt-4o-mini")
    messages = [{"role": "user", "content": "hello world"}]
    tools = [
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file.",
                "parameters": {"type": "object", "properties": {}},
            },
        }
    ]

    assert counter.count_messages(messages) > 0
    assert counter.count_tools(tools) > 0


def test_messages_respect_budget_and_include_tool_schema_tokens(tmp_path: Path) -> None:
    context = ContextManager(
        system_prompt="system",
        user_goal="user",
        db_path=tmp_path / "context.sqlite3",
        model_context_window=120,
        output_token_reserve=20,
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )

    messages = context.messages(
        tools=[
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file.",
                    "parameters": {"type": "object", "properties": {}},
                },
            }
        ]
    )

    budget = context.last_budget
    assert budget is not None
    assert budget.tool_tokens > 0
    assert budget.output_token_reserve == 20
    assert budget.total_tokens <= 120
    assert messages[0]["role"] == "system"
    assert messages[1]["role"] == "user"


def test_window_trimming_keeps_tool_call_pairs_or_removes_them_together(
    tmp_path: Path,
) -> None:
    context = ContextManager(
        system_prompt="system",
        user_goal="user",
        db_path=tmp_path / "context.sqlite3",
        model_context_window=95,
        output_token_reserve=20,
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    context.add_assistant_message({"role": "assistant", "content": "old " * 120})
    old_call = tool_call("call_old")
    context.add_assistant_message(
        {"role": "assistant", "content": None, "tool_calls": [old_call]}
    )
    context.add_tool_result(
        tool_call=old_call,
        result={"ok": True, "content": "old result " * 120},
        turn=1,
    )

    messages = context.messages()

    assistant_tool_call_ids = {
        call["id"]
        for message in messages
        if message["role"] == "assistant"
        for call in message.get("tool_calls", [])
    }
    tool_message_ids = {
        message["tool_call_id"] for message in messages if message["role"] == "tool"
    }
    assert assistant_tool_call_ids == tool_message_ids
    assert messages[0]["role"] == "system"
    assert messages[1]["role"] == "user"


def test_compression_creates_summary_with_references_when_history_exceeds_budget(
    tmp_path: Path,
) -> None:
    provider = CompressionProvider()
    context = ContextManager(
        system_prompt="system",
        user_goal="inspect project",
        provider=provider,
        db_path=tmp_path / "context.sqlite3",
        model_context_window=100,
        output_token_reserve=20,
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    context.add_assistant_message({"role": "assistant", "content": "history " * 200})

    messages = context.messages()

    assert provider.calls
    assert provider.calls[0]["tool_choice"] == ToolChoiceMode.NONE
    assert any("compressed facts" in message.get("content", "") for message in messages)
    snapshot = context.store.latest_snapshot(context.run_id)
    assert snapshot is not None
    assert "ref_readme" in snapshot.known_observation_ids


def test_observation_store_persists_result_digest_preview_and_references(tmp_path: Path) -> None:
    db_path = tmp_path / "context.sqlite3"
    context = ContextManager(
        system_prompt="system",
        user_goal="user",
        db_path=db_path,
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    call = tool_call("call_readme")
    result = {
        "ok": True,
        "content": {
            "path": "README.md",
            "content": "hello",
            "bytes_read": 5,
            "bytes_total": 5,
        },
        "metadata": {"duration_seconds": 0.01, "cache_hit": False},
    }

    observation = context.add_tool_result(tool_call=call, result=result, turn=2)
    store = ObservationStore(db_path)
    reloaded = store.get_observation(observation.id)
    refs = ReferenceResolver(store).references_for_observation(observation.id)

    assert reloaded is not None
    assert reloaded.raw_result != result
    assert reloaded.raw_result["tool_name"] == "read_file"
    assert reloaded.raw_result["tool_call_id"] == "call_readme"
    assert "hello" in reloaded.raw_result["content_preview"]
    assert reloaded.raw_result["raw_digest"] == observation.raw_digest
    assert reloaded.raw_result["redacted"] is True
    assert reloaded.raw_digest == observation.raw_digest
    assert refs[0].path == "README.md"
    assert refs[0].digest == observation.raw_digest


def test_observation_store_does_not_persist_raw_result_or_secret_metadata(tmp_path: Path) -> None:
    db_path = tmp_path / "context.sqlite3"
    context = ContextManager(
        system_prompt="system",
        user_goal="user",
        db_path=db_path,
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    call = tool_call("call_secret")
    result = {
        "ok": True,
        "content": {"api_key": "sk-secret-value", "path": "README.md"},
        "metadata": {
            "raw_result": {"api_key": "sk-secret-value"},
            "token": "sk-secret-value",
            "safe": "README.md",
        },
    }

    context.add_tool_result(tool_call=call, result=result, turn=2)

    with sqlite3.connect(db_path) as connection:
        row = connection.execute("select raw_result, metadata, preview from observations").fetchone()
    serialized = "\n".join(str(value) for value in row if value is not None)
    assert "sk-secret-value" not in serialized
    assert "raw_result" not in serialized
    assert "<redacted:" in serialized
    assert "README.md" in serialized


def test_recovery_restores_completed_tool_result_without_repeating_call(
    tmp_path: Path,
) -> None:
    db_path = tmp_path / "context.sqlite3"
    trace_path = tmp_path / "run.jsonl"
    context = ContextManager(
        system_prompt="system",
        user_goal="user",
        db_path=db_path,
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    call = tool_call("call_readme")
    context.add_assistant_message({"role": "assistant", "content": None, "tool_calls": [call]})
    context.add_tool_result(
        tool_call=call,
        result={"ok": True, "content": {"path": "README.md", "content": "hello"}},
        turn=1,
    )
    trace_path.write_text(
        json.dumps(
            {
                "event": "tool_result",
                "data": {"tool_call_id": "call_readme"},
            }
        )
        + "\n",
        encoding="utf-8",
    )

    recovered = RecoveryManager(db_path, trace_path=trace_path).recover(context.run_id)

    assert recovered.last_completed_tool_call_ids == {"call_readme"}
    assert recovered.next_action == "request_model"
    assert recovered.trace_last_event == "tool_result"
    assert any(message["role"] == "tool" for message in recovered.messages)


def test_store_detects_version_conflicts_and_rolls_back_failed_transactions(
    tmp_path: Path,
) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    run_id = "run_a"
    first_version = store.current_version(run_id)

    store.bump_version(run_id, expected_version=first_version)

    with pytest.raises(ContextVersionConflict):
        store.bump_version(run_id, expected_version=first_version)

    before_count = store.observation_count(run_id)
    with pytest.raises(RuntimeError):
        with store.transaction(run_id, expected_version=store.current_version(run_id)):
            store._connection.execute("insert into observations(id, run_id) values(?, ?)", ("bad", run_id))
            raise RuntimeError("abort")
    assert store.observation_count(run_id) == before_count
