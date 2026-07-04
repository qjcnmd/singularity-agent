from __future__ import annotations

import json
import threading
from pathlib import Path
from typing import Any

from pydantic import BaseModel

from singularity.context import ContextManager
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.model import (
    MockModelProvider,
    ModelCapabilities,
    ModelMessage,
    ModelPurpose,
    ModelRunner,
    ModelToolCall,
    ModelToolParseStatus,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
)
from singularity.policy import DecisionOutcome
from singularity.tool_protocol.binding import ToolProtocolResultBinder
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tool_protocol.executor import ToolProtocolPlanExecutor
from singularity.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolCallPhase,
    ToolExecutionMode,
    ToolProtocolTurnStatus,
)
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tool_protocol.transitions import ToolProtocolStateTransitioner
from singularity.tools import ToolExecutionRequest, ToolExecutor, ToolPolicy, ToolRegistry, ToolResult
from singularity.tools.command import register_command_tools
from singularity.tools.models import PermissionLevel, ToolExecutionFailure, ToolSideEffectKind, ToolSpec
from tests.test_tool_executor_policy_approval import SequencedPolicyEngine
from tests.tool_executor_helpers import make_test_policy_engine


def _make_request(tmp_path: Path) -> tuple[ModelTurnRequest, ContextManager]:
    context = ContextManager(system_prompt="system", user_goal="inspect")
    component = ModelRunner.with_mock_provider(
        MockModelProvider(text="ok"),
        tool_registry=ToolRegistry(tmp_path),
    )
    request = component.build_request_from_context(
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


def _make_tool_protocol(
    tmp_path: Path,
    *,
    workspace_state_hook: Any | None = None,
) -> tuple[ToolProtocolEngine, ToolExecutor]:
    tool_executor = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    tool_protocol = ToolProtocolEngine(
        registry=ToolRegistry(tmp_path),
        trace=JsonlTraceRecorder.create(tmp_path),
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
        workspace_state_hook=workspace_state_hook,
    )
    return tool_protocol, tool_executor


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


def test_tool_protocol_executes_tool_call_and_appends_tool_message(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("Singularity README content", encoding="utf-8")
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
    tool_protocol, tool_executor = _make_tool_protocol(tmp_path)

    result = tool_protocol.process_model_turn(
        request=request,
        result=response,
        turn=1,
        context=context,
        tool_executor=tool_executor,
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
    assert "Singularity README content" in payload["content_preview"]
    assert context.tool_observations[-1].turn == 1


def test_tool_protocol_passes_structured_execution_request(tmp_path: Path) -> None:
    request, context = _make_request(tmp_path)
    response = ModelTurnResult(
        request_id=request.request_id,
        response_id="resp_request",
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
    tool_protocol = ToolProtocolEngine(
        registry=ToolRegistry(tmp_path),
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )

    class RequestOnlyExecutor:
        def __init__(self) -> None:
            self.requests: list[ToolExecutionRequest] = []

        def execute_tool_call(self, _tool_call: dict[str, Any]) -> ToolResult:
            raise AssertionError("protocol component should not pass provider dicts to ToolExecutor")

        def execute_request(self, execution_request: ToolExecutionRequest) -> ToolResult:
            self.requests.append(execution_request)
            return ToolResult.success(content={"content": "ok"})

    tool_executor = RequestOnlyExecutor()

    result = tool_protocol.process_model_turn(
        request=request,
        result=response,
        turn=1,
        context=context,
        tool_executor=tool_executor,  # type: ignore[arg-type]
    )

    assert result.status == ToolProtocolTurnStatus.PROCESSED
    assert len(tool_executor.requests) == 1
    execution_request = tool_executor.requests[0]
    assert execution_request.tool_call_id == "call_readme"
    assert execution_request.tool_name == "read_file"
    assert execution_request.batch_id == result.batch_id
    assert execution_request.run_id == context.run_id
    assert execution_request.model_request_id == request.request_id
    assert execution_request.model_response_id == response.response_id
    assert execution_request.normalized_arguments["path"] == "README.md"
    assert execution_request.normalized_arguments["max_bytes"] == 20000
    assert execution_request.argument_digest


def test_tool_protocol_replay_prevents_executor_ledger_reexecution(tmp_path: Path) -> None:
    class EchoInput(BaseModel):
        value: str

    calls: list[str] = []

    def handler(args: EchoInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="echo",
            description="echo",
            input_model=EchoInput,
            handler=handler,
            permission_level=PermissionLevel.READ_ONLY,
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            idempotent=True,
        )
    )
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    tool_protocol = ToolProtocolEngine(
        registry=registry,
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )

    def result(response_id: str) -> ModelTurnResult:
        return ModelTurnResult(
            request_id="req_replay",
            response_id=response_id,
            status=ModelTurnStatus.SUCCESS,
            assistant_message=ModelMessage.assistant_text(""),
            tool_calls=[
                ModelToolCall(
                    tool_call_id="call_echo",
                    tool_name="echo",
                    arguments={"value": "x"},
                    raw_arguments='{"value":"x"}',
                    parse_status=ModelToolParseStatus.VALID,
                )
            ],
        )

    first = tool_protocol.process_model_turn(
        request=None,
        result=result("resp_first"),
        turn=1,
        context=context,
        tool_executor=tool_executor,
    )
    second = tool_protocol.process_model_turn(
        request=None,
        result=result("resp_second"),
        turn=2,
        context=context,
        tool_executor=tool_executor,
    )

    assert first.executed_count == 1
    assert second.executed_count == 0
    assert second.appended_tool_message_count == 0
    assert calls == ["x"]
    assert tool_protocol.state_store.result_binding_by_tool_call_id("call_echo") is not None
    assert json.loads(context.messages()[-1]["content"])["ok"] is True


def test_tool_protocol_executes_parallel_read_only_group_concurrently(tmp_path: Path) -> None:
    barrier = threading.Barrier(2, timeout=3)
    calls_started: list[str] = []
    registry = ToolRegistry(tmp_path, include_default_tools=False)

    def make_handler(name: str):
        def handler(_args: _EmptyInput) -> dict[str, str]:
            calls_started.append(name)
            barrier.wait()
            return {"tool": name}

        return handler

    for name in ("read_one", "read_two"):
        registry.register(
            ToolSpec(
                name=name,
                description=name,
                input_model=_EmptyInput,
                handler=make_handler(name),
                permission_level=PermissionLevel.READ_ONLY,
                side_effects=ToolSideEffectKind.READ_WORKSPACE,
                idempotent=True,
            )
        )
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    tool_protocol = ToolProtocolEngine(
        registry=registry,
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )
    model_result = ModelTurnResult(
        request_id="req_parallel",
        response_id="resp_parallel",
        status=ModelTurnStatus.SUCCESS,
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_read_one",
                tool_name="read_one",
                arguments={},
                raw_arguments="{}",
                parse_status=ModelToolParseStatus.VALID,
            ),
            ModelToolCall(
                tool_call_id="call_read_two",
                tool_name="read_two",
                arguments={},
                raw_arguments="{}",
                parse_status=ModelToolParseStatus.VALID,
            ),
        ],
        metadata={
            "provider_capabilities": ModelCapabilities(
                supports_parallel_tool_calls=True
            ).to_dict()
        },
    )

    result = tool_protocol.handle_model_turn_result(
        model_result,
        context=context,
        tool_executor=tool_executor,
        planner=None,
    )

    assert result.status == ToolProtocolTurnStatus.PROCESSED
    assert result.executed_count == 2
    assert result.failed_count == 0
    assert result.appended_tool_message_count == 2
    assert result.metadata["execution_mode"] == ToolExecutionMode.PARALLEL_READONLY.value
    assert sorted(calls_started) == ["read_one", "read_two"]
    tool_payloads = [
        json.loads(message["content"])
        for message in context.messages()
        if message["role"] == "tool"
    ]
    assert [payload["tool_call_id"] for payload in tool_payloads] == [
        "call_read_one",
        "call_read_two",
    ]


def test_tool_call_provider_serialization_is_shared_with_execution_context() -> None:
    batch = ToolCallBatch(
        batch_id="batch_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={"role": "assistant", "content": None, "tool_calls": []},
    )
    model_call = ModelToolCall(
        tool_call_id="call_1",
        tool_name="read_file",
        arguments={"path": "README.md"},
        raw_arguments='{"path":"README.md"}',
        parse_status=ModelToolParseStatus.VALID,
    )
    envelope = ToolCallEnvelope(
        protocol_version="1.0",
        run_id=batch.run_id,
        session_id=batch.session_id,
        task_id=batch.task_id,
        phase_id=batch.phase_id,
        model_request_id=batch.model_request_id,
        model_response_id=batch.model_response_id,
        assistant_message_id="assistant_1",
        tool_call_id=model_call.tool_call_id,
        tool_name=model_call.tool_name,
        raw_arguments=model_call.raw_arguments,
        parsed_arguments=model_call.arguments,
        normalized_arguments=model_call.arguments,
    )

    assert envelope.to_provider_tool_call() == model_call.to_provider_tool_call()

    request = ToolExecutionRequest.from_envelope(envelope, batch=batch)
    assert request.batch_id == "batch_1"
    assert request.run_id == "run_1"
    assert request.session_id == "session_1"
    assert request.model_request_id == "req_1"


def test_tool_protocol_creates_synthetic_result_for_rejected_call(tmp_path: Path) -> None:
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
    tool_protocol, tool_executor = _make_tool_protocol(tmp_path)

    result = tool_protocol.process_model_turn(
        request=request,
        result=response,
        turn=1,
        context=context,
        tool_executor=tool_executor,
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
    assert payload["error_kind"] == "unknown_tool"


def test_tool_protocol_maps_internal_validation_reason_to_canonical_error_code(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_protocol = ToolProtocolEngine(
        registry=registry,
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )
    batch = ToolCallBatch(
        batch_id="batch_internal_validation",
        run_id=context.run_id,
        session_id=context.session_id,
        task_id=context.task_id,
        phase_id=context.phase_id,
        model_request_id="req_internal_validation",
        model_response_id="resp_internal_validation",
        assistant_message={"role": "assistant", "content": None, "tool_calls": []},
        tool_calls=[
            ToolCallEnvelope(
                protocol_version="1.0",
                run_id=context.run_id,
                session_id=context.session_id,
                task_id=context.task_id,
                phase_id=context.phase_id,
                model_request_id="req_internal_validation",
                model_response_id="resp_internal_validation",
                assistant_message_id="assistant_internal_validation",
                tool_call_id="call_internal",
                tool_name="read_file",
                raw_arguments="{}",
                parsed_arguments={},
                normalized_arguments={},
                validation_errors=["missing_tool_call_id"],
            )
        ],
    )
    tool_protocol.state_store.save_batch(batch)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = tool_protocol.execute_plan(
        tool_protocol.build_execution_plan(batch),
        context=context,
        tool_executor=tool_executor,
        planner=None,
    )

    payload = json.loads(context.messages()[-1]["content"])
    assert result.status == ToolProtocolTurnStatus.REJECTED
    assert payload["error_kind"] == "missing_tool_call_id"
    assert payload["error_code"] == "protocol_violation"


def test_parallel_readonly_validation_uses_same_synthetic_lifecycle_trace(
    tmp_path: Path,
) -> None:
    trace = _TraceCollector()
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="read_one",
            description="read",
            input_model=_EmptyInput,
            handler=lambda _args: {"ok": True},
            permission_level=PermissionLevel.READ_ONLY,
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            idempotent=True,
        )
    )
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    tool_protocol = ToolProtocolEngine(
        registry=registry,
        trace=trace,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )
    batch = ToolCallBatch(
        batch_id="batch_parallel_validation",
        run_id=context.run_id,
        session_id=context.session_id,
        task_id=context.task_id,
        phase_id=context.phase_id,
        model_request_id="req_parallel_validation",
        model_response_id="resp_parallel_validation",
        assistant_message={"role": "assistant", "content": None, "tool_calls": []},
        tool_calls=[
            ToolCallEnvelope(
                protocol_version="1.0",
                run_id=context.run_id,
                session_id=context.session_id,
                task_id=context.task_id,
                phase_id=context.phase_id,
                model_request_id="req_parallel_validation",
                model_response_id="resp_parallel_validation",
                assistant_message_id="assistant_parallel_validation",
                tool_call_id="call_bad",
                tool_name="read_one",
                raw_arguments="{}",
                parsed_arguments={},
                normalized_arguments={},
                validation_errors=["schema_mismatch"],
            ),
            ToolCallEnvelope(
                protocol_version="1.0",
                run_id=context.run_id,
                session_id=context.session_id,
                task_id=context.task_id,
                phase_id=context.phase_id,
                model_request_id="req_parallel_validation",
                model_response_id="resp_parallel_validation",
                assistant_message_id="assistant_parallel_validation",
                tool_call_id="call_ok",
                tool_name="read_one",
                raw_arguments="{}",
                parsed_arguments={},
                normalized_arguments={},
            ),
        ],
        supports_parallel_execution=True,
    )
    tool_protocol.state_store.save_batch(batch)
    plan = tool_protocol.build_execution_plan(batch)

    result = tool_protocol.execute_plan(
        plan,
        context=context,
        tool_executor=tool_executor,
        planner=None,
    )

    event_names = [event for event, _payload in trace.events]
    assert result.rejected_count == 1
    assert "tool_protocol.call_rejected" in event_names
    assert "tool_protocol.synthetic_result_created" in event_names


def test_tool_protocol_invokes_workspace_state_hook(tmp_path: Path) -> None:
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

    tool_protocol, tool_executor = _make_tool_protocol(
        tmp_path,
        workspace_state_hook=workspace_state_hook,
    )

    result = tool_protocol.process_model_turn(
        request=request,
        result=response,
        turn=7,
        context=context,
        tool_executor=tool_executor,
    )

    assert result.status == ToolProtocolTurnStatus.PROCESSED
    assert len(hook_calls) == 1
    tool_messages = [message for message in context.messages() if message["role"] == "tool"]
    assert len(tool_messages) == 1
    assert any(
        message["role"] == "system" and "workspace_state" in str(message.get("content"))
        for message in context.messages()
    )


def test_tool_protocol_appends_tool_message_when_tool_executor_fails(tmp_path: Path) -> None:
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
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    tool_protocol = ToolProtocolEngine(
        registry=registry,
        trace=JsonlTraceRecorder.create(tmp_path),
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )

    result = tool_protocol.handle_model_turn_result(
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
        tool_executor=tool_executor,
        planner=None,
    )

    assert result.failed_count == 1
    assert result.appended_tool_message_count == 1
    tool_message = [message for message in context.messages() if message["role"] == "tool"][-1]
    payload = json.loads(tool_message["content"])
    assert payload["tool_call_id"] == "call_explode"
    assert payload["ok"] is False
    assert payload["error_code"] == "boom"
    assert "super-secret" not in tool_message["content"]


def test_tool_protocol_marks_pending_approval_next_action(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="review_tool",
            description="requires review",
            input_model=_EmptyInput,
            handler=lambda _args: {"ok": True},
        )
    )
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=SequencedPolicyEngine([DecisionOutcome.REQUIRE_REVIEW]),
    )
    tool_protocol = ToolProtocolEngine(
        registry=registry,
        trace=JsonlTraceRecorder.create(tmp_path),
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )
    component = ModelRunner.with_mock_provider(
        MockModelProvider(text="ok"),
        tool_registry=registry,
    )
    request = component.build_request_from_context(
        context,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        allowed_tool_names=["review_tool"],
    )

    result = tool_protocol.process_model_turn(
        request=request,
        result=_tool_result(
            ModelToolCall(
                tool_call_id="call_review",
                tool_name="review_tool",
                arguments={},
                raw_arguments="{}",
                parse_status=ModelToolParseStatus.VALID,
            )
        ),
        turn=1,
        context=context,
        tool_executor=tool_executor,
    )

    assert result.status == ToolProtocolTurnStatus.PENDING_APPROVAL
    assert result.next_action == "pending_approval"


def test_tool_protocol_marks_existing_context_tool_message_as_appended(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_protocol = ToolProtocolEngine(
        registry=registry,
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )
    call = ModelToolCall(
        tool_call_id="call_readme",
        tool_name="read_file",
        arguments={"path": "README.md"},
        raw_arguments='{"path":"README.md"}',
        parse_status=ModelToolParseStatus.VALID,
    )
    model_result = _tool_result(call)
    assistant_message = tool_protocol._assistant_message_from_model_result(model_result)
    validation = tool_protocol.validate_batch(
        model_result,
        context=context,
        assistant_message=assistant_message,
    )
    batch = tool_protocol.state_store.save_batch(validation.batch)
    record = tool_protocol.state_store.upsert_record(
        batch.tool_calls[0],
        phase=ToolCallPhase.SUCCEEDED,
    )
    result = tool_protocol._synthetic_result(
        batch.tool_calls[0],
        error_kind=ToolCallFailureKind.replay_detected,
        message="already appended",
        error_code="replay_detected",
    )
    tool_protocol.state_store.bind_result(record.record_id, result=result)
    context.add_tool_protocol_result(result)

    observation_id = tool_protocol.append_results_to_context(
        context,
        envelope=batch.tool_calls[0],
        result=result,
    )

    binding = tool_protocol.state_store.result_binding(record.record_id)
    assert observation_id is None
    assert binding is not None
    assert binding.appended is True


def test_tool_protocol_engine_delegates_execution_state_and_binding_boundaries(
    tmp_path: Path,
) -> None:
    tool_protocol = ToolProtocolEngine(
        registry=ToolRegistry(tmp_path),
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )

    assert isinstance(tool_protocol.plan_executor, ToolProtocolPlanExecutor)
    assert isinstance(tool_protocol.state_transitions, ToolProtocolStateTransitioner)
    assert isinstance(tool_protocol.result_binder, ToolProtocolResultBinder)


def test_tool_protocol_blocks_side_effect_replay_without_calling_handler(tmp_path: Path) -> None:
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
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=SequencedPolicyEngine([DecisionOutcome.ALLOW]),  # type: ignore[arg-type]
    )
    tool_protocol = ToolProtocolEngine(
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

    first = tool_protocol.handle_model_turn_result(
        _tool_result(first_call, response_id="resp_write_1"),
        context=context,
        tool_executor=tool_executor,
        planner=None,
    )
    calls.clear()
    replay = tool_protocol.handle_model_turn_result(
        _tool_result(first_call, response_id="resp_write_2"),
        context=context,
        tool_executor=tool_executor,
        planner=None,
    )

    assert first.executed_count == 1
    assert replay.rejected_count == 1
    assert calls == []
    tool_payload = json.loads([message for message in context.messages() if message["role"] == "tool"][-1]["content"])
    assert tool_payload["error_code"] == "side_effect_replay"


def test_tool_protocol_appends_policy_and_sandbox_results_to_context(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    register_command_tools(registry)
    context = ContextManager(system_prompt="system", user_goal="inspect")
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    tool_protocol = ToolProtocolEngine(
        registry=registry,
        trace=JsonlTraceRecorder.create(tmp_path),
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
        tool_protocol.handle_model_turn_result(
            response,
            context=context,
            tool_executor=tool_executor,
            planner=None,
        )

    tool_payloads = [
        json.loads(message["content"])
        for message in context.messages()
        if message["role"] == "tool"
    ]
    error_codes = {payload["tool_call_id"]: payload["error_code"] for payload in tool_payloads}
    assert error_codes["call_policy"] is None
    assert error_codes["call_sandbox"] in {"sandbox_required", "policy_denied", "review_required"}


def test_tool_protocol_traces_only_digests_not_raw_payloads(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("API_KEY=super-secret-value", encoding="utf-8")
    request, context = _make_request(tmp_path)
    trace = _TraceCollector()
    tool_executor = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    tool_protocol = ToolProtocolEngine(
        registry=ToolRegistry(tmp_path),
        trace=trace,
        state_store=ToolProtocolStateStore(tmp_path / "tool_protocol.sqlite3"),
    )

    tool_protocol.process_model_turn(
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
        tool_executor=tool_executor,
    )

    serialized = json.dumps(trace.events, ensure_ascii=False, default=str)
    event_names = {event for event, _payload in trace.events}
    assert "tool_protocol.call_validated" in event_names
    assert "tool_protocol.call_scheduled" in event_names
    assert "tool_protocol.result_bound" in event_names
    assert "super-secret-value" not in serialized
    assert "raw_result" not in serialized
    assert "raw_arguments" not in serialized
