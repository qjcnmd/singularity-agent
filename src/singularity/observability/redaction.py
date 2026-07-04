from __future__ import annotations

from typing import Any

from singularity.redaction import RedactionMarker, RedactionProvider

DEFAULT_TRACE_REDACTION_OUTPUT_LIMIT_CHARS = 8000


class TraceRedactor:
    def __init__(
        self,
        *,
        output_limit_chars: int = DEFAULT_TRACE_REDACTION_OUTPUT_LIMIT_CHARS,
    ) -> None:
        self.output_limit_chars = output_limit_chars
        self.provider = RedactionProvider(
            marker=RedactionMarker.PLAIN,
            output_limit_chars=output_limit_chars,
        )

    def redact_value(self, value: Any) -> Any:
        return self.provider.redact_value(value)

    def redact_payload(self, payload: dict[str, Any]) -> dict[str, Any]:
        redacted = self.redact_value(payload)
        return redacted if isinstance(redacted, dict) else {}

    def redact_text(self, text: str) -> str:
        return self.provider.redact_text(text)

    def hash_payload(self, payload: dict[str, Any]) -> str:
        return self.provider.hash_payload(payload)


_SHARED_REDACTOR = TraceRedactor()


def shared_trace_redactor(
    *,
    output_limit_chars: int = DEFAULT_TRACE_REDACTION_OUTPUT_LIMIT_CHARS,
) -> TraceRedactor:
    if output_limit_chars == DEFAULT_TRACE_REDACTION_OUTPUT_LIMIT_CHARS:
        return _SHARED_REDACTOR
    return TraceRedactor(output_limit_chars=output_limit_chars)
