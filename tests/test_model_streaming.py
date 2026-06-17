from miniharness.model import (
    ModelToolParseStatus,
    ProviderStreamEvent,
    ProviderStreamEventType,
    StreamingAccumulator,
    MockModelProvider,
    ModelRuntime,
    ModelTurnRequest,
    ModelPurpose,
    ToolChoicePolicy,
    ToolChoiceMode,
)
from miniharness.tools import ToolRegistry
from miniharness.observability import TraceRuntime


def test_streaming_accumulator_buffers_text_and_tool_calls_without_executing() -> None:
    accumulator = StreamingAccumulator()
    accumulator.add(ProviderStreamEvent(type=ProviderStreamEventType.TEXT_DELTA, text_delta="he"))
    accumulator.add(ProviderStreamEvent(type=ProviderStreamEventType.TEXT_DELTA, text_delta="llo"))
    accumulator.add(
        ProviderStreamEvent(
            type=ProviderStreamEventType.TOOL_CALL_DELTA,
            tool_call_id="call_1",
            tool_name="read_file",
            arguments_delta='{"path":',
        )
    )
    accumulator.add(
        ProviderStreamEvent(
            type=ProviderStreamEventType.TOOL_CALL_COMPLETED,
            tool_call_id="call_1",
            arguments_delta=' "README.md"}',
        )
    )

    result = accumulator.to_response()

    assert result.message.content == "hello"
    assert result.tool_calls[0].parse_status == ModelToolParseStatus.VALID
    assert result.executed_tool_count == 0


def test_model_runtime_consumes_streaming_provider(tmp_path):
    trace = TraceRuntime.create(tmp_path, run_id="run_1", session_id="session_1")

    class _StreamingProvider(MockModelProvider):
        def __init__(self) -> None:
            super().__init__(text="", stream_events=[
                ProviderStreamEvent(type=ProviderStreamEventType.TEXT_DELTA, text_delta="he"),
                ProviderStreamEvent(type=ProviderStreamEventType.TEXT_DELTA, text_delta="llo"),
                ProviderStreamEvent(type=ProviderStreamEventType.TOOL_CALL_DELTA, tool_call_id="call_1", tool_name="read_file", arguments_delta='{"path":"README.md"}'),
                ProviderStreamEvent(type=ProviderStreamEventType.TOOL_CALL_COMPLETED, tool_call_id="call_1"),
                ProviderStreamEvent(type=ProviderStreamEventType.RESPONSE_COMPLETED),
            ])

        def complete(self, request):
            raise AssertionError("streaming path should not call complete()")

    provider = _StreamingProvider()
    provider._capabilities.supports_streaming = True
    runtime = ModelRuntime.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path), trace=trace)
    request = ModelTurnRequest(
        request_id="req_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        messages=[],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.AUTO),
    )
    request.model_preferences.stream = True

    result = runtime.run_turn(request)

    assert result.status.value == "success"
    assert result.assistant_message is not None
    assert result.assistant_message.text == "hello"
    assert result.tool_calls[0].tool_name == "read_file"
