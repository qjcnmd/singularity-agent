from __future__ import annotations

from typing import Any

import httpx

from miniharness.config import Settings


class OpenAICompatibleProvider:
    def __init__(self, settings: Settings, *, timeout_seconds: float = 60.0) -> None:
        self.settings = settings
        self.timeout_seconds = timeout_seconds

    def chat(
        self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]]
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "model": self.settings.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
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
