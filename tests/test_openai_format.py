from singularity.model.models import (
    ContentBlock,
    ModelCapabilities,
    ModelMessage,
    ModelRole,
    ModelToolSchema,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.model.openai_format import (
    model_messages_to_openai,
    model_tool_to_openai,
    serialize_tool_choice,
)


def test_model_messages_to_openai_preserves_existing_wire_shape() -> None:
    messages = [
        ModelMessage(
            role=ModelRole.DEVELOPER,
            content=[ContentBlock.from_text("inspect the workspace")],
            metadata={
                "internal_trace": "not-provider-visible",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": {"path": "README.md"},
                        },
                    }
                ],
            },
        )
    ]

    payloads = model_messages_to_openai(
        messages,
        ModelCapabilities(supports_developer_message=False, supports_system_message=True),
    )

    assert payloads == [
        {
            "role": "system",
            "content": "inspect the workspace",
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": '{"path": "README.md"}',
                    },
                }
            ],
        }
    ]


def test_model_tool_to_openai_uses_strict_metadata_and_explicit_override() -> None:
    tool = ModelToolSchema(
        name="read_file",
        description="Read a file",
        parameters_schema={"type": "object"},
        metadata={"strict": True},
    )

    assert model_tool_to_openai(tool) == {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file",
            "parameters": {"type": "object"},
            "strict": True,
        },
    }
    assert "strict" not in model_tool_to_openai(tool, strict=False)["function"]


def test_serialize_tool_choice_preserves_openai_compatible_values() -> None:
    assert serialize_tool_choice(ToolChoiceMode.NONE) == "none"
    assert serialize_tool_choice(
        ToolChoicePolicy(mode=ToolChoiceMode.SPECIFIC_TOOL, tool_name="read_file")
    ) == {"type": "function", "function": {"name": "read_file"}}
    assert serialize_tool_choice(ToolChoicePolicy(mode=ToolChoiceMode.ALLOWED_TOOLS)) == "auto"
