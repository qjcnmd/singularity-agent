import importlib.util
from typing import Any, ClassVar

import httpx
import pytest

from singularity.config import Settings
from singularity.model import (
    ContentBlock,
    ModelMessage,
    ModelPurpose,
    ModelRole,
    OpenAICompatibleModelProvider,
    ProviderRequest,
    ToolChoiceMode,
    ToolChoicePolicy,
)


class FakeResponse:
    status_code = 200
    text = "{}"

    def raise_for_status(self) -> None:
        return None

    def json(self) -> dict[str, Any]:
        return {"choices": [{"message": {"role": "assistant", "content": "ok"}}]}


class FakeClient:
    payloads: ClassVar[list[dict[str, Any]]] = []

    def __init__(self, *, timeout: float) -> None:
        self.timeout = timeout

    def __enter__(self) -> "FakeClient":
        return self

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        return None

    def post(
        self,
        url: str,
        *,
        headers: dict[str, str],
        json: dict[str, Any],
    ) -> FakeResponse:
        self.payloads.append(json)
        return FakeResponse()


class FakeErrorResponse:
    status_code = 401
    text = '{"error":"OPENAI_API_KEY=sk-secret-provider-body"}'

    def raise_for_status(self) -> None:
        request = httpx.Request("POST", "https://example.test/v1/chat/completions")
        response = httpx.Response(self.status_code, text=self.text, request=request)
        raise httpx.HTTPStatusError("auth failed", request=request, response=response)


class FakeErrorClient(FakeClient):
    def post(
        self,
        url: str,
        *,
        headers: dict[str, str],
        json: dict[str, Any],
    ) -> FakeErrorResponse:
        self.payloads.append(json)
        return FakeErrorResponse()


@pytest.mark.parametrize(
    ("mode", "expected"),
    [
        (ToolChoiceMode.AUTO, "auto"),
        (ToolChoiceMode.REQUIRED, "required"),
        (ToolChoiceMode.NONE, "none"),
    ],
)
def test_provider_chat_passes_tool_choice(
    monkeypatch: pytest.MonkeyPatch,
    mode: ToolChoiceMode,
    expected: str,
) -> None:
    FakeClient.payloads = []
    monkeypatch.setattr("singularity.model.providers.httpx.Client", FakeClient)
    provider = OpenAICompatibleModelProvider(
        Settings(
            base_url="https://example.test/v1",
            api_key="test-key",
            model="test-model",
        )
    )

    provider.complete(
        ProviderRequest(
            request_id="req",
            purpose=ModelPurpose.PLAN_NEXT_ACTION.value,
            messages=[
                ModelMessage(
                    role=ModelRole.USER,
                    content=[ContentBlock.from_text("hi")],
                )
            ],
            tool_choice=ToolChoicePolicy(mode=mode),
        )
    )

    assert FakeClient.payloads[0]["tool_choice"] == expected


def test_provider_http_error_does_not_echo_response_body(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    FakeErrorClient.payloads = []
    monkeypatch.setattr("singularity.model.providers.httpx.Client", FakeErrorClient)
    provider = OpenAICompatibleModelProvider(
        Settings(
            base_url="https://example.test/v1",
            api_key="test-key",
            model="test-model",
        )
    )

    with pytest.raises(Exception) as exc:
        provider.complete(
            ProviderRequest(
                request_id="req",
                purpose=ModelPurpose.PLAN_NEXT_ACTION.value,
                messages=[
                    ModelMessage(
                        role=ModelRole.USER,
                        content=[ContentBlock.from_text("hi")],
                    )
                ],
            )
        )

    message = str(exc.value)
    assert "HTTP 401" in message
    assert "sk-secret-provider-body" not in message
    assert "OPENAI_API_KEY" not in message


def test_legacy_provider_module_is_removed() -> None:
    assert importlib.util.find_spec("singularity.provider") is None
