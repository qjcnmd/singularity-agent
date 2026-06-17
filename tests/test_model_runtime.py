from pathlib import Path

from miniharness.context import ContextManager
from miniharness.model import (
    ContentBlock,
    ContentBlockType,
    MockModelProvider,
    ModelBudget,
    ModelPurpose,
    ModelRuntime,
    ModelRuntimeConfig,
    ModelTurnRequest,
    ModelTurnStatus,
    ModelToolCall,
    ModelToolParseStatus,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from miniharness.observability import TraceRuntime
from miniharness.tools import ToolRegistry


def test_model_runtime_success_tool_call_trace_and_budget(tmp_path: Path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_1", session_id="session_1")
    provider = MockModelProvider(
        text="",
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_1",
                tool_name="read_file",
                arguments={"path": "README.md"},
                raw_arguments='{"path":"README.md"}',
                parse_status=ModelToolParseStatus.VALID,
            )
        ],
    )
    runtime = ModelRuntime.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        trace=trace,
    )
    request = ModelTurnRequest(
        request_id="req_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="understanding_task",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        messages=[],
        tools=[],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.AUTO),
        budget=ModelBudget(max_input_tokens=1000),
    )

    result = runtime.run_turn(request)

    assert result.status == ModelTurnStatus.SUCCESS
    assert result.tool_calls[0].tool_name == "read_file"
    assert provider.complete_calls == 1
    event_types = [event.event_type.value for event in trace.store.query_events()]
    assert "model.request.created" in event_types
    assert "model.tool_call.proposed" in event_types


def test_model_runtime_invalid_tool_call_does_not_execute_provider_result(tmp_path: Path) -> None:
    provider = MockModelProvider(
        text="",
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_bad",
                tool_name="missing",
                arguments={},
                raw_arguments="{}",
                parse_status=ModelToolParseStatus.UNKNOWN_TOOL,
            )
        ],
    )
    runtime = ModelRuntime.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))

    result = runtime.run_turn(
        ModelTurnRequest(
            request_id="req_1",
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            phase_id="understanding_task",
            action_id="action_1",
            purpose=ModelPurpose.PLAN_NEXT_ACTION,
            messages=[],
            tools=[],
            tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.AUTO),
        )
    )

    assert result.status == ModelTurnStatus.INVALID
    assert result.validation and not result.validation.valid


def test_model_runtime_blocks_secret_like_remote_context(tmp_path: Path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_1", session_id="session_1")
    provider = MockModelProvider(text="ok")
    runtime = ModelRuntime.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRuntimeConfig(allow_remote_provider=True),
        trace=trace,
    )
    request = ModelTurnRequest(
        request_id="req_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="understanding_task",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        messages=[
            {
                "role": "user",
                "content": "OPENAI_API_KEY=sk-test should not leave",
            }
        ],
    )

    result = runtime.run_turn(request)

    assert result.status == ModelTurnStatus.INVALID
    assert provider.complete_calls == 0
    event_types = [event.event_type.value for event in trace.store.query_events()]
    assert "model.request.failed" in event_types


def test_model_runtime_build_request_from_context_uses_context_manager(tmp_path: Path) -> None:
    context = ContextManager(system_prompt="system", user_goal="inspect project")
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

    assert request.messages[0].role.value == "system"
    assert request.tools[0].name == "read_file"
    assert context.last_budget is not None


def test_model_runtime_respects_empty_allowed_tools_from_context(tmp_path: Path) -> None:
    context = ContextManager(system_prompt="system", user_goal="inspect project")
    provider = MockModelProvider(
        text="",
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_1",
                tool_name="read_file",
                arguments={"path": "README.md"},
                raw_arguments='{"path":"README.md"}',
                parse_status=ModelToolParseStatus.VALID,
            )
        ],
    )
    runtime = ModelRuntime.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))

    request = runtime.build_request_from_context(
        context,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="finalizing",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        allowed_tool_names=[],
    )
    result = runtime.run_turn(request)

    assert request.tools == []
    assert result.status == ModelTurnStatus.INVALID
    assert result.validation is not None
    assert "unknown_tool" in result.validation.errors
