from singularity.model import (
    ChatProviderModelProvider,
    ContentBlock,
    ContentBlockType,
    MessageConverter,
    ModelCapabilities,
    ModelMessage,
    ModelRole,
    ProviderRequest,
)
from singularity.model.openai_format import model_messages_to_openai


def test_message_converter_preserves_tool_call_id_and_developer_fallback() -> None:
    converter = MessageConverter()
    messages = [
        ModelMessage(role=ModelRole.SYSTEM, content=[ContentBlock.from_text("system")]),
        ModelMessage(role=ModelRole.DEVELOPER, content=[ContentBlock.from_text("dev")]),
        ModelMessage(
            role=ModelRole.TOOL,
            content=[ContentBlock(type=ContentBlockType.TOOL_RESULT, text="result")],
            name="read_file",
            tool_call_id="call_1",
        ),
    ]

    provider_messages = converter.to_provider_messages(
        messages,
        capabilities=ModelCapabilities(supports_developer_message=False),
    )

    assert provider_messages[1]["role"] == "system"
    assert provider_messages[1]["metadata"]["developer_fallback"] == "system"
    assert provider_messages[2]["tool_call_id"] == "call_1"
    restored = converter.from_provider_message({"role": "assistant", "content": "ok"})
    assert restored.role == ModelRole.ASSISTANT
    assert converter.estimate_tokens(messages) > 0


class _CapturingProvider:
    def __init__(self) -> None:
        self.messages = []

    def chat(self, *, messages, tools):
        self.messages = messages
        return {"choices": [{"message": {"role": "assistant", "content": "ok"}}]}


def test_legacy_chat_adapter_applies_developer_fallback_without_metadata_leak() -> None:
    provider = _CapturingProvider()
    adapter = ChatProviderModelProvider(provider)

    adapter.complete(
        ProviderRequest(
            request_id="req",
            purpose="plan_next_action",
            messages=[
                ModelMessage(role=ModelRole.DEVELOPER, content=[ContentBlock.from_text("dev")])
            ],
        )
    )

    assert provider.messages[0]["role"] == "system"
    assert "metadata" not in provider.messages[0]


def test_provider_messages_use_safe_empty_arguments_for_historical_tool_calls() -> None:
    message = ModelMessage(
        role=ModelRole.ASSISTANT,
        content=[ContentBlock.from_text("")],
        metadata={
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read_file"},
                }
            ]
        },
    )

    provider_messages = model_messages_to_openai(
        [message],
        ModelCapabilities(supports_tools=True),
    )

    assert provider_messages[0]["tool_calls"] == [
        {
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"},
        }
    ]
