import pytest

from miniharness.model import (
    ContentBlock,
    ModelBudget,
    ModelBudgetExceeded,
    ModelBudgetManager,
    ModelMessage,
    ModelRole,
    ModelUsage,
)


def test_budget_manager_estimates_tokens_and_merges_usage() -> None:
    manager = ModelBudgetManager()
    messages = [
        ModelMessage(role=ModelRole.USER, content=[ContentBlock.from_text("hello world")])
    ]
    usage = manager.estimate_input(messages=messages, tools=[])

    assert usage.input_tokens > 0
    manager.check_budget(messages=messages, tools=[], budget=ModelBudget(max_input_tokens=1000))
    with pytest.raises(ModelBudgetExceeded):
        manager.check_budget(messages=messages, tools=[], budget=ModelBudget(max_input_tokens=1))

    merged = manager.merge_usage(
        ModelUsage(input_tokens=1, output_tokens=2),
        ModelUsage(input_tokens=3, output_tokens=4, cached_input_tokens=1),
    )
    assert merged.total_tokens == 10
    assert merged.cached_input_tokens == 1


def test_budget_manager_checks_response_usage_and_latency() -> None:
    manager = ModelBudgetManager()
    budget = ModelBudget(
        max_output_tokens=2,
        max_total_tokens=5,
        max_latency_ms=100,
        max_cost_estimate=0.01,
    )

    manager.check_response_budget(
        ModelUsage(input_tokens=2, output_tokens=2, cost_estimate=0.01),
        budget=budget,
        latency_ms=100,
    )
    with pytest.raises(ModelBudgetExceeded):
        manager.check_response_budget(
            ModelUsage(input_tokens=1, output_tokens=3),
            budget=budget,
            latency_ms=1,
        )
    with pytest.raises(ModelBudgetExceeded):
        manager.check_response_budget(
            ModelUsage(input_tokens=4, output_tokens=2),
            budget=budget,
            latency_ms=1,
        )
    with pytest.raises(ModelBudgetExceeded):
        manager.check_response_budget(
            ModelUsage(input_tokens=1, output_tokens=1, cost_estimate=0.02),
            budget=budget,
            latency_ms=1,
        )
    with pytest.raises(ModelBudgetExceeded):
        manager.check_response_budget(
            ModelUsage(input_tokens=1, output_tokens=1),
            budget=budget,
            latency_ms=101,
        )

