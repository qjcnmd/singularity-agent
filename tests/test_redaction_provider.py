from __future__ import annotations

from singularity.context.redaction import ContextRedactor
from singularity.observability.redaction import TraceRedactor
from singularity.policy.audit import redact as redact_audit
from singularity.redaction import RedactionMarker, RedactionProvider


def test_redaction_provider_supports_context_digest_markers() -> None:
    provider = RedactionProvider(marker=RedactionMarker.DIGEST)
    text = "Authorization: Bearer sk-secret-123456789"

    redacted = provider.redact_text(text)

    assert "sk-secret" not in redacted
    assert redacted.startswith("Authorization: Bearer <redacted:")


def test_redaction_provider_supports_trace_plain_markers_and_safe_metrics() -> None:
    provider = RedactionProvider(marker=RedactionMarker.PLAIN, output_limit_chars=1000)
    payload = {
        "usage": {
            "input_tokens": 12,
            "output_tokens": 7,
            "token": "secret-token",
        },
        "authorization": "Bearer opaque-secret",
        "url": "https://example.test/path?api_key=plain-key",
    }

    redacted = provider.redact_value(payload)

    assert redacted["usage"]["input_tokens"] == 12
    assert redacted["usage"]["output_tokens"] == 7
    assert redacted["usage"]["token"] == "<redacted>"
    assert redacted["authorization"] == "<redacted>"
    assert redacted["url"].endswith("api_key=<redacted>")


def test_redaction_provider_redacts_sensitive_paths_without_hashing() -> None:
    provider = RedactionProvider(marker=RedactionMarker.PLAIN)

    assert provider.redact_path(r"C:\Users\me\.ssh\id_rsa") == "<redacted>"
    assert provider.redact_path(r"C:\project\src\module.py") == r"C:\project\src\module.py"


def test_callers_preserve_their_public_redaction_markers() -> None:
    text = "Authorization: Bearer sk-secret-123456789"

    assert "<redacted:" in ContextRedactor().redact_text(text)
    assert TraceRedactor().redact_text(text) == "Authorization: <redacted>"
    assert redact_audit(text) == "Authorization: [REDACTED]"


def test_redaction_provider_detects_and_redacts_common_secret_shapes() -> None:
    provider = RedactionProvider(marker=RedactionMarker.PLAIN)
    samples = [
        "Authorization: Bearer ghp_abcdefghijklmnopqrstuvwxyz123456",
        "provider replied with Bearer raw-token-value",
        "Cookie: session=super-secret-cookie",
        "https://example.test/path?token=raw-token&client_secret=raw-secret",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature",
        "AKIA1234567890ABCDEF",
        "xoxb-123456789-abcdefghijklmnop",
        "sk_live_abcdefghijklmnopqrstuvwxyz",
        "AIzaSyA1234567890abcdefghijklmnopqrstuvwxyz",
        "-----BEGIN PRIVATE KEY-----\nsecret\n-----END PRIVATE KEY-----",
    ]

    for sample in samples:
        assert provider.contains_secret(sample), sample
        redacted = provider.redact_text(sample)
        assert redacted != sample
        assert "<redacted>" in redacted


def test_redaction_provider_does_not_treat_token_words_as_secret_values() -> None:
    provider = RedactionProvider(marker=RedactionMarker.PLAIN)

    assert not provider.contains_secret("1M tokens")
    assert not provider.contains_secret("https://token-plan-cn.xiaomimimo.com/v1")
    assert provider.contains_secret("Authorization: Bearer ghp_abcdefghijklmnopqrstuvwxyz123456")
    assert provider.contains_secret("https://example.test/path?token=raw-token")


def test_redaction_provider_redacts_command_parts_with_secret_flags() -> None:
    provider = RedactionProvider(marker=RedactionMarker.PLAIN)

    redacted = provider.redact_command_parts(["tool", "--token", "plain-secret", "--safe", "value"])

    assert redacted == ["tool", "--token", "<redacted>", "--safe", "value"]
