from miniharness.context import ContextManager
from miniharness.context.models import ContextSensitivity
from miniharness.context.redaction import ContextRedactor, SensitivityClassifier
from miniharness.context.tokens import TokenCounter


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
