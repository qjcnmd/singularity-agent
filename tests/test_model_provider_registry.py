import pytest
import httpx

from singularity.config import Settings
from singularity.model import (
    MockModelProvider,
    ModelCapabilities,
    ModelCapabilityError,
    ModelPreferences,
    ModelMessage,
    ModelRole,
    OpenAICompatibleModelProvider,
    ModelProviderNotFound,
    ModelProviderRegistry,
    ProviderRequest,
    ToolChoiceMode,
    ToolChoicePolicy,
    ContentBlock,
)


def test_registry_selects_default_provider_and_checks_capabilities() -> None:
    provider = MockModelProvider(
        provider_name="mock",
        model_name="mock-model",
        capabilities=ModelCapabilities(
            supports_tools=True,
            supports_streaming=False,
            supports_json_mode=False,
        ),
    )
    registry = ModelProviderRegistry(default_provider_name="mock")
    registry.register(provider)

    selected = registry.select_provider(ModelPreferences(provider_name=None), purpose=None)

    assert selected.name() == "mock"
    assert registry.default_provider().name() == "mock"
    registry.check_capabilities(selected, requires_tools=True)
    with pytest.raises(ModelCapabilityError):
        registry.check_capabilities(selected, requires_streaming=True)
    with pytest.raises(ModelProviderNotFound):
        registry.get("missing")


def test_registry_summarizes_provider_capabilities_without_private_payload() -> None:
    provider = MockModelProvider(
        provider_name="mock",
        capabilities=ModelCapabilities(
            supports_tools=False,
            supports_streaming=False,
            supports_json_mode=False,
            supports_parallel_tool_calls=False,
            supports_developer_message=False,
        ),
    )

    summary = ModelProviderRegistry.provider_capability_summary(provider)

    assert summary == {
        "provider": "mock",
        "supports_tools": False,
        "supports_parallel_tool_calls": False,
        "supports_streaming": False,
        "supports_json_mode": False,
        "supports_system_message": True,
        "supports_developer_message": False,
        "max_context_tokens": 128000,
        "max_output_tokens": 4096,
    }


class _FakeResponse:
    status_code = 200
    text = "{}"

    def raise_for_status(self) -> None:
        return None

    def json(self) -> dict:
        return {
            "id": "resp_1",
            "model": "test-model",
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
        }


class _FakeToolResponse(_FakeResponse):
    def json(self) -> dict:
        return {
            "id": "resp_2",
            "model": "test-model",
            "choices": [
                {
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "read_file", "arguments": '{"path":"README.md"}'},
                            }
                        ],
                    },
                }
            ],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
        }


class _FakeErrorResponse:
    status_code = 500
    text = "provider echoed OPENAI_API_KEY=sk-leaked"

    def raise_for_status(self) -> None:
        request = httpx.Request("POST", "https://example.test/v1/chat/completions")
        response = httpx.Response(self.status_code, request=request, text=self.text)
        raise httpx.HTTPStatusError("server error", request=request, response=response)


class _FakeClient:
    payloads: list[dict] = []

    def __init__(self, *, timeout: float) -> None:
        self.timeout = timeout

    def __enter__(self) -> "_FakeClient":
        return self

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        return None

    def post(self, url: str, *, headers: dict[str, str], json: dict) -> _FakeResponse:
        self.payloads.append(json)
        return _FakeResponse()


class _FakeToolClient(_FakeClient):
    def post(self, url: str, *, headers: dict[str, str], json: dict) -> _FakeToolResponse:
        self.payloads.append(json)
        return _FakeToolResponse()


class _FakeErrorClient(_FakeClient):
    def post(self, url: str, *, headers: dict[str, str], json: dict) -> _FakeErrorResponse:
        self.payloads.append(json)
        return _FakeErrorResponse()


def test_openai_compatible_model_provider_serializes_runtime_request(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _FakeClient.payloads = []
    monkeypatch.setattr("singularity.model.providers.httpx.Client", _FakeClient)
    provider = OpenAICompatibleModelProvider(
        Settings(base_url="https://example.test/v1", api_key="test-key", model="test-model")
    )

    response = provider.complete(
        ProviderRequest(
            request_id="req_1",
            purpose="plan_next_action",
            messages=[
                ModelMessage(
                    role=ModelRole.USER,
                    content=[ContentBlock.from_text("hi")],
                )
            ],
            tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE),
        )
    )

    assert response.message.text == "ok"
    assert response.usage.total_tokens == 5
    assert _FakeClient.payloads[0]["tool_choice"] == "none"


def test_openai_compatible_provider_keeps_parallel_tool_compatibility() -> None:
    provider = OpenAICompatibleModelProvider(
        Settings(base_url="https://example.test/v1", api_key="test-key", model="test-model")
    )

    assert provider.capabilities().supports_parallel_tool_calls is True
    assert provider.capabilities().supports_streaming is True


def test_openai_compatible_provider_streams_complete_response_fallback(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("singularity.model.providers.httpx.Client", _FakeToolClient)
    provider = OpenAICompatibleModelProvider(
        Settings(base_url="https://example.test/v1", api_key="test-key", model="test-model")
    )

    events = list(
        provider.stream(
            ProviderRequest(
                request_id="req_1",
                purpose="plan_next_action",
                messages=[
                    ModelMessage(
                        role=ModelRole.USER,
                        content=[ContentBlock.from_text("hi")],
                    )
                ],
            )
        )
    )

    assert [event.type.value for event in events] == [
        "tool_call_delta",
        "tool_call_completed",
        "usage_delta",
        "response_completed",
    ]
    assert events[0].tool_name == "read_file"
    assert events[0].arguments_delta == '{"path":"README.md"}'


def test_openai_provider_error_does_not_include_response_body(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("singularity.model.providers.httpx.Client", _FakeErrorClient)
    provider = OpenAICompatibleModelProvider(
        Settings(base_url="https://example.test/v1", api_key="test-key", model="test-model")
    )

    with pytest.raises(Exception) as exc_info:
        provider.complete(
            ProviderRequest(
                request_id="req_1",
                purpose="plan_next_action",
                messages=[
                    ModelMessage(
                        role=ModelRole.USER,
                        content=[ContentBlock.from_text("hi")],
                    )
                ],
            )
        )

    assert "sk-leaked" not in str(exc_info.value)
    assert "OPENAI_API_KEY" not in str(exc_info.value)
