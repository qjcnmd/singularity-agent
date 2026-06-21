from singularity.model import (
    ContentBlock,
    ContentBlockType,
    ModelBudget,
    ModelMessage,
    ModelPurpose,
    ModelRole,
    ModelToolCall,
    ModelToolParseStatus,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
    ModelUsage,
    ToolChoiceMode,
    ToolChoicePolicy,
)


def test_model_core_objects_round_trip() -> None:
    message = ModelMessage(
        role=ModelRole.USER,
        content=[ContentBlock(type=ContentBlockType.TEXT, text="hello")],
        metadata={"source": "test"},
    )
    tool_call = ModelToolCall(
        tool_call_id="call_1",
        tool_name="read_file",
        arguments={"path": "README.md"},
        raw_arguments='{"path":"README.md"}',
        parse_status=ModelToolParseStatus.VALID,
    )
    request = ModelTurnRequest(
        request_id="req_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="understanding_task",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        messages=[message],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.AUTO),
        budget=ModelBudget(max_input_tokens=1000),
    )
    result = ModelTurnResult(
        request_id=request.request_id,
        response_id="resp_1",
        status=ModelTurnStatus.SUCCESS,
        assistant_message=ModelMessage.assistant_text("ok"),
        tool_calls=[tool_call],
        usage=ModelUsage(input_tokens=5, output_tokens=2),
        provider_name="mock",
        model_name="mock-model",
    )

    assert ModelTurnRequest.from_dict(request.to_dict()).purpose == ModelPurpose.PLAN_NEXT_ACTION
    restored = ModelTurnResult.from_dict(result.to_dict())
    assert restored.tool_calls[0].tool_name == "read_file"
    assert restored.usage.total_tokens == 7

