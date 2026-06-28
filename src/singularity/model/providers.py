from __future__ import annotations

import inspect
import json
from collections.abc import Iterable
from dataclasses import dataclass, field
from typing import Any, Protocol
from uuid import uuid4

import httpx

from singularity.config import Settings
from singularity.model.messages import MessageConverter
from singularity.model.models import (
    ContentBlock,
    ModelCapabilities,
    ModelError,
    ModelErrorKind,
    ModelMessage,
    ModelPreferences,
    ModelRole,
    ModelToolCall,
    ModelToolParseStatus,
    ModelToolSchema,
    ModelUsage,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.model.streaming import ProviderStreamEvent, ProviderStreamEventType


@dataclass
class ProviderRequest:
    request_id: str
    purpose: str
    messages: list[ModelMessage]
    tools: list[ModelToolSchema] = field(default_factory=list)
    tool_choice: ToolChoicePolicy = field(default_factory=ToolChoicePolicy)
    preferences: ModelPreferences = field(default_factory=ModelPreferences)
    policy_metadata: dict[str, Any] = field(default_factory=dict)
    trace_metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class ProviderResponse:
    response_id: str
    message: ModelMessage
    tool_calls: list[ModelToolCall] = field(default_factory=list)
    usage: ModelUsage = field(default_factory=ModelUsage)
    finish_reason: str | None = None
    provider_name: str | None = None
    model_name: str | None = None
    raw_response: dict[str, Any] | None = None

    @classmethod
    def from_openai_response(
        cls,
        payload: dict[str, Any],
        *,
        provider_name: str,
        model_name: str | None,
    ) -> ProviderResponse:
        choices = payload.get("choices") or []
        choice = choices[0] if choices else {}
        message_payload = choice.get("message") or {}
        content = message_payload.get("content")
        message = ModelMessage(
            role=ModelRole.ASSISTANT,
            content=[ContentBlock.from_text("" if content is None else str(content))],
            metadata={
                key: value
                for key, value in message_payload.items()
                if key not in {"role", "content", "tool_calls"}
            },
        )
        tool_calls = [
            _raw_provider_tool_call(tool_call)
            for tool_call in (message_payload.get("tool_calls") or [])
        ]
        usage_payload = payload.get("usage") or {}
        prompt_details = usage_payload.get("prompt_tokens_details") or {}
        completion_details = usage_payload.get("completion_tokens_details") or {}
        usage = ModelUsage(
            input_tokens=int(usage_payload.get("prompt_tokens") or 0),
            output_tokens=int(usage_payload.get("completion_tokens") or 0),
            total_tokens=int(usage_payload.get("total_tokens") or 0),
            cached_input_tokens=int(prompt_details.get("cached_tokens") or 0),
            reasoning_tokens=int(completion_details.get("reasoning_tokens") or 0),
        )
        return cls(
            response_id=str(payload.get("id") or f"model_resp_{uuid4().hex[:12]}"),
            message=message,
            tool_calls=tool_calls,
            usage=usage,
            finish_reason=choice.get("finish_reason"),
            provider_name=provider_name,
            model_name=str(payload.get("model") or model_name or ""),
            raw_response=payload,
        )


class ModelProvider(Protocol):
    def name(self) -> str:
        ...

    def capabilities(self) -> ModelCapabilities:
        ...

    def complete(self, request: ProviderRequest) -> ProviderResponse:
        ...

    def stream(self, request: ProviderRequest) -> Iterable[ProviderStreamEvent]:
        ...


class MockModelProvider:
    def __init__(
        self,
        *,
        provider_name: str = "mock",
        model_name: str = "mock-model",
        text: str = "",
        tool_calls: list[ModelToolCall] | None = None,
        capabilities: ModelCapabilities | None = None,
        usage: ModelUsage | None = None,
        error: ModelError | Exception | None = None,
        stream_events: list[ProviderStreamEvent] | None = None,
    ) -> None:
        self.provider_name = provider_name
        self.model_name = model_name
        self.text = text
        self.tool_calls = tool_calls or []
        self._capabilities = capabilities or ModelCapabilities(
            supports_tools=True,
            supports_streaming=bool(stream_events),
            supports_json_mode=True,
            supports_parallel_tool_calls=True,
            supports_developer_message=True,
        )
        self.usage = usage or ModelUsage(input_tokens=1, output_tokens=len(text.split()))
        self.error = error
        self.stream_events = stream_events or []
        self.complete_calls = 0
        self.stream_calls = 0
        self.requests: list[ProviderRequest] = []

    def name(self) -> str:
        return self.provider_name

    def capabilities(self) -> ModelCapabilities:
        return self._capabilities

    def complete(self, request: ProviderRequest) -> ProviderResponse:
        self.complete_calls += 1
        self.requests.append(request)
        if self.error is not None:
            if isinstance(self.error, ModelError):
                raise self.error
            raise self.error
        return ProviderResponse(
            response_id=f"mock_resp_{self.complete_calls}",
            message=ModelMessage.assistant_text(self.text),
            tool_calls=list(self.tool_calls),
            usage=self.usage,
            finish_reason="tool_calls" if self.tool_calls else "stop",
            provider_name=self.provider_name,
            model_name=request.preferences.model_name or self.model_name,
            raw_response=None,
        )

    def stream(self, request: ProviderRequest) -> Iterable[ProviderStreamEvent]:
        self.stream_calls += 1
        yield from self.stream_events


class ChatProviderModelProvider:
    """Adapter for the earlier Provider.chat(messages, tools) contract."""

    def __init__(
        self,
        provider: Any,
        *,
        provider_name: str = "legacy_chat",
        model_name: str | None = None,
        capabilities: ModelCapabilities | None = None,
    ) -> None:
        self.provider = provider
        self.provider_name = provider_name
        self.model_name = model_name
        self._capabilities = capabilities or ModelCapabilities(supports_tools=True)

    def name(self) -> str:
        return self.provider_name

    def capabilities(self) -> ModelCapabilities:
        return self._capabilities

    def complete(self, request: ProviderRequest) -> ProviderResponse:
        messages = _model_messages_to_openai(request.messages, self.capabilities())
        tools = [_model_tool_to_openai(tool) for tool in request.tools]
        kwargs: dict[str, Any] = {"messages": messages, "tools": tools}
        if _chat_accepts_tool_choice(self.provider):
            kwargs["tool_choice"] = _serialize_tool_choice(request.tool_choice)
        response = self.provider.chat(**kwargs)
        return ProviderResponse.from_openai_response(
            response,
            provider_name=self.provider_name,
            model_name=self.model_name,
        )

    def stream(self, request: ProviderRequest) -> Iterable[ProviderStreamEvent]:
        raise ModelError(
            kind=ModelErrorKind.UNSUPPORTED_CAPABILITY,
            message="Legacy chat provider does not support streaming.",
            retryable=False,
            provider_name=self.provider_name,
            model_name=self.model_name,
        )


class OpenAICompatibleModelProvider:
    def __init__(
        self,
        settings: Settings,
        *,
        provider_name: str = "openai_compatible",
        timeout_seconds: float = 60.0,
        capabilities: ModelCapabilities | None = None,
    ) -> None:
        self.settings = settings
        self.provider_name = provider_name
        self.timeout_seconds = timeout_seconds
        self._capabilities = capabilities or ModelCapabilities(
            supports_tools=True,
            supports_parallel_tool_calls=True,
            supports_streaming=True,
            supports_json_mode=True,
            supports_developer_message=False,
        )

    def name(self) -> str:
        return self.provider_name

    def capabilities(self) -> ModelCapabilities:
        return self._capabilities

    def complete(self, request: ProviderRequest) -> ProviderResponse:
        payload = self._chat_completion_payload(request)
        headers = self._headers()
        try:
            with httpx.Client(timeout=self.timeout_seconds) as client:
                response = client.post(self._chat_completions_url(), headers=headers, json=payload)
            response.raise_for_status()
        except httpx.TimeoutException as exc:
            raise ModelError(
                kind=ModelErrorKind.TIMEOUT,
                message=str(exc),
                retryable=True,
                provider_name=self.provider_name,
                model_name=str(payload["model"]),
            ) from exc
        except httpx.HTTPStatusError as exc:
            status = exc.response.status_code
            raise ModelError(
                kind=_error_kind_for_status(status),
                message=f"Provider returned HTTP {status}.",
                retryable=status in {408, 409, 429, 500, 502, 503, 504},
                provider_name=self.provider_name,
                model_name=str(payload["model"]),
                metadata={"http_status": status},
            ) from exc
        except httpx.RequestError as exc:
            retryable = not _request_error_is_permission_denied(exc)
            raise ModelError(
                kind=ModelErrorKind.NETWORK_ERROR,
                message=str(exc),
                retryable=retryable,
                provider_name=self.provider_name,
                model_name=str(payload["model"]),
            ) from exc

        return ProviderResponse.from_openai_response(
            response.json(),
            provider_name=self.provider_name,
            model_name=str(payload["model"]),
        )

    def stream(self, request: ProviderRequest) -> Iterable[ProviderStreamEvent]:
        payload = {
            **self._chat_completion_payload(request),
            "stream": True,
            "stream_options": {"include_usage": True},
        }
        headers = self._headers()
        model_name = str(payload["model"])
        try:
            with (
                httpx.Client(timeout=self.timeout_seconds) as client,
                client.stream(
                    "POST",
                    self._chat_completions_url(),
                    headers=headers,
                    json=payload,
                ) as response,
            ):
                response.raise_for_status()
                state = _StreamParseState()
                for line in response.iter_lines():
                    yield from _events_from_sse_line(line, state=state)
        except httpx.TimeoutException as exc:
            raise ModelError(
                kind=ModelErrorKind.TIMEOUT,
                message=str(exc),
                retryable=True,
                provider_name=self.provider_name,
                model_name=model_name,
            ) from exc
        except httpx.HTTPStatusError as exc:
            status = exc.response.status_code
            raise ModelError(
                kind=_error_kind_for_status(status),
                message=f"Provider returned HTTP {status}.",
                retryable=status in {408, 409, 429, 500, 502, 503, 504},
                provider_name=self.provider_name,
                model_name=model_name,
                metadata={"http_status": status},
            ) from exc
        except httpx.RequestError as exc:
            retryable = not _request_error_is_permission_denied(exc)
            raise ModelError(
                kind=ModelErrorKind.NETWORK_ERROR,
                message=str(exc),
                retryable=retryable,
                provider_name=self.provider_name,
                model_name=model_name,
            ) from exc

    def _chat_completion_payload(self, request: ProviderRequest) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "model": request.preferences.model_name or self.settings.model,
            "messages": _model_messages_to_openai(
                request.messages,
                self.capabilities(),
            ),
            "tools": [_model_tool_to_openai(tool) for tool in request.tools],
            "tool_choice": _serialize_tool_choice(request.tool_choice),
        }
        if request.preferences.temperature is not None:
            payload["temperature"] = request.preferences.temperature
        if request.preferences.top_p is not None:
            payload["top_p"] = request.preferences.top_p
        if request.preferences.max_output_tokens is not None:
            payload["max_tokens"] = request.preferences.max_output_tokens
        if request.preferences.json_mode:
            payload["response_format"] = {"type": "json_object"}
        return payload

    def _headers(self) -> dict[str, str]:
        return {
            "Authorization": f"Bearer {self.settings.api_key}",
            "Content-Type": "application/json",
        }

    def _chat_completions_url(self) -> str:
        base_url = self.settings.base_url.rstrip("/")
        if base_url.endswith("/chat/completions"):
            return base_url
        if base_url.endswith("/v1"):
            return f"{base_url}/chat/completions"
        return f"{base_url}/v1/chat/completions"


@dataclass
class _StreamParseState:
    tool_call_ids_by_index: dict[int, str] = field(default_factory=dict)
    seen_tool_call_ids: set[str] = field(default_factory=set)
    completed_tool_call_ids: set[str] = field(default_factory=set)
    finish_reason: str = "stop"


def _model_messages_to_openai(
    messages: list[ModelMessage],
    capabilities: ModelCapabilities,
) -> list[dict[str, Any]]:
    converter = MessageConverter()
    provider_messages = converter.to_provider_messages(
        messages,
        capabilities=capabilities,
    )
    for index, payload in enumerate(provider_messages):
        metadata = payload.pop("metadata", {}) or {}
        tool_calls = messages[index].metadata.get("tool_calls") or metadata.get("tool_calls")
        if tool_calls:
            payload["tool_calls"] = [_safe_provider_tool_call(tool_call) for tool_call in tool_calls]
    return provider_messages


def _safe_provider_tool_call(tool_call: Any) -> dict[str, Any]:
    if not isinstance(tool_call, dict):
        return {
            "id": "",
            "type": "function",
            "function": {"name": "<unknown>", "arguments": "{}"},
        }
    raw_function = tool_call.get("function")
    function = raw_function if isinstance(raw_function, dict) else {}
    arguments = function.get("arguments", "{}")
    if not isinstance(arguments, str):
        arguments = json.dumps(arguments, ensure_ascii=False, sort_keys=True, default=str)
    return {
        "id": str(tool_call.get("id") or ""),
        "type": str(tool_call.get("type") or "function"),
        "function": {
            "name": str(function.get("name") or "<unknown>"),
            "arguments": arguments or "{}",
        },
    }


def _model_tool_to_openai(tool: ModelToolSchema) -> dict[str, Any]:
    function: dict[str, Any] = {
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters_schema,
    }
    if tool.metadata.get("strict"):
        function["strict"] = True
    return {"type": "function", "function": function}


def _serialize_tool_choice(policy: ToolChoicePolicy | ToolChoiceMode | str) -> Any:
    if isinstance(policy, ToolChoiceMode):
        return policy.value
    if isinstance(policy, str):
        return policy
    if policy.mode == ToolChoiceMode.SPECIFIC_TOOL and policy.tool_name:
        return {"type": "function", "function": {"name": policy.tool_name}}
    if policy.mode == ToolChoiceMode.ALLOWED_TOOLS:
        return ToolChoiceMode.AUTO.value
    return policy.mode.value


def _raw_provider_tool_call(tool_call: dict[str, Any]) -> ModelToolCall:
    function = tool_call.get("function") or {}
    raw_arguments = function.get("arguments", "{}")
    if not isinstance(raw_arguments, str):
        raw_arguments = json.dumps(raw_arguments, ensure_ascii=False, sort_keys=True, default=str)
    try:
        parsed = json.loads(raw_arguments)
        parse_status = (
            ModelToolParseStatus.VALID
            if isinstance(parsed, dict)
            else ModelToolParseStatus.SCHEMA_MISMATCH
        )
        arguments = parsed if isinstance(parsed, dict) else {}
    except json.JSONDecodeError:
        parse_status = ModelToolParseStatus.INVALID_JSON
        arguments = {}
    return ModelToolCall(
        tool_call_id=str(tool_call.get("id") or ""),
        tool_name=str(function.get("name") or ""),
        arguments=arguments,
        raw_arguments=raw_arguments,
        parse_status=parse_status,
        provider_metadata={"raw_tool_call": tool_call},
    )


def _events_from_sse_line(
    line: str | bytes,
    *,
    state: _StreamParseState,
) -> Iterable[ProviderStreamEvent]:
    text = line.decode("utf-8", errors="replace") if isinstance(line, bytes) else str(line)
    text = text.strip()
    if not text or text.startswith(":"):
        return []
    if text.startswith("data:"):
        text = text.removeprefix("data:").strip()
    if text == "[DONE]":
        completed_events = [
            ProviderStreamEvent(
                type=ProviderStreamEventType.TOOL_CALL_COMPLETED,
                tool_call_id=call_id,
            )
            for call_id in sorted(state.seen_tool_call_ids - state.completed_tool_call_ids)
        ]
        completed_events.append(
            ProviderStreamEvent(
                type=ProviderStreamEventType.RESPONSE_COMPLETED,
                metadata={"finish_reason": state.finish_reason},
            )
        )
        return completed_events
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return [
            ProviderStreamEvent(
                type=ProviderStreamEventType.ERROR,
                error=f"Invalid provider stream chunk: {text[:80]}",
            )
        ]

    events: list[ProviderStreamEvent] = []
    usage_payload = payload.get("usage")
    if isinstance(usage_payload, dict):
        events.append(
            ProviderStreamEvent(
                type=ProviderStreamEventType.USAGE_DELTA,
                usage_delta=_usage_delta_from_payload(usage_payload),
            )
        )
    for choice in payload.get("choices") or []:
        if not isinstance(choice, dict):
            continue
        delta = choice.get("delta") or {}
        if not isinstance(delta, dict):
            delta = {}
        content = delta.get("content")
        if content:
            events.append(
                ProviderStreamEvent(
                    type=ProviderStreamEventType.TEXT_DELTA,
                    text_delta=str(content),
                )
            )
        for tool_event in _tool_delta_events(delta.get("tool_calls"), state):
            events.append(tool_event)
        finish_reason = choice.get("finish_reason")
        if finish_reason:
            state.finish_reason = str(finish_reason)
            for call_id in sorted(state.seen_tool_call_ids - state.completed_tool_call_ids):
                state.completed_tool_call_ids.add(call_id)
                events.append(
                    ProviderStreamEvent(
                        type=ProviderStreamEventType.TOOL_CALL_COMPLETED,
                        tool_call_id=call_id,
                    )
                )
    return events


def _tool_delta_events(
    tool_calls: Any,
    state: _StreamParseState,
) -> Iterable[ProviderStreamEvent]:
    if not isinstance(tool_calls, list):
        return []
    events: list[ProviderStreamEvent] = []
    for index, tool_call in enumerate(tool_calls):
        if not isinstance(tool_call, dict):
            continue
        raw_index = tool_call.get("index", index)
        try:
            call_index = int(raw_index)
        except (TypeError, ValueError):
            call_index = index
        if tool_call.get("id"):
            state.tool_call_ids_by_index[call_index] = str(tool_call["id"])
        call_id = state.tool_call_ids_by_index.get(call_index, f"call_stream_{call_index}")
        raw_function = tool_call.get("function")
        function = raw_function if isinstance(raw_function, dict) else {}
        name = function.get("name")
        arguments_delta = function.get("arguments")
        state.seen_tool_call_ids.add(call_id)
        events.append(
            ProviderStreamEvent(
                type=ProviderStreamEventType.TOOL_CALL_DELTA,
                tool_call_id=call_id,
                tool_name=str(name) if name else None,
                arguments_delta=str(arguments_delta) if arguments_delta is not None else None,
            )
        )
    return events


def _usage_delta_from_payload(payload: dict[str, Any]) -> dict[str, Any]:
    prompt_details = payload.get("prompt_tokens_details") or {}
    completion_details = payload.get("completion_tokens_details") or {}
    return {
        "input_tokens": int(payload.get("prompt_tokens") or 0),
        "output_tokens": int(payload.get("completion_tokens") or 0),
        "total_tokens": int(payload.get("total_tokens") or 0),
        "cached_input_tokens": int(prompt_details.get("cached_tokens") or 0),
        "reasoning_tokens": int(completion_details.get("reasoning_tokens") or 0),
    }


def _chat_accepts_tool_choice(provider: Any) -> bool:
    try:
        signature = inspect.signature(provider.chat)
    except (TypeError, ValueError, AttributeError):
        return False
    return "tool_choice" in signature.parameters


def _error_kind_for_status(status: int) -> ModelErrorKind:
    if status in {401, 403}:
        return ModelErrorKind.AUTH_ERROR
    if status == 400:
        return ModelErrorKind.INVALID_REQUEST
    if status == 408:
        return ModelErrorKind.TIMEOUT
    if status == 429:
        return ModelErrorKind.RATE_LIMITED
    if status in {500, 502, 503, 504}:
        return ModelErrorKind.PROVIDER_OVERLOADED
    return ModelErrorKind.UNKNOWN_PROVIDER_ERROR


def _request_error_is_permission_denied(exc: httpx.RequestError) -> bool:
    message = str(exc).lower()
    return any(
        marker in message
        for marker in (
            "winerror 10013",
            "permission denied",
            "operation not permitted",
            "access is denied",
            "访问权限不允许",
        )
    )
