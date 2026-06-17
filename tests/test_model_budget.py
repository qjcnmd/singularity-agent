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
        ModelMessage(role=ModelRole.USER, content=[ContentBlock.text("hello world")])
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

