from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel

from miniharness.context import ContextManager
from miniharness.model import (
    ModelMessage,
    ModelPurpose,
    ModelRuntime,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
    ModelToolCall,
    ModelToolParseStatus,
    MockModelProvider,
)
from miniharness.tool_protocol.models import ToolProtocolTurnStatus
from miniharness.tool_protocol.runtime import ToolCallingProtocolRuntime
from miniharness.tool_protocol.state import ToolProtocolStateStore
from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.tools.command import register_command_tools
from miniharness.tools.models import PermissionLevel, ToolExecutionFailure, ToolSideEffectKind, ToolSpec
from miniharness.trace import TraceWriter
from tests.tool_runtime_helpers import make_test_policy_runtime
from tests.test_tool_runtime_policy_approval import SequencedPolicyRuntime
from miniharness.policy import DecisionOutcome


def _make_request(tmp_path: Path) -> tuple[ModelTurnRequest, ContextManager]:
    context = ContextManager(system_prompt="system", user_goal="inspect")
    runtime = ModelRuntime.with_mock_provider(
        MockModelProvider(text="ok"),
        tool_registry=ToolRegistry(tmp_path),
    )
    request = runtime.build_request_from_context(
        context,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="understanding_task",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        allowed_tool_names=["read_file"],
    )
    return request, context


def _make_protocol_runtime(
    tmp_path: Path,
    *,
    workspace_state_hook: Any | None = None,
) -> tuple[ToolCallingProtocolRuntime, ToolRuntime]:
    tool_runtime = ToolRuntime(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )
    protocol_runtime = ToolCallingProtocolRuntime(
        registry=ToolRegistry(tmp_path),
        trace=TraceWriter.create(tmp_path),
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
        workspace_state_hook=workspace_state_hook,
    )
    return protocol_runtime, tool_runtime


class _EmptyInput(BaseModel):
    pass


class _TraceCollector:
    def __init__(self) -> None:
        self.events: list[tuple[str, dict[str, Any]]] = []

    def record(self, event: str, data: dict[str, Any]) -> None:
        self.events.append((event, data))


def _tool_result(call: ModelToolCall, *, response_id: str = "resp_tool") -> ModelTurnResult:
    return ModelTurnResult(
        request_id="req_1",
        response_id=response_id,
        status=ModelTurnStatus.SUCCESS,
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[call],
    )


def test_protocol_runtime_executes_tool_call_and_appends_tool_message(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("MiniHarness README content", encoding="utf-8")
    request, context = _make_request(tmp_path)
    response = ModelTurnResult(
        request_id=request.request_id,
        response_id="resp_1",
        status=ModelTurnStatus.SUCCESS,
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_readme",
                tool_name="read_file",
                arguments={"path": "README.md", "max_bytes": 100},
                raw_arguments='{"path":"README.md","max_bytes":100}',
                parse_status=ModelToolParseStatus.VALID,
            )
        ],
    )
    protocol_runtime, tool_runtime = _make_protocol_runtime(tmp_path)

    result = protocol_runtime.process_model_turn(
        request=request,
        result=response,
        turn=1,
        context=context,
        tool_runtime=tool_runtime,
    )

    assert result.status == ToolProtocolTurnStatus.PROCESSED
    assert result.executed_count == 1
    assert result.appended_tool_message_count == 1
    tool_message = context.messages()[-1]
    assert tool_message["role"] == "tool"
    payload = json.loads(tool_message["content"])
    assert payload["tool_call_id"] == "call_readme"
    assert payload["tool_name"] == "read_file"
    assert payload["ok"] is True
    assert "MiniHarness README content" in payload["content_preview"]


def test_protocol_runtime_creates_synthetic_result_for_rejected_call(tmp_path: Path) -> None:
    request, context = _make_request(tmp_path)
    response = ModelTurnResult(
        request_id=request.request_id,
        response_id="resp_2",
        status=ModelTurnStatus.SUCCESS,
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_missing",
                tool_name="missing_tool",
                arguments={},
                raw_arguments="{}",
                parse_status=ModelToolParseStatus.UNKNOWN_TOOL,
                validation_errors=["unknown_tool"],
            )
        ],
    )
    protocol_runtime, tool_runtime = _make_protocol_runtime(tmp_path)

    result = protocol_runtime.process_model_turn(
        request=request,
        result=response,
        turn=1,
        context=context,
        tool_runtime=tool_runtime,
    )

    assert result.status == ToolProtocolTurnStatus.REJECTED
    assert result.rejected_count == 1
    tool_message = context.messages()[-1]
    payload = json.loads(tool_message["content"])
    assert payload["tool_call_id"] == "call_missing"
    assert payload["tool_name"] == "missing_tool"
    assert payload["ok"] is False
    assert payload["status"] == "rejected"
    assert payload["error_code"] == "unknown_tool"


def test_protocol_runtime_invokes_workspace_state_hook(tmp_path: Path) -> None:
    request, context = _make_request(tmp_path)
    response = ModelTurnResult(
        request_id=request.request_id,
        response_id="resp_3",
        status=ModelTurnStatus.SUCCESS,
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_readme",
                tool_name="read_file",
                arguments={"path": "README.md"},
                raw_arguments='{"path":"README.md"}',
                parse_status=ModelToolParseStatus.VALID,
            )
        ],
    )
    hook_calls: list[tuple[str, int]] = []

    def workspace_state_hook(hook_context: ContextManager, *, batch: Any, tool_call_id: str | None) -> None:
        hook_calls.append((str(batch.batch_id), 7))
        _ = tool_call_id
        hook_context.add_workspace_state({"workspace_state": {"status": "clean"}})

    protocol_runtime, tool_runtime = _make_protocol_runtime(
        tmp_path,
        workspace_state_hook=workspace_state_hook,
    )

    result = protocol_runtime.process_model_turn(
        request=request,
        result=response,
        turn=7,
        context=context,
        tool_runtime=tool_runtime,
    )

    assert result.status == ToolProtocolTurnStatus.PROCESSED
    assert len(hook_calls) == 1
    tool_messages = [message for message in context.messages() if message["role"] == "tool"]
    assert len(tool_messages) == 1
    assert any(
        message["role"] == "system" and "workspace_state" in str(message.get("content"))
        for message in context.messages()
    )


def test_protocol_runtime_appends_tool_message_when_tool_runtime_fails(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="explode",
            description="explode",
            input_model=_EmptyInput,
            handler=lambda _args: (_ for _ in ()).throw(
                ToolExecutionFailure("API_KEY=super-secret", code="boom")
            ),
        )
    )
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )
    protocol_runtime = ToolCallingProtocolRuntime(
        registry=registry,
        trace=TraceWriter.create(tmp_path),
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )

    result = protocol_runtime.handle_model_turn_result(
        _tool_result(
            ModelToolCall(
                tool_call_id="call_explode",
                tool_name="explode",
                arguments={},
                raw_arguments="{}",
                parse_status=ModelToolParseStatus.VALID,
            )
        ),
        context=context,
        tool_runtime=tool_runtime,
        planner=None,
        policy_runtime=None,
    )

    assert result.failed_count == 1
    assert result.appended_tool_message_count == 1
    tool_message = [message for message in context.messages() if message["role"] == "tool"][-1]
    payload = json.loads(tool_message["content"])
    assert payload["tool_call_id"] == "call_explode"
    assert payload["ok"] is False
    assert payload["error_code"] == "boom"
    assert "super-secret" not in tool_message["content"]


def test_protocol_runtime_blocks_side_effect_replay_without_calling_handler(tmp_path: Path) -> None:
    calls = []
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_file",
            description="write",
            input_model=_EmptyInput,
            handler=lambda _args: calls.append("called") or {"ok": True},
            permission_level=PermissionLevel.READ_ONLY,
            side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
            idempotent=False,
        )
    )
    context = ContextManager(system_prompt="system", user_goal="mutate")
    tool_runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=SequencedPolicyRuntime([DecisionOutcome.ALLOW]),  # type: ignore[arg-type]
    )
    protocol_runtime = ToolCallingProtocolRuntime(
        registry=registry,
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )
    first_call = ModelToolCall(
        tool_call_id="call_write",
        tool_name="write_file",
        arguments={},
        raw_arguments="{}",
        parse_status=ModelToolParseStatus.VALID,
    )

    first = protocol_runtime.handle_model_turn_result(
        _tool_result(first_call, response_id="resp_write_1"),
        context=context,
        tool_runtime=tool_runtime,
        planner=None,
        policy_runtime=None,
    )
    calls.clear()
    replay = protocol_runtime.handle_model_turn_result(
        _tool_result(first_call, response_id="resp_write_2"),
        context=context,
        tool_runtime=tool_runtime,
        planner=None,
        policy_runtime=None,
    )

    assert first.executed_count == 1
    assert replay.rejected_count == 1
    assert calls == []
    tool_payload = json.loads([message for message in context.messages() if message["role"] == "tool"][-1]["content"])
    assert tool_payload["error_code"] == "side_effect_replay"


def test_protocol_runtime_appends_policy_and_sandbox_results_to_context(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    register_command_tools(registry)
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )
    protocol_runtime = ToolCallingProtocolRuntime(
        registry=registry,
        trace=TraceWriter.create(tmp_path),
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )

    for response in [
        _tool_result(
            ModelToolCall(
                tool_call_id="call_policy",
                tool_name="run_command",
                arguments={"argv": ["python", "-V"]},
                raw_arguments='{"argv":["python","-V"]}',
                parse_status=ModelToolParseStatus.VALID,
            ),
            response_id="resp_policy",
        ),
        _tool_result(
            ModelToolCall(
                tool_call_id="call_sandbox",
                tool_name="start_process",
                arguments={"argv": ["python", "-m", "http.server"]},
                raw_arguments='{"argv":["python","-m","http.server"]}',
                parse_status=ModelToolParseStatus.VALID,
            ),
            response_id="resp_sandbox",
        ),
    ]:
        protocol_runtime.handle_model_turn_result(
            response,
            context=context,
            tool_runtime=tool_runtime,
            planner=None,
            policy_runtime=None,
        )

    tool_payloads = [
        json.loads(message["content"])
        for message in context.messages()
        if message["role"] == "tool"
    ]
    error_codes = {payload["tool_call_id"]: payload["error_code"] for payload in tool_payloads}
    assert error_codes["call_policy"] == "policy_denied"
    assert error_codes["call_sandbox"] in {"sandbox_required", "policy_denied"}


def test_protocol_runtime_traces_only_digests_not_raw_payloads(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("API_KEY=super-secret-value", encoding="utf-8")
    request, context = _make_request(tmp_path)
    trace = _TraceCollector()
    tool_runtime = ToolRuntime(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )
    protocol_runtime = ToolCallingProtocolRuntime(
        registry=ToolRegistry(tmp_path),
        trace=trace,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )

    protocol_runtime.process_model_turn(
        request=request,
        result=ModelTurnResult(
            request_id=request.request_id,
            response_id="resp_trace",
            status=ModelTurnStatus.SUCCESS,
            assistant_message=ModelMessage.assistant_text(""),
            tool_calls=[
                ModelToolCall(
                    tool_call_id="call_read_secret",
                    tool_name="read_file",
                    arguments={"path": "README.md"},
                    raw_arguments='{"path":"README.md"}',
                    parse_status=ModelToolParseStatus.VALID,
                )
            ],
        ),
        turn=1,
        context=context,
        tool_runtime=tool_runtime,
    )

    serialized = json.dumps(trace.events, ensure_ascii=False, default=str)
    event_names = {event for event, _payload in trace.events}
    assert "tool_protocol.call_validated" in event_names
    assert "tool_protocol.call_scheduled" in event_names
    assert "tool_protocol.result_bound" in event_names
    assert "super-secret-value" not in serialized
    assert "raw_result" not in serialized
    assert "raw_arguments" not in serialized
