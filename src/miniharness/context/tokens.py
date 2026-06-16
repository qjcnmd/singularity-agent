from __future__ import annotations

import json
from typing import Any


class TokenizerUnavailableError(RuntimeError):
    pass


class TokenCounter:
    def __init__(self, *, model: str = "gpt-4o-mini") -> None:
        try:
            import tiktoken
        except ModuleNotFoundError as exc:
            raise TokenizerUnavailableError(
                "Precise token counting requires the 'tiktoken' package. "
                "Install Miniharness dependencies before running context budgeting."
            ) from exc

        try:
            self.encoding = tiktoken.encoding_for_model(model)
        except KeyError:
            self.encoding = tiktoken.get_encoding("o200k_base")
        self.model = model

    def count_text(self, text: str) -> int:
        return len(self.encoding.encode(text))

    def count_messages(self, messages: list[dict[str, Any]]) -> int:
        return sum(self.count_message(message) for message in messages)

    def count_message(self, message: dict[str, Any]) -> int:
        payload = json.dumps(message, ensure_ascii=False, sort_keys=True, default=str)
        return self.count_text(payload) + 4

    def count_tools(self, tools: list[dict[str, Any]] | None) -> int:
        if not tools:
            return 0
        payload = json.dumps(tools, ensure_ascii=False, sort_keys=True, default=str)
        return self.count_text(payload)
