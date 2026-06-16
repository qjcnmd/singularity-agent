from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from typing import Any

import httpx

from miniharness.config import Settings


class ToolChoiceMode(StrEnum):
    AUTO = "auto"
    REQUIRED = "required"
    NONE = "none"


@dataclass(frozen=True)
class ProviderCapabilities:
    supports_tools: bool = True
    supports_strict_tools: bool = False
    supports_tool_choice_required: bool = False
    supports_parallel_tool_calls: bool = False


class OpenAICompatibleProvider:
    def __init__(
        self,
        settings: Settings,
        *,
        timeout_seconds: float = 60.0,
        capabilities: ProviderCapabilities | None = None,
    ) -> None:
        self.settings = settings
        self.timeout_seconds = timeout_seconds
        self.capabilities = capabilities or ProviderCapabilities()

    def chat(
        self,
        *,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        tool_choice: ToolChoiceMode | str = ToolChoiceMode.AUTO,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "model": self.settings.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": self._serialize_tool_choice(tool_choice),
        }
        headers = {
            "Authorization": f"Bearer {self.settings.api_key}",
            "Content-Type": "application/json",
        }

        with httpx.Client(timeout=self.timeout_seconds) as client:
            response = client.post(self._chat_completions_url(), headers=headers, json=payload)

        try:
            response.raise_for_status()
        except httpx.HTTPStatusError as exc:
            body = response.text[:1000]
            raise RuntimeError(
                f"Provider returned HTTP {response.status_code}: {body}"
            ) from exc

        return response.json()

    def _chat_completions_url(self) -> str:
        base_url = self.settings.base_url.rstrip("/")
        if base_url.endswith("/chat/completions"):
            return base_url
        if base_url.endswith("/v1"):
            return f"{base_url}/chat/completions"
        return f"{base_url}/v1/chat/completions"

    @staticmethod
    def _serialize_tool_choice(tool_choice: ToolChoiceMode | str) -> str:
        if isinstance(tool_choice, ToolChoiceMode):
            return tool_choice.value
        allowed = {mode.value for mode in ToolChoiceMode}
        if tool_choice not in allowed:
            raise ValueError(
                f"Unsupported tool_choice={tool_choice!r}; expected one of {sorted(allowed)}"
            )
        return tool_choice
