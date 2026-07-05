from pathlib import Path

from singularity.context import ContextManager
from singularity.model import (
    MockModelProvider,
    ModelBudget,
    ModelCapabilities,
    ModelErrorKind,
    ModelMessage,
    ModelPurpose,
    ModelRole,
    ModelRunner,
    ModelRunnerConfig,
    ModelToolCall,
    ModelToolParseStatus,
    ModelTurnRequest,
    ModelTurnStatus,
    ModelUsage,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.observability import TraceRecorder
from singularity.tools import ToolRegistry


def test_model_runner_success_tool_call_trace_and_budget(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
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
    component = ModelRunner.with_mock_provider(
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

    result = component.run_turn(request)

    assert result.status == ModelTurnStatus.SUCCESS
    assert result.tool_calls[0].tool_name == "read_file"
    assert provider.complete_calls == 1
    event_types = [event.event_type.value for event in trace.store.query_events()]
    assert "model.request.created" in event_types
    assert "model.tool_call.proposed" in event_types


def test_model_runner_invalid_tool_call_does_not_execute_provider_result(tmp_path: Path) -> None:
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
    component = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))

    result = component.run_turn(
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


def test_model_runner_blocks_secret_like_remote_context(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
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

    result = component.run_turn(request)

    assert result.status == ModelTurnStatus.INVALID
    assert provider.complete_calls == 0
    event_types = [event.event_type.value for event in trace.store.query_events()]
    assert "model.request.failed" in event_types


def test_model_runner_allows_env_filename_safety_instruction(tmp_path: Path) -> None:
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
    )

    result = component.run_turn(
        ModelTurnRequest(
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
                    "content": "Do not read, print, or modify .env files or API keys.",
                }
            ],
        )
    )

    assert result.status == ModelTurnStatus.SUCCESS
    assert provider.complete_calls == 1


def test_model_runner_allows_structured_redacted_env_status(tmp_path: Path) -> None:
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
    )

    result = component.run_turn(
        ModelTurnRequest(
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
                    "content": {
                        "env_status": {
                            "SINGULARITY_API_KEY": "present_redacted",
                            "SINGULARITY_MODEL": "present",
                        }
                    },
                }
            ],
        )
    )

    assert result.status == ModelTurnStatus.SUCCESS
    assert provider.complete_calls == 1


def test_model_runner_allows_redacted_env_assignment_status(tmp_path: Path) -> None:
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
    )

    result = component.run_turn(
        ModelTurnRequest(
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
                    "content": (
                        "SINGULARITY_API_KEY=present(redacted)\n"
                        "SINGULARITY_MODEL=present"
                    ),
                }
            ],
        )
    )

    assert result.status == ModelTurnStatus.SUCCESS
    assert provider.complete_calls == 1


def test_model_runner_allows_placeholder_env_assignment_examples(tmp_path: Path) -> None:
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
    )

    result = component.run_turn(
        ModelTurnRequest(
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
                    "content": (
                        "SINGULARITY_API_KEY=your-api-key\n"
                        "export SINGULARITY_TOKEN=<your-token>"
                    ),
                }
            ],
        )
    )

    assert result.status == ModelTurnStatus.SUCCESS
    assert provider.complete_calls == 1


def test_model_runner_blocks_unredacted_env_assignment(tmp_path: Path) -> None:
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
    )

    result = component.run_turn(
        ModelTurnRequest(
            request_id="req_1",
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            phase_id="understanding_task",
            action_id="action_1",
            purpose=ModelPurpose.PLAN_NEXT_ACTION,
            messages=[{"role": "user", "content": "FOO=bar"}],
        )
    )

    assert result.status == ModelTurnStatus.INVALID
    assert provider.complete_calls == 0
    assert result.validation is not None
    assert "context_export_policy_env_content" in result.validation.errors


def test_model_runner_blocks_high_entropy_env_token_without_leaking_trace(
    tmp_path: Path,
) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
        trace=trace,
    )

    result = component.run_turn(
        ModelTurnRequest(
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
                    "content": "TOKEN=ghp_abcdefghijklmnopqrstuvwxyz123456",
                }
            ],
        )
    )

    failed = next(
        event
        for event in trace.store.query_events()
        if event.event_type.value == "model.request.failed"
    )

    assert result.status == ModelTurnStatus.INVALID
    assert provider.complete_calls == 0
    assert result.validation is not None
    assert "context_export_policy_secret_like_content" in result.validation.errors
    assert "ghp_abcdefghijklmnopqrstuvwxyz123456" not in str(failed.to_dict())


def test_model_runner_blocks_lowercase_secret_assignment(tmp_path: Path) -> None:
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
    )

    result = component.run_turn(
        ModelTurnRequest(
            request_id="req_1",
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            phase_id="understanding_task",
            action_id="action_1",
            purpose=ModelPurpose.PLAN_NEXT_ACTION,
            messages=[{"role": "user", "content": "password=raw-secret-value"}],
        )
    )

    assert result.status == ModelTurnStatus.INVALID
    assert provider.complete_calls == 0
    assert result.validation is not None
    assert "context_export_policy_secret_like_content" in result.validation.errors


def test_model_runner_blocks_inline_unredacted_secret_assignment(tmp_path: Path) -> None:
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
    )

    result = component.run_turn(
        ModelTurnRequest(
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
                    "content": "Provider config says OPENAI_API_KEY=abc123.",
                }
            ],
        )
    )

    assert result.status == ModelTurnStatus.INVALID
    assert provider.complete_calls == 0
    assert result.validation is not None
    assert "context_export_policy_secret_like_content" in result.validation.errors


def test_model_runner_reports_redacted_export_policy_diagnostics(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    provider = MockModelProvider(text="ok")
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        config=ModelRunnerConfig(allow_remote_provider=True),
        trace=trace,
    )

    result = component.run_turn(
        ModelTurnRequest(
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
                    "content": "OPENAI_API_KEY=sk-secret-value should not leave",
                }
            ],
        )
    )

    failed = next(
        event
        for event in trace.store.query_events()
        if event.event_type.value == "model.request.failed"
    )
    diagnostics = failed.payload["error"]["metadata"]["context_export_diagnostics"]

    assert result.status == ModelTurnStatus.INVALID
    assert provider.complete_calls == 0
    assert diagnostics["rule"] == "secret_like_content"
    assert diagnostics["message_index"] == 0
    assert diagnostics["role"] == "user"
    assert "text_hash" in diagnostics
    assert "sk-secret-value" not in str(failed.to_dict())


def test_model_runner_build_request_from_context_uses_context_manager(tmp_path: Path) -> None:
    context = ContextManager(system_prompt="system", user_goal="inspect project")
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

    assert request.messages[0].role.value == "system"
    assert request.tools[0].name == "read_file"
    assert context.last_budget is not None


def test_model_runner_respects_empty_allowed_tools_from_context(tmp_path: Path) -> None:
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
    component = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))

    request = component.build_request_from_context(
        context,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="finalizing",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        allowed_tool_names=[],
    )
    result = component.run_turn(request)

    assert request.tools == []
    assert result.status == ModelTurnStatus.INVALID
    assert result.validation is not None
    assert "unknown_tool" in result.validation.errors


def test_model_runner_records_cache_hit_ratio_in_result_and_trace(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    provider = MockModelProvider(
        text="ok",
        usage=ModelUsage(input_tokens=100, output_tokens=5, cached_input_tokens=75),
    )
    component = ModelRunner.with_mock_provider(
        provider,
        tool_registry=ToolRegistry(tmp_path),
        trace=trace,
    )

    result = component.run_turn(
        ModelTurnRequest(
            request_id="req_cache",
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            phase_id="phase",
            action_id="action",
            purpose=ModelPurpose.PLAN_NEXT_ACTION,
            messages=[{"role": "user", "content": "hello"}],
            context_metadata={
                "stable_prefix_hash": "stable",
                "dynamic_tail_hash": "tail",
                "tool_schema_hash": "tools",
                "context_shape_hash": "shape",
                "context_ordering_hash": "order",
            },
        )
    )

    assert result.metadata["cache"]["cache_hit_ratio"] == 0.75
    response = next(
        event
        for event in trace.store.query_events()
        if event.event_type.value == "model.response.received"
    )
    assert response.payload["cache"]["cached_input_tokens"] == 75
    assert response.payload["cache"]["cache_attribution"]["source"] == "provider_native"


def test_model_runner_marks_cache_miss_reason_for_tool_schema_change(tmp_path: Path) -> None:
    provider = MockModelProvider(
        text="ok",
        usage=ModelUsage(input_tokens=100, output_tokens=5, cached_input_tokens=0),
    )
    component = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))

    def request(request_id: str, tool_hash: str) -> ModelTurnRequest:
        return ModelTurnRequest(
            request_id=request_id,
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            phase_id="phase",
            action_id=request_id,
            purpose=ModelPurpose.PLAN_NEXT_ACTION,
            messages=[{"role": "user", "content": "hello"}],
            context_metadata={
                "stable_prefix_hash": "stable",
                "dynamic_tail_hash": "tail",
                "tool_schema_hash": tool_hash,
                "context_shape_hash": "shape",
                "context_ordering_hash": "order",
            },
        )

    first = component.run_turn(request("req_1", "tools_a"))
    second = component.run_turn(request("req_2", "tools_b"))

    assert first.metadata["cache_miss_reasons"] == ["first_request"]
    assert "tool_schema_change" in second.metadata["cache_miss_reasons"]
    assert second.metadata["cache"]["cache_attribution"]["source"] == "component_inferred"


def test_model_runner_downgrades_json_stream_and_parallel_tool_preferences(
    tmp_path: Path,
) -> None:
    provider = MockModelProvider(
        text="ok",
        capabilities=ModelCapabilities(
            supports_tools=True,
            supports_streaming=False,
            supports_json_mode=False,
            supports_parallel_tool_calls=False,
            supports_developer_message=True,
        ),
    )
    component = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))
    request = ModelTurnRequest(
        request_id="req_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        messages=[ModelMessage.assistant_text("context")],
        tools=[],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.AUTO, max_tool_calls=4),
    )
    request.model_preferences.json_mode = True
    request.model_preferences.stream = True

    result = component.run_turn(request)

    assert result.status == ModelTurnStatus.SUCCESS
    sent = provider.requests[0]
    assert sent.preferences.json_mode is False
    assert sent.preferences.stream is False
    assert sent.tool_choice.max_tool_calls == 1
    assert result.metadata["capability_adjustments"]["downgraded"] == [
        "json_mode",
        "streaming",
        "parallel_tool_calls",
    ]


def test_model_runner_downgrades_structured_outputs_when_provider_does_not_support_it(
    tmp_path: Path,
) -> None:
    provider = MockModelProvider(
        text='{"findings":[]}',
        capabilities=ModelCapabilities(
            supports_tools=True,
            supports_json_mode=True,
            supports_structured_outputs=False,
        ),
    )
    component = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))
    request = ModelTurnRequest(
        request_id="req_structured",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        purpose=ModelPurpose.FINAL_REVIEW,
        messages=[],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE, max_tool_calls=1),
    )
    request.model_preferences.structured_output_schema = {
        "name": "review_findings",
        "strict": True,
        "schema": {"type": "object", "additionalProperties": False},
    }

    result = component.run_turn(request)

    assert result.status == ModelTurnStatus.SUCCESS
    assert provider.requests[0].preferences.structured_output_schema is None
    assert result.metadata["capability_adjustments"]["downgraded"] == ["structured_outputs"]


def test_model_runner_returns_structured_capability_error_when_tools_required(
    tmp_path: Path,
) -> None:
    provider = MockModelProvider(
        text="ok",
        capabilities=ModelCapabilities(supports_tools=False),
    )
    component = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))
    request = ModelTurnRequest(
        request_id="req_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        messages=[],
        tools=component.tool_renderer.render(allowed_tool_names=["read_file"]),
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.REQUIRED),
    )

    result = component.run_turn(request)

    assert result.status == ModelTurnStatus.FAILED
    assert provider.complete_calls == 0
    assert result.error is not None
    assert result.error.kind == ModelErrorKind.UNSUPPORTED_CAPABILITY
    assert result.error.metadata["capability"] == "tools"


def test_model_runner_folds_developer_messages_for_provider_without_support(
    tmp_path: Path,
) -> None:
    provider = MockModelProvider(
        text="ok",
        capabilities=ModelCapabilities(
            supports_tools=True,
            supports_system_message=True,
            supports_developer_message=False,
        ),
    )
    component = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))
    request = ModelTurnRequest(
        request_id="req_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        messages=[
            ModelMessage(
                role=ModelRole.DEVELOPER,
                content=[],
            )
        ],
    )

    result = component.run_turn(request)

    assert result.status == ModelTurnStatus.SUCCESS
    sent_message = provider.requests[0].messages[0]
    assert sent_message.role == ModelRole.SYSTEM
    assert sent_message.metadata["developer_fallback"] == "system"
