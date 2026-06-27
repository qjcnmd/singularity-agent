import json

from singularity.context import ContextManager
from singularity.memory.models import MemoryContextBlock
from singularity.tool_protocol.models import ToolProtocolResultEnvelope


def test_context_manager_initializes_system_and_user_messages() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")

    assert context.messages() == [
        {"role": "system", "content": "system rules"},
        {"role": "user", "content": "inspect project"},
    ]


def test_add_assistant_message_includes_assistant_message() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")
    assistant_message = {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_readme",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{}"},
            }
        ],
    }

    context.add_assistant_message(assistant_message)

    persisted = context.messages()[-1]
    assert persisted == assistant_message


def test_add_tool_result_generates_tool_message() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")
    tool_call = {
        "id": "call_readme",
        "type": "function",
        "function": {"name": "read_file", "arguments": "{}"},
    }
    result = {"ok": True, "content": "README content"}

    observation = context.add_tool_result(tool_call=tool_call, result=result)

    tool_message = context.messages()[-1]
    assert tool_message["role"] == "tool"
    assert tool_message["tool_call_id"] == "call_readme"
    assert tool_message["name"] == "read_file"
    assert '"content": "README content"' in tool_message["content"]
    assert observation.tool_name == "read_file"
    assert observation.tool_call_id == "call_readme"
    assert observation.ok is True


def test_long_tool_result_is_truncated_in_message_but_raw_result_is_preserved() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")
    tool_call = {
        "id": "call_long",
        "type": "function",
        "function": {"name": "read_file", "arguments": "{}"},
    }
    long_content = "x" * 4100
    result = {"ok": True, "content": long_content, "path": "README.md"}

    observation = context.add_tool_result(tool_call=tool_call, result=result)

    tool_message = context.messages()[-1]
    assert len(observation.preview) == 4000
    assert observation.preview == "x" * 4000
    assert observation.truncated is True
    assert observation.raw_result == result
    payload = json.loads(tool_message["content"])
    assert payload["truncated"] is True
    assert payload["truncation_reason"] == "preview_limit"
    # The manager stores the preview; the assembler adds the context_fragment_cap marker.
    assert payload["content"] == "x" * 4000
    assert payload["raw_digest"] == observation.raw_digest
    assert long_content not in tool_message["content"]


def test_tool_protocol_result_message_omits_internal_only_fields() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")
    envelope = ToolProtocolResultEnvelope(
        tool_call_id="call_internal",
        tool_name="read_file",
        ok=True,
        status="ok",
        content_preview='{"content":"README"}',
        content_digest="digest_1",
        raw_result_ref="result_ref_1",
        artifact_refs=["artifact_1"],
        policy_decision_id="policy_decision_1",
        approval_grant_id="approval_grant_1",
        metadata={
            "cache_hit": True,
            "raw_arguments": {"path": "README.md"},
            "internal_debug": {"policy": "allow"},
        },
    )

    observation = context.add_tool_protocol_result(envelope)

    tool_message = context.messages()[-1]
    payload = json.loads(tool_message["content"])
    assert payload["tool_call_id"] == "call_internal"
    assert payload["tool_name"] == "read_file"
    assert payload["content_digest"] == "digest_1"
    assert payload["result_ref"] == "result_ref_1"
    assert payload["reference_ids"] == ["artifact_1"]
    assert "policy_decision_id" not in payload
    assert "approval_grant_id" not in payload
    assert "metadata" not in payload
    assert "artifact_refs" not in payload
    assert "raw_arguments" not in tool_message["content"]
    assert "internal_debug" not in tool_message["content"]
    assert observation.metadata["policy_decision_id"] == "policy_decision_1"
    assert observation.metadata["approval_grant_id"] == "approval_grant_1"
    assert observation.raw_result == payload


def test_add_memory_context_block_adds_untrusted_memory_item() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")
    block = MemoryContextBlock(
        items=[
            {
                "id": "mem_1",
                "title": "Use pytest",
                "body": "Run python -m pytest tests.",
                "source": "verification",
                "confidence": "high",
                "last_verified_at": "2026-06-19T00:00:00+00:00",
                "pollution_risk": "low",
            }
        ],
        token_count=12,
        budget=128,
    )

    item = context.add_memory_context_block(block)

    assert item.item_type.value == "memory_context"
    assert item.source_component.value == "memory"
    assert item.content["trust_level"] == "untrusted_memory"
    assert item.content["items"][0]["id"] == "mem_1"
