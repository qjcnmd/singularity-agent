from pathlib import Path

from singularity.model import (
    ModelMessage,
    ModelResponseValidator,
    ModelToolCall,
    ModelToolParseStatus,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.tools import ToolRegistry


def test_response_validator_enforces_tool_choice_and_tool_call_rules(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    validator = ModelResponseValidator(registry)
    call = ModelToolCall(
        tool_call_id="call_1",
        tool_name="read_file",
        arguments={"path": "README.md"},
        raw_arguments='{"path":"README.md"}',
        parse_status=ModelToolParseStatus.VALID,
    )

    none_result = validator.validate(
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[call],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE),
        allowed_tool_names=["read_file"],
    )
    assert not none_result.valid
    assert "tool_choice_none" in none_result.errors

    required_result = validator.validate(
        assistant_message=ModelMessage.assistant_text("no tool"),
        tool_calls=[],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.REQUIRED),
        allowed_tool_names=["read_file"],
    )
    assert not required_result.valid
    assert "tool_choice_required" in required_result.errors

    duplicate_result = validator.validate(
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[call, call],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.AUTO),
        allowed_tool_names=["read_file"],
    )
    assert "duplicate_tool_call_id" in duplicate_result.errors

    empty_result = validator.validate(
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[],
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.AUTO),
        allowed_tool_names=["read_file"],
    )
    assert "empty_response" in empty_result.errors

