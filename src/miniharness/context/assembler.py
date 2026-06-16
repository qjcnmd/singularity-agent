from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from miniharness.context.tokens import TokenCounter


@dataclass(frozen=True)
class ContextBudget:
    model_context_window: int
    output_token_reserve: int
    message_tokens: int
    tool_tokens: int

    @property
    def total_tokens(self) -> int:
        return self.message_tokens + self.tool_tokens + self.output_token_reserve

    @property
    def remaining_tokens(self) -> int:
        return self.model_context_window - self.total_tokens


class ContextAssembler:
    def __init__(
        self,
        *,
        token_counter: TokenCounter,
        model_context_window: int,
        output_token_reserve: int,
    ) -> None:
        self.token_counter = token_counter
        self.model_context_window = model_context_window
        self.output_token_reserve = output_token_reserve

    def assemble(
        self,
        *,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]] | None = None,
        summary: str | None = None,
    ) -> tuple[list[dict[str, Any]], ContextBudget]:
        tool_tokens = self.token_counter.count_tools(tools)
        base = self._base_messages(messages, summary=summary)
        self._assert_base_fits(base, tool_tokens)
        groups = self._history_groups(messages[2:])
        selected: list[list[dict[str, Any]]] = []
        for group in reversed(groups):
            candidate = base + [
                message for selected_group in reversed(selected) for message in selected_group
            ]
            candidate = candidate + group
            if self._fits(candidate, tool_tokens):
                selected.append(group)

        assembled = base + [
            message for selected_group in reversed(selected) for message in selected_group
        ]
        return assembled, self._budget(assembled, tool_tokens)

    def needs_compression(
        self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]] | None = None
    ) -> bool:
        tool_tokens = self.token_counter.count_tools(tools)
        return not self._fits(messages, tool_tokens)

    def _base_messages(
        self, messages: list[dict[str, Any]], *, summary: str | None
    ) -> list[dict[str, Any]]:
        base = [dict(messages[0]), dict(messages[1])]
        if summary:
            base.append(
                {
                    "role": "system",
                    "content": f"Context summary:\n{summary}",
                }
            )
        return base

    def _history_groups(
        self, history: list[dict[str, Any]]
    ) -> list[list[dict[str, Any]]]:
        groups: list[list[dict[str, Any]]] = []
        index = 0
        while index < len(history):
            message = history[index]
            if message.get("role") == "assistant" and message.get("tool_calls"):
                call_ids = {
                    call.get("id")
                    for call in message.get("tool_calls", [])
                    if call.get("id")
                }
                group = [message]
                index += 1
                while (
                    index < len(history)
                    and history[index].get("role") == "tool"
                    and history[index].get("tool_call_id") in call_ids
                ):
                    group.append(history[index])
                    index += 1
                groups.append(group)
                continue
            if message.get("role") == "tool":
                groups.append([message])
                index += 1
                continue
            groups.append([message])
            index += 1
        return groups

    def _assert_base_fits(self, messages: list[dict[str, Any]], tool_tokens: int) -> None:
        if not self._fits(messages, tool_tokens):
            raise ValueError(
                "System/user context plus reserved output tokens exceed the model context window."
            )

    def _fits(self, messages: list[dict[str, Any]], tool_tokens: int) -> bool:
        return self._budget(messages, tool_tokens).total_tokens <= self.model_context_window

    def _budget(
        self, messages: list[dict[str, Any]], tool_tokens: int
    ) -> ContextBudget:
        return ContextBudget(
            model_context_window=self.model_context_window,
            output_token_reserve=self.output_token_reserve,
            message_tokens=self.token_counter.count_messages(messages),
            tool_tokens=tool_tokens,
        )
