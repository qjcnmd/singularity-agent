from singularity.context.assembler import ContextAssembler
from singularity.context.models import (
    ContextAuthority,
    ContextBundle,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextReference,
    ContextRenderPolicy,
    ContextSource,
    ContextSensitivity,
)
from singularity.context.tokens import TokenCounter


def item(
    item_id: str,
    *,
    layer: ContextLayer,
    item_type: ContextItemType,
    content: object,
    phase_id: str = "inspect",
    importance: float = 0.5,
    pinned: bool = False,
    sensitivity: ContextSensitivity = ContextSensitivity.WORKSPACE,
) -> ContextItem:
    return ContextItem(
        item_id=item_id,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id=phase_id,
        layer=layer,
        source_component=ContextSource.TOOL,
        item_type=item_type,
        content=content,
        authority=ContextAuthority.COMPONENT,
        sensitivity=sensitivity,
        importance=importance,
        pinned=pinned,
        token_count=5,
    )


def test_assembler_prioritizes_pinned_phase_relevant_component_evidence() -> None:
    assembler = ContextAssembler(
        token_counter=TokenCounter(model="gpt-4o-mini"),
        model_context_window=180,
        output_token_reserve=20,
    )
    items = [
        item(
            "system",
            layer=ContextLayer.SYSTEM,
            item_type=ContextItemType.SYSTEM_INSTRUCTION,
            content="system",
            pinned=True,
        ),
        item(
            "goal",
            layer=ContextLayer.USER_GOAL,
            item_type=ContextItemType.USER_GOAL,
            content="goal",
            pinned=True,
        ),
        item(
            "old_chat",
            layer=ContextLayer.RECENT_DIALOGUE,
            item_type=ContextItemType.ASSISTANT_MESSAGE,
            content="old chat " * 80,
            phase_id="inspect",
            importance=0.1,
        ),
        item(
            "policy_deny",
            layer=ContextLayer.POLICY_STATE,
            item_type=ContextItemType.POLICY_OBSERVATION,
            content={"outcome": "deny", "reason": "dangerous"},
            phase_id="verify",
            importance=0.9,
        ),
        item(
            "verification_fail",
            layer=ContextLayer.VERIFICATION,
            item_type=ContextItemType.VERIFICATION_EVIDENCE,
            content={"status": "failed", "repair_hints": ["fix import"]},
            phase_id="verify",
            importance=0.8,
        ),
    ]

    bundle = assembler.build_bundle(
        items=items,
        run_id="run_1",
        task_id="task_1",
        phase_id="verify",
        model="gpt-4o-mini",
        provider="mock",
        render_policy=ContextRenderPolicy(),
    )

    assert isinstance(bundle, ContextBundle)
    assert "policy_deny" in bundle.included_item_ids
    assert "verification_fail" in bundle.included_item_ids
    assert "old_chat" in bundle.excluded_item_ids
    assert bundle.budget.total_tokens <= 180
    assert bundle.metadata["lost_evidence_warning"] is True


def test_assembler_preserves_assistant_tool_call_and_tool_result_pairs() -> None:
    assembler = ContextAssembler(
        token_counter=TokenCounter(model="gpt-4o-mini"),
        model_context_window=120,
        output_token_reserve=20,
    )
    assistant = item(
        "assistant_call",
        layer=ContextLayer.RECENT_DIALOGUE,
        item_type=ContextItemType.ASSISTANT_MESSAGE,
        content={
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": "call_readme",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"},
                }
            ],
        },
        importance=0.9,
    )
    tool_result = item(
        "tool_result",
        layer=ContextLayer.TOOL_OBSERVATIONS,
        item_type=ContextItemType.TOOL_OBSERVATION,
        content={
            "role": "tool",
            "tool_call_id": "call_readme",
            "name": "read_file",
            "content": "README preview",
        },
        importance=0.9,
    )

    bundle = assembler.build_bundle(
        items=[
            item(
                "system",
                layer=ContextLayer.SYSTEM,
                item_type=ContextItemType.SYSTEM_INSTRUCTION,
                content="system",
                pinned=True,
            ),
            item(
                "goal",
                layer=ContextLayer.USER_GOAL,
                item_type=ContextItemType.USER_GOAL,
                content="goal",
                pinned=True,
            ),
            assistant,
            tool_result,
        ],
        run_id="run_1",
        task_id="task_1",
        phase_id="inspect",
        model="gpt-4o-mini",
        provider="mock",
        render_policy=ContextRenderPolicy(),
    )

    assistant_ids = {
        call["id"]
        for message in bundle.messages
        if message["role"] == "assistant"
        for call in message.get("tool_calls", [])
    }
    tool_ids = {
        message["tool_call_id"] for message in bundle.messages if message["role"] == "tool"
    }
    assert assistant_ids == tool_ids


def test_assembler_keeps_multi_tool_call_assistant_with_all_tool_results() -> None:
    assembler = ContextAssembler(
        token_counter=TokenCounter(model="gpt-4o-mini"),
        model_context_window=220,
        output_token_reserve=20,
    )
    assistant = item(
        "assistant_multi",
        layer=ContextLayer.RECENT_DIALOGUE,
        item_type=ContextItemType.ASSISTANT_MESSAGE,
        content={
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": "call_a",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"},
                },
                {
                    "id": "call_b",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"},
                },
            ],
        },
        importance=0.9,
    )
    tool_a = item(
        "tool_a",
        layer=ContextLayer.TOOL_OBSERVATIONS,
        item_type=ContextItemType.TOOL_OBSERVATION,
        content={"role": "tool", "tool_call_id": "call_a", "name": "read_file", "content": "A"},
        importance=0.9,
    )
    tool_b = item(
        "tool_b",
        layer=ContextLayer.TOOL_OBSERVATIONS,
        item_type=ContextItemType.TOOL_OBSERVATION,
        content={"role": "tool", "tool_call_id": "call_b", "name": "read_file", "content": "B"},
        importance=0.9,
    )

    bundle = assembler.build_bundle(
        items=[
            item(
                "system",
                layer=ContextLayer.SYSTEM,
                item_type=ContextItemType.SYSTEM_INSTRUCTION,
                content="system",
                pinned=True,
            ),
            item(
                "goal",
                layer=ContextLayer.USER_GOAL,
                item_type=ContextItemType.USER_GOAL,
                content="goal",
                pinned=True,
            ),
            assistant,
            tool_a,
            tool_b,
        ],
        run_id="run_1",
        task_id="task_1",
        phase_id="inspect",
        model="gpt-4o-mini",
        provider="mock",
        render_policy=ContextRenderPolicy(),
    )

    assistant_messages = [m for m in bundle.messages if m["role"] == "assistant"]
    assert len(assistant_messages) == 1
    assert [m["tool_call_id"] for m in bundle.messages if m["role"] == "tool"] == [
        "call_a",
        "call_b",
    ]


def test_assembler_renders_tool_previews_with_reference_ids_not_raw_outputs() -> None:
    assembler = ContextAssembler(
        token_counter=TokenCounter(model="gpt-4o-mini"),
        model_context_window=300,
        output_token_reserve=20,
    )
    reference = ContextReference(
        ref_id="ref_readme",
        ref_type="file",
        target="README.md",
        path="README.md",
        digest="abc",
        source_item_id="tool_result",
    )
    tool_item = item(
        "tool_result",
        layer=ContextLayer.TOOL_OBSERVATIONS,
        item_type=ContextItemType.TOOL_OBSERVATION,
        content={"preview": "short preview", "raw_result": "raw " * 200},
        importance=0.8,
    )
    tool_item.references.append(reference)

    bundle = assembler.build_bundle(
        items=[
            item(
                "system",
                layer=ContextLayer.SYSTEM,
                item_type=ContextItemType.SYSTEM_INSTRUCTION,
                content="system",
                pinned=True,
            ),
            item(
                "goal",
                layer=ContextLayer.USER_GOAL,
                item_type=ContextItemType.USER_GOAL,
                content="goal",
                pinned=True,
            ),
            tool_item,
        ],
        run_id="run_1",
        task_id="task_1",
        phase_id="inspect",
        model="gpt-4o-mini",
        provider="mock",
        render_policy=ContextRenderPolicy(include_raw_tool_outputs=False),
    )

    rendered = "\n".join(str(message.get("content")) for message in bundle.messages)
    assert "short preview" in rendered
    assert "ref_readme" in rendered
    assert "raw raw raw" not in rendered


def test_assembler_bounds_large_fragments_and_reports_context_shape() -> None:
    assembler = ContextAssembler(
        token_counter=TokenCounter(model="gpt-4o-mini"),
        model_context_window=2500,
        output_token_reserve=20,
    )
    huge = item(
        "workspace_big",
        layer=ContextLayer.WORKSPACE_STATE,
        item_type=ContextItemType.WORKSPACE_STATE,
        content={"raw": "x" * 40000},
        importance=0.9,
    )

    bundle = assembler.build_bundle(
        items=[
            item(
                "system",
                layer=ContextLayer.SYSTEM,
                item_type=ContextItemType.SYSTEM_INSTRUCTION,
                content="system",
                pinned=True,
            ),
            item(
                "goal",
                layer=ContextLayer.USER_GOAL,
                item_type=ContextItemType.USER_GOAL,
                content="goal",
                pinned=True,
            ),
            huge,
        ],
        run_id="run_1",
        task_id="task_1",
        phase_id="inspect",
        model="gpt-4o-mini",
        provider="mock",
        render_policy=ContextRenderPolicy(),
    )

    rendered = "\n".join(str(message.get("content")) for message in bundle.messages)

    assert "workspace_big" in rendered
    assert "source=tool" in rendered
    assert "digest=" in rendered
    assert "[truncated:context_fragment_cap]" in rendered
    assert bundle.metadata["context_shape_hash"]
    assert bundle.metadata["context_ordering_hash"]
    assert bundle.metadata["context_usage_report"]["included_item_ids"]
