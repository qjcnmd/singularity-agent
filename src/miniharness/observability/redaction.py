from __future__ import annotations

import hashlib
import json
import re
from typing import Any


SECRET_KEY_RE = re.compile(
    r"(authorization|cookie|token|api[_-]?key|secret|password|private[_-]?key|openai_api_key|anthropic_api_key|github_token|npm_token)",
    re.IGNORECASE,
)
ENV_SECRET_RE = re.compile(
    r"(?im)^([A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD)|OPENAI_API_KEY|ANTHROPIC_API_KEY|GITHUB_TOKEN|NPM_TOKEN)\s*=\s*([^\r\n]+)"
)
HEADER_SECRET_RE = re.compile(r"(?im)^(Authorization|Cookie)\s*:\s*(.+)$")
PRIVATE_KEY_RE = re.compile(
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    re.IGNORECASE | re.DOTALL,
)
TOKEN_VALUE_RE = re.compile(
    r"\b(sk-[A-Za-z0-9._\-]+|gh[pousr]_[A-Za-z0-9_]+|npm_[A-Za-z0-9_]+)\b"
)


class TraceRedactor:
    def __init__(self, *, output_limit_chars: int = 8000) -> None:
        self.output_limit_chars = output_limit_chars

    def redact_value(self, value: Any) -> Any:
        if isinstance(value, dict):
            redacted: dict[str, Any] = {}
            for key, item in value.items():
                if SECRET_KEY_RE.search(str(key)):
                    redacted[key] = "<redacted>"
                else:
                    redacted[key] = self.redact_value(item)
            return redacted
        if isinstance(value, list):
            return [self.redact_value(item) for item in value]
        if isinstance(value, tuple):
            return [self.redact_value(item) for item in value]
        if isinstance(value, str):
            return self.redact_text(value)
        return value

    def redact_payload(self, payload: dict[str, Any]) -> dict[str, Any]:
        redacted = self.redact_value(payload)
        return redacted if isinstance(redacted, dict) else {}

    def redact_text(self, text: str) -> str:
        redacted = PRIVATE_KEY_RE.sub("<redacted>", text)
        redacted = ENV_SECRET_RE.sub(lambda match: f"{match.group(1)}=<redacted>", redacted)
        redacted = HEADER_SECRET_RE.sub(lambda match: f"{match.group(1)}: <redacted>", redacted)
        redacted = TOKEN_VALUE_RE.sub("<redacted>", redacted)
        if len(redacted) > self.output_limit_chars:
            return f"{redacted[: self.output_limit_chars]}[truncated]"
        return redacted

    def hash_payload(self, payload: dict[str, Any]) -> str:
        redacted = self.redact_payload(payload)
        text = json.dumps(redacted, ensure_ascii=False, sort_keys=True, default=str)
        return hashlib.sha256(text.encode("utf-8")).hexdigest()
