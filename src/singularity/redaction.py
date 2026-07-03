from __future__ import annotations

import hashlib
import json
import re
from dataclasses import is_dataclass
from enum import Enum, StrEnum
from pathlib import Path
from typing import Any

SECRET_FIELD_RE = re.compile(
    r"(authorization|cookie|token|api[_-]?key|secret|password|private[_-]?key|"
    r"credential|passphrase|access[_-]?token|refresh[_-]?token|client[_-]?secret|"
    r"database[_-]?url|dsn|conn(?:ection)?[_-]?(?:str|string)|openai_api_key|"
    r"anthropic_api_key|github_token|npm_token)",
    re.IGNORECASE,
)
ENV_SECRET_RE = re.compile(
    r"(?im)^([A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|DSN|CONN_STR|CONN_STRING|CONNECTION_STRING)|"
    r"DATABASE_URL|OPENAI_API_KEY|ANTHROPIC_API_KEY|GITHUB_TOKEN|NPM_TOKEN)\s*=\s*([^\r\n]+)"
)
HEADER_SECRET_RE = re.compile(r"(?im)\b(Authorization|Cookie)\s*:\s*([^\r\n,\]]+)")
AUTHORIZATION_BEARER_RE = re.compile(
    r"(?i)(authorization\s*:\s*bearer\s+)([A-Za-z0-9._\-]{8,})"
)
BEARER_VALUE_RE = re.compile(r"(?i)(bearer\s+)([A-Za-z0-9._\-]{8,})")
COOKIE_VALUE_RE = re.compile(r"(?i)\b(cookie\s*:\s*)([^\n\r]+)")
CLI_SECRET_FLAG_RE = re.compile(
    r"(?i)(--?(?:password|passwd|pwd|token|secret|api[-_]?key|authorization|cookie)(?:=|\s+))"
    r"('[^']*'|\"[^\"]*\"|[^\s,\]\}]+)"
)
URL_QUERY_SECRET_RE = re.compile(
    r"(?i)([?&](?:access[_-]?token|api[_-]?key|token|secret|password|signature|sig|auth|key)=)"
    r"([^&#\s,\]\}]+)"
)
JSON_ARG_SECRET_RE = re.compile(
    r"(?i)(\"--?(?:password|passwd|pwd|token|secret|api[-_]?key|authorization|cookie)\"\s*,\s*)"
    r"\"[^\"]*\""
)
PRIVATE_KEY_RE = re.compile(
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    re.IGNORECASE | re.DOTALL,
)
TOKEN_VALUE_RE = re.compile(
    r"\b("
    r"sk-[A-Za-z0-9._\-]+"
    r"|gh[pousr]_[A-Za-z0-9_]+"
    r"|npm_[A-Za-z0-9_]+"
    r"|AKIA[0-9A-Z]{16}"
    r"|eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"
    r"|xox[baprs]-[A-Za-z0-9-]+"
    r"|sk_live_[A-Za-z0-9]+"
    r"|AIza[0-9A-Za-z_-]{35}"
    r")\b"
)
CONTEXT_SECRET_PATTERNS: tuple[re.Pattern[str], ...] = (
    AUTHORIZATION_BEARER_RE,
    BEARER_VALUE_RE,
    re.compile(r"(?i)\b([A-Z0-9_]*(?:API[_-]?KEY|TOKEN|SECRET|PASSWORD))\s*=\s*([^\s]+)"),
    COOKIE_VALUE_RE,
    PRIVATE_KEY_RE,
    TOKEN_VALUE_RE,
)
SENSITIVE_PATH_RE = re.compile(
    r"(^\.env(?:\..*)?$|(^|[\\/])\.ssh([\\/]|$)|(^|[\\/])\.gnupg([\\/]|$)|"
    r"(^|[\\/])\.aws([\\/]|$)|(^|[\\/])\.azure([\\/]|$)|id_rsa|id_dsa|id_ecdsa|id_ed25519|"
    r"credentials?|credential|token|secret|api[_-]?key|password|\.pem$|\.pfx$|\.p12$|\.key$)",
    re.IGNORECASE,
)
SENSITIVE_TEXT_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"(?i)\.env\b"),
    re.compile(r"(?i)\bprivate[_-]?key\b"),
    re.compile(r"(?i)\bpassword\b"),
)
SAFE_NUMERIC_METRIC_KEYS = {
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "cached_input_tokens",
    "reasoning_tokens",
    "prompt_tokens",
    "completion_tokens",
}
SAFE_BOOLEAN_STATUS_KEYS = {"restricted_token"}


class RedactionMarker(StrEnum):
    DIGEST = "digest"
    PLAIN = "plain"
    BRACKETED = "bracketed"


class RedactionProvider:
    def __init__(
        self,
        *,
        marker: RedactionMarker = RedactionMarker.PLAIN,
        output_limit_chars: int | None = None,
    ) -> None:
        self.marker = marker
        self.output_limit_chars = output_limit_chars

    def redact_text(self, text: str) -> str:
        redacted = PRIVATE_KEY_RE.sub(lambda match: self._marker(match.group(0)), text)
        redacted = ENV_SECRET_RE.sub(
            lambda match: f"{match.group(1)}={self._marker(match.group(2))}",
            redacted,
        )
        if self.marker == RedactionMarker.DIGEST:
            redacted = AUTHORIZATION_BEARER_RE.sub(
                lambda match: f"{match.group(1)}{self._marker(match.group(2))}",
                redacted,
            )
            redacted = BEARER_VALUE_RE.sub(
                lambda match: f"{match.group(1)}{self._marker(match.group(2))}",
                redacted,
            )
            redacted = COOKIE_VALUE_RE.sub(
                lambda match: f"{match.group(1)}{self._marker(match.group(2))}",
                redacted,
            )
        else:
            redacted = HEADER_SECRET_RE.sub(
                lambda match: f"{match.group(1)}: {self._marker(match.group(2))}",
                redacted,
            )
        redacted = URL_QUERY_SECRET_RE.sub(
            lambda match: f"{match.group(1)}{self._marker(match.group(2))}",
            redacted,
        )
        redacted = JSON_ARG_SECRET_RE.sub(
            lambda match: f'{match.group(1)}"{self._marker(match.group(0))}"',
            redacted,
        )
        redacted = CLI_SECRET_FLAG_RE.sub(
            lambda match: f"{match.group(1)}{self._marker(match.group(2))}",
            redacted,
        )
        redacted = TOKEN_VALUE_RE.sub(lambda match: self._marker(match.group(0)), redacted)
        if self.output_limit_chars is not None and len(redacted) > self.output_limit_chars:
            return f"{redacted[: self.output_limit_chars]}[truncated]"
        return redacted

    def redact_value(self, value: Any) -> Any:
        if isinstance(value, dict):
            redacted: dict[str, Any] = {}
            for key, item in value.items():
                key_text = str(key)
                if _is_safe_numeric_metric(key_text, item) or _is_safe_boolean_status(key_text, item):
                    redacted[key_text] = item
                elif SECRET_FIELD_RE.search(key_text):
                    redacted[key_text] = self._marker(_stringify(item))
                else:
                    redacted[key_text] = self.redact_value(item)
            return redacted
        if isinstance(value, list):
            return [self.redact_value(item) for item in value]
        if isinstance(value, tuple):
            return [self.redact_value(item) for item in value]
        if isinstance(value, set):
            return sorted(self.redact_value(item) for item in value)
        if isinstance(value, str):
            return self.redact_text(value)
        if isinstance(value, Enum):
            return value.value
        if is_dataclass(value):
            return self.redact_value(value.__dict__)
        return value

    def redact_path(self, path: str | Path) -> str:
        text = str(path)
        normalized = text.replace("\\", "/")
        if SENSITIVE_PATH_RE.search(normalized):
            return self._marker(text)
        return self.redact_text(text)

    def digest_identifier(self, value: str) -> str:
        return hashlib.sha256(value.encode("utf-8")).hexdigest()

    def hash_payload(self, payload: dict[str, Any]) -> str:
        text = json.dumps(self.redact_value(payload), ensure_ascii=False, sort_keys=True, default=str)
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def contains_secret(self, value: Any) -> bool:
        text = _stringify(value)
        return (
            bool(SECRET_FIELD_RE.search(text))
            or bool(ENV_SECRET_RE.search(text))
            or bool(HEADER_SECRET_RE.search(text))
            or bool(PRIVATE_KEY_RE.search(text))
            or bool(TOKEN_VALUE_RE.search(text))
        )

    def contains_context_secret_text(self, value: Any) -> bool:
        text = _stringify(value)
        return any(pattern.search(text) for pattern in CONTEXT_SECRET_PATTERNS)

    def contains_sensitive_path_or_text(self, value: Any) -> bool:
        text = _stringify(value)
        return bool(SENSITIVE_PATH_RE.search(text)) or any(
            pattern.search(text) for pattern in SENSITIVE_TEXT_PATTERNS
        )

    def contains_context_sensitive_text(self, value: Any) -> bool:
        text = _stringify(value)
        return any(pattern.search(text) for pattern in SENSITIVE_TEXT_PATTERNS)

    def _marker(self, value: str) -> str:
        if self.marker == RedactionMarker.DIGEST:
            digest = hashlib.sha256(value.encode("utf-8")).hexdigest()[:12]
            return f"<redacted:{digest}>"
        if self.marker == RedactionMarker.BRACKETED:
            return "[REDACTED]"
        return "<redacted>"


def _is_safe_numeric_metric(key: object, value: Any) -> bool:
    if str(key).lower() not in SAFE_NUMERIC_METRIC_KEYS:
        return False
    return isinstance(value, int | float) and not isinstance(value, bool)


def _is_safe_boolean_status(key: object, value: Any) -> bool:
    return str(key).lower() in SAFE_BOOLEAN_STATUS_KEYS and isinstance(value, bool)


def _stringify(value: Any) -> str:
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
    except TypeError:
        return str(value)
