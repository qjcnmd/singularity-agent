import json
import sqlite3

from miniharness.context import ContextManager
from miniharness.context.models import ContextSensitivity
from miniharness.context.redaction import ContextRedactor, SensitivityClassifier
from miniharness.context.tokens import TokenCounter
from miniharness.provider import ToolChoiceMode


def test_context_redactor_classifies_and_redacts_secret_patterns() -> None:
    text = (
        "Authorization: Bearer sk-secret-123456789\n"
        "password=my-password\n"
        "ghp_abcdefghijklmnopqrstuvwxyz123456\n"
        "npm_abcdefghijklmnop"
    )
    classifier = SensitivityClassifier()
    redactor = ContextRedactor()

    assert classifier.classify(text) == ContextSensitivity.SECRET
    redacted = redactor.redact_text(text)
    assert "sk-secret" not in redacted
    assert "my-password" not in redacted
    assert "ghp_" not in redacted
    assert "npm_" not in redacted
    assert "<redacted:" in redacted


def test_secret_tool_result_is_not_rendered_to_model_by_default(tmp_path) -> None:
    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        db_path=tmp_path / "context.sqlite3",
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )

    context.add_tool_result(
        tool_call={
            "id": "call_env",
            "type": "function",
            "function": {"name": "read_file", "arguments": '{"path": ".env"}'},
        },
        result={"ok": True, "content": "OPENAI_API_KEY=sk-secret-123456789"},
    )

    rendered = "\n".join(str(message.get("content")) for message in context.messages())
    stored = context.store.query_items(
        run_id=context.run_id,
        item_type="tool_observation",
    )[-1]

    assert "sk-secret" not in rendered
    assert stored.sensitivity == ContextSensitivity.SECRET
    assert "<redacted:" in rendered


def test_assistant_tool_call_arguments_are_not_persisted_or_compacted(tmp_path) -> None:
    db_path = tmp_path / "context.sqlite3"
    provider = _CompressionProvider()
    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        provider=provider,
        db_path=db_path,
        model_context_window=100,
        output_token_reserve=20,
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    secret_call = {
        "id": "call_secret",
        "type": "function",
        "function": {
            "name": "read_file",
            "arguments": '{"path": ".env", "api_key": "sk-secret-123456789"}',
        },
    }

    context.add_assistant_message(
        {
            "role": "assistant",
            "content": None,
            "tool_calls": [secret_call],
        }
    )
    context.messages(persist=True)

    with sqlite3.connect(db_path) as connection:
        persisted = "\n".join(
            str(row[0])
            for row in connection.execute("select payload from messages union all select messages from context_bundles")
        )
    compacted = json.dumps(provider.calls, ensure_ascii=False)
    compression_payload = json.loads(provider.calls[0]["messages"][1]["content"])
    compacted_tool_call = compression_payload["messages"][-1]["tool_calls"][0]

    assert "call_secret" in persisted
    assert "read_file" in persisted
    assert '"arguments": "{}"' in persisted
    assert "sk-secret" not in persisted
    assert ".env" not in persisted
    assert compacted_tool_call["function"] == {"name": "read_file", "arguments": "{}"}
    assert "sk-secret" not in compacted
    assert ".env" not in compacted


class _CompressionProvider:
    def __init__(self) -> None:
        self.calls = []

    def chat(self, *, messages, tools, tool_choice=ToolChoiceMode.AUTO):
        self.calls.append({"messages": messages, "tools": tools, "tool_choice": tool_choice})
        return {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": json.dumps(
                            {
                                "goal": "inspect",
                                "current_state": "compressed",
                                "completed_actions": [],
                                "pending_actions": [],
                                "verified_facts": [],
                                "failed_attempts": [],
                                "policy_constraints": [],
                                "workspace_changes": [],
                                "verification_status": "unknown",
                                "open_questions": [],
                                "reference_ids": [],
                                "omitted_item_ids": [],
                                "confidence": 0.8,
                            }
                        ),
                    }
                }
            ]
        }
