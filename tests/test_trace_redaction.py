from __future__ import annotations

from singularity.observability.redaction import TraceRedactor


def test_redacts_secret_keys_recursively_without_preserving_secret_parts() -> None:
    redactor = TraceRedactor(output_limit_chars=1000)
    payload = {
        "OPENAI_API_KEY": "sk-secret-value",
        "nested": [
            {"password": "hunter2"},
            {"Authorization": "Bearer token-value"},
            {"safe": "hello"},
        ],
        "cookie": "session=abc123",
    }

    redacted = redactor.redact_payload(payload)

    assert redacted["OPENAI_API_KEY"] == "<redacted>"
    assert redacted["nested"][0]["password"] == "<redacted>"
    assert redacted["nested"][1]["Authorization"] == "<redacted>"
    assert redacted["nested"][2]["safe"] == "hello"
    assert redacted["cookie"] == "<redacted>"
    assert "sk-" not in str(redacted)
    assert "hunter2" not in str(redacted)
    assert "abc123" not in str(redacted)


def test_redacts_env_style_text_and_auth_headers() -> None:
    redactor = TraceRedactor(output_limit_chars=1000)
    text = "\n".join(
        [
            "OPENAI_API_KEY=sk-prod-123",
            "Authorization: Bearer github-secret",
            "Cookie: sid=abc123",
            "normal=value",
        ]
    )

    redacted = redactor.redact_text(text)

    assert "OPENAI_API_KEY=<redacted>" in redacted
    assert "Authorization: <redacted>" in redacted
    assert "Cookie: <redacted>" in redacted
    assert "normal=value" in redacted
    assert "sk-prod" not in redacted
    assert "github-secret" not in redacted
    assert "abc123" not in redacted


def test_redacts_cli_secret_flags_and_json_argv_values() -> None:
    redactor = TraceRedactor(output_limit_chars=1000)
    text = "\n".join(
        [
            "tool --password hunter2 --token=abc123",
            '["curl", "-H", "Authorization: Bearer opaque", "--api-key", "plain-key"]',
        ]
    )

    redacted = redactor.redact_text(text)

    assert "hunter2" not in redacted
    assert "abc123" not in redacted
    assert "opaque" not in redacted
    assert "plain-key" not in redacted
    assert "--password <redacted>" in redacted
    assert "--token=<redacted>" in redacted


def test_redacts_long_text_and_payload_hash_is_stable() -> None:
    redactor = TraceRedactor(output_limit_chars=24)
    payload = {"safe": "x" * 100, "token": "secret-token"}

    first = redactor.hash_payload(payload)
    second = redactor.hash_payload({"token": "secret-token", "safe": "x" * 100})
    redacted = redactor.redact_payload(payload)

    assert first == second
    assert redacted["token"] == "<redacted>"
    assert str(redacted["safe"]).endswith("[truncated]")
    assert len(redacted["safe"]) <= 40


def test_preserves_safe_numeric_token_usage_metrics() -> None:
    redactor = TraceRedactor(output_limit_chars=1000)
    payload = {
        "usage": {
            "input_tokens": 12,
            "output_tokens": 7,
            "total_tokens": 19,
            "token": "secret-token",
        }
    }

    redacted = redactor.redact_payload(payload)

    assert redacted["usage"]["input_tokens"] == 12
    assert redacted["usage"]["output_tokens"] == 7
    assert redacted["usage"]["total_tokens"] == 19
    assert redacted["usage"]["token"] == "<redacted>"
