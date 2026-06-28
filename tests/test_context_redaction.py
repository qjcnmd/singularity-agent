import json
import sqlite3
from typing import Any

from singularity.context import ContextManager
from singularity.context.models import ContextSensitivity
from singularity.context.redaction import ContextRedactor, SensitivityClassifier
from singularity.context.tokens import TokenCounter
from singularity.provider import ToolChoiceMode
from singularity.tool_protocol.models import ToolProtocolResultEnvelope


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


def test_context_redactor_redacts_aws_access_key() -> None:
    aws_key = "AKIAIOSFODNN7EXAMPLE"
    text = f"using aws key {aws_key} for upload"
    classifier = SensitivityClassifier()
    redactor = ContextRedactor()

    assert classifier.classify(text) == ContextSensitivity.SECRET
    redacted = redactor.redact_text(text)
    assert aws_key not in redacted
    assert "<redacted:" in redacted


def test_context_redactor_redacts_jwt() -> None:
    jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
    text = f"bearer {jwt}"
    classifier = SensitivityClassifier()
    redactor = ContextRedactor()

    assert classifier.classify(text) == ContextSensitivity.SECRET
    redacted = redactor.redact_text(text)
    assert jwt not in redacted
    assert "eyJ" not in redacted
    assert "<redacted:" in redacted


def test_context_redactor_redacts_slack_token() -> None:
    slack = "xoxb-1234567890-abcdef"
    text = f"slack token: {slack}"
    classifier = SensitivityClassifier()
    redactor = ContextRedactor()

    assert classifier.classify(text) == ContextSensitivity.SECRET
    redacted = redactor.redact_text(text)
    assert slack not in redacted
    assert "xoxb-" not in redacted
    assert "<redacted:" in redacted


def test_context_redactor_redacts_stripe_live_key() -> None:
    stripe = "sk_live_abcdef1234567890"
    text = f"charge with {stripe} failed"
    classifier = SensitivityClassifier()
    redactor = ContextRedactor()

    assert classifier.classify(text) == ContextSensitivity.SECRET
    redacted = redactor.redact_text(text)
    assert stripe not in redacted
    assert "sk_live_" not in redacted
    assert "<redacted:" in redacted


def test_context_redactor_redacts_google_api_key() -> None:
    google_key = "AIza" + "A" * 35
    text = f"maps api call {google_key} done"
    classifier = SensitivityClassifier()
    redactor = ContextRedactor()

    assert classifier.classify(text) == ContextSensitivity.SECRET
    redacted = redactor.redact_text(text)
    assert google_key not in redacted
    assert "AIza" not in redacted
    assert "<redacted:" in redacted


def test_context_redactor_redacts_sensitive_dict_field_values() -> None:
    payload = {
        "authorization": "Bearer abcdefgh",
        "credential": "user:pass",
        "access_token": "token-value-123",
        "refresh_token": "refresh-value-456",
        "client_secret": "secret-value-789",
        "passphrase": "my-passphrase",
        "private_key": "-----BEGIN PRIVATE KEY-----",
        "safe": "keep-me",
    }
    redactor = ContextRedactor()

    redacted = redactor.redact_value(payload)

    assert redacted["authorization"] != "Bearer abcdefgh"
    assert "abcdefgh" not in str(redacted["authorization"])
    assert redacted["credential"] != "user:pass"
    assert "user:pass" not in str(redacted["credential"])
    assert redacted["access_token"] != "token-value-123"
    assert "token-value-123" not in str(redacted["access_token"])
    assert redacted["refresh_token"] != "refresh-value-456"
    assert redacted["client_secret"] != "secret-value-789"
    assert redacted["passphrase"] != "my-passphrase"
    assert redacted["private_key"] != "-----BEGIN PRIVATE KEY-----"
    assert redacted["safe"] == "keep-me"
    assert "<redacted:" in str(redacted["authorization"])
    assert "<redacted:" in str(redacted["credential"])
    assert "<redacted:" in str(redacted["access_token"])


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


class _StubClassifier:
    def __init__(self, level: ContextSensitivity) -> None:
        self.level = level

    def classify(self, value: Any) -> ContextSensitivity:
        return self.level


def test_add_tool_protocol_result_redacts_all_sensitivity_levels(tmp_path) -> None:
    for level in (ContextSensitivity.WORKSPACE, ContextSensitivity.PUBLIC):
        context = ContextManager(
            system_prompt="system",
            user_goal="inspect",
            db_path=tmp_path / f"context_{level.value}.sqlite3",
            token_counter=TokenCounter(model="gpt-4o-mini"),
        )
        context.classifier = _StubClassifier(level)
        envelope = ToolProtocolResultEnvelope(
            tool_call_id="call_secret",
            tool_name="read_file",
            ok=True,
            status="ok",
            content_preview="loaded key sk-test123 for upload",
            content_digest="digest_1",
        )

        observation = context.add_tool_protocol_result(envelope)

        tool_message = context.messages()[-1]
        payload = json.loads(tool_message["content"])
        assert observation.sensitivity == level
        assert "sk-test123" not in observation.preview
        assert "sk-test123" not in tool_message["content"]
        assert "<redacted:" in observation.preview
        assert payload["redacted"] is True
        assert payload["content_preview"] == observation.preview
        context.close()


def test_add_tool_protocol_result_redacts_error_code_field(tmp_path) -> None:
    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        db_path=tmp_path / "context_error.sqlite3",
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    envelope = ToolProtocolResultEnvelope(
        tool_call_id="call_err",
        tool_name="read_file",
        ok=False,
        status="error",
        error_code="auth failed for token sk-test123",
        content_preview="permission denied",
        content_digest="digest_err",
    )

    context.add_tool_protocol_result(envelope)

    tool_message = context.messages()[-1]
    payload = json.loads(tool_message["content"])
    assert "sk-test123" not in tool_message["content"]
    assert "<redacted:" in payload["error_code"]
    assert payload["redacted"] is True
    context.close()
