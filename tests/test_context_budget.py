import pytest

from miniharness.context.assembler import ContextAssembler, ContextOverflowError
from miniharness.context.models import (
    ContextAuthority,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextRuntime,
    ContextSensitivity,
)
from miniharness.context.tokens import TokenCounter


def system_item(text: str) -> ContextItem:
    return ContextItem(
        item_id="system",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="inspect",
        layer=ContextLayer.SYSTEM,
        source_runtime=ContextRuntime.SYSTEM,
        item_type=ContextItemType.SYSTEM_INSTRUCTION,
        content=text,
        authority=ContextAuthority.SYSTEM,
        sensitivity=ContextSensitivity.PUBLIC,
        pinned=True,
    )


def goal_item(text: str) -> ContextItem:
    return ContextItem(
        item_id="goal",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="inspect",
        layer=ContextLayer.USER_GOAL,
        source_runtime=ContextRuntime.USER,
        item_type=ContextItemType.USER_GOAL,
        content=text,
        authority=ContextAuthority.USER,
        sensitivity=ContextSensitivity.PUBLIC,
        pinned=True,
    )


def test_budget_reserves_output_and_counts_tool_schema_tokens() -> None:
    assembler = ContextAssembler(
        token_counter=TokenCounter(model="gpt-4o-mini"),
        model_context_window=160,
        output_token_reserve=25,
    )
    bundle = assembler.build_bundle(
        items=[system_item("system"), goal_item("goal")],
        run_id="run_1",
        task_id="task_1",
        phase_id="inspect",
        model="gpt-4o-mini",
        provider="mock",
        tools=[
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object", "properties": {}},
                },
            }
        ],
    )

    assert bundle.budget.output_token_reserve == 25
    assert bundle.budget.tool_schema_tokens > 0
    assert bundle.budget.total_tokens <= 160


def test_budget_raises_structured_overflow_for_unavoidable_base_context() -> None:
    assembler = ContextAssembler(
        token_counter=TokenCounter(model="gpt-4o-mini"),
        model_context_window=40,
        output_token_reserve=20,
    )

    with pytest.raises(ContextOverflowError) as exc:
        assembler.build_bundle(
            items=[system_item("system " * 100), goal_item("goal")],
            run_id="run_1",
            task_id="task_1",
            phase_id="inspect",
            model="gpt-4o-mini",
            provider="mock",
        )

    assert exc.value.overflow_tokens > 0
    assert exc.value.budget.hard_limit == 40

