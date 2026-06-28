from singularity.model import (
    MockModelProvider,
    ModelPurpose,
    ModelRunner,
    ModelToolParseStatus,
    ModelTurnRequest,
    ProviderStreamEvent,
    ProviderStreamEventType,
    StreamingAccumulator,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.observability import TraceRecorder
from singularity.tools import ToolRegistry


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


def test_model_runner_consumes_streaming_provider(tmp_path):
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")

    class _StreamingProvider(MockModelProvider):
        def __init__(self) -> None:
            super().__init__(
                text="",
                stream_events=[
                    ProviderStreamEvent(type=ProviderStreamEventType.TEXT_DELTA, text_delta="he"),
                    ProviderStreamEvent(type=ProviderStreamEventType.TEXT_DELTA, text_delta="llo"),
                    ProviderStreamEvent(
                        type=ProviderStreamEventType.TOOL_CALL_DELTA,
                        tool_call_id="call_1",
                        tool_name="read_file",
                        arguments_delta='{"path":"README.md"}',
                    ),
                    ProviderStreamEvent(type=ProviderStreamEventType.TOOL_CALL_COMPLETED, tool_call_id="call_1"),
                    ProviderStreamEvent(
                        type=ProviderStreamEventType.USAGE_DELTA,
                        usage_delta={
                            "input_tokens": 3,
                            "output_tokens": 4,
                            "total_tokens": 7,
                            "cached_input_tokens": 1,
                            "reasoning_tokens": 2,
                        },
                    ),
                    ProviderStreamEvent(type=ProviderStreamEventType.RESPONSE_COMPLETED),
                ],
            )

        def complete(self, request):
            raise AssertionError("streaming path should not call complete()")

    provider = _StreamingProvider()
    provider._capabilities.supports_streaming = True
    component = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path), trace=trace)
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

    result = component.run_turn(request)

    assert result.status.value == "success"
    assert result.assistant_message is not None
    assert result.assistant_message.text == "hello"
    assert result.tool_calls[0].tool_name == "read_file"
    assert result.usage.input_tokens == 3
    assert result.usage.cached_input_tokens == 1
