from __future__ import annotations

from miniharness.context.tokens import TokenCounter
from miniharness.model.errors import ModelBudgetExceeded, ModelContextTooLong
from miniharness.model.messages import MessageConverter
from miniharness.model.models import ModelBudget, ModelMessage, ModelToolSchema, ModelUsage


class ModelBudgetManager:
    def __init__(self, *, token_counter: TokenCounter | None = None) -> None:
        self.token_counter = token_counter or TokenCounter()
        self.converter = MessageConverter(token_counter=self.token_counter)

    def estimate_input(
        self,
        *,
        messages: list[ModelMessage],
        tools: list[ModelToolSchema],
    ) -> ModelUsage:
        message_tokens = self.converter.estimate_tokens(messages)
        tool_tokens = self.token_counter.count_text(
            " ".join(tool.name + " " + tool.description for tool in tools)
        ) if tools else 0
        return ModelUsage(input_tokens=message_tokens + tool_tokens)

    def check_budget(
        self,
        *,
        messages: list[ModelMessage],
        tools: list[ModelToolSchema],
        budget: ModelBudget,
    ) -> ModelUsage:
        usage = self.estimate_input(messages=messages, tools=tools)
        if budget.max_input_tokens is not None and usage.input_tokens > budget.max_input_tokens:
            raise ModelBudgetExceeded("Model input token budget exceeded.")
        if budget.max_total_tokens is not None and usage.input_tokens > budget.max_total_tokens:
            raise ModelBudgetExceeded("Model total token budget exceeded.")
        return usage

    @staticmethod
    def check_context_window(usage: ModelUsage, *, max_context_tokens: int) -> None:
        if usage.input_tokens > max_context_tokens:
            raise ModelContextTooLong("Model context length exceeded.")

    @staticmethod
    def merge_usage(*usages: ModelUsage) -> ModelUsage:
        cost_values = [usage.cost_estimate for usage in usages if usage.cost_estimate is not None]
        return ModelUsage(
            input_tokens=sum(usage.input_tokens for usage in usages),
            output_tokens=sum(usage.output_tokens for usage in usages),
            total_tokens=sum(usage.total_tokens for usage in usages),
            cached_input_tokens=sum(usage.cached_input_tokens for usage in usages),
            reasoning_tokens=sum(usage.reasoning_tokens for usage in usages),
            cost_estimate=sum(cost_values) if cost_values else None,
        )

