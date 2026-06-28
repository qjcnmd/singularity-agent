from __future__ import annotations

import hashlib
import json
import re
from dataclasses import is_dataclass
from enum import Enum
from typing import Any

from singularity.context.models import ContextSensitivity

SECRET_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"(?i)(authorization\s*:\s*bearer\s+)([A-Za-z0-9._\-]{8,})"),
    re.compile(r"(?i)(bearer\s+)([A-Za-z0-9._\-]{8,})"),
    re.compile(r"(?i)\b([A-Z0-9_]*(?:API[_-]?KEY|TOKEN|SECRET|PASSWORD))\s*=\s*([^\s]+)"),
    re.compile(r"(?i)\b(cookie)\s*:\s*([^\n\r]+)"),
    re.compile(r"(?is)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----"),
    re.compile(r"\b(sk-[A-Za-z0-9._\-]+|gh[pousr]_[A-Za-z0-9_]+|npm_[A-Za-z0-9_]+)\b"),
    re.compile(r"\b(AKIA[0-9A-Z]{16})\b"),
    re.compile(r"\b(eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+)\b"),
    re.compile(r"\b(xox[baprs]-[A-Za-z0-9-]+)\b"),
    re.compile(r"\b(sk_live_[A-Za-z0-9]+)\b"),
    re.compile(r"\b(AIza[0-9A-Za-z_-]{35})\b"),
)


SENSITIVE_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"(?i)\.env\b"),
    re.compile(r"(?i)\bprivate[_-]?key\b"),
    re.compile(r"(?i)\bpassword\b"),
)


SENSITIVE_FIELD_MARKERS: tuple[str, ...] = (
    "token",
    "secret",
    "password",
    "cookie",
    "api_key",
    "authorization",
    "credential",
    "passphrase",
    "private_key",
    "access_token",
    "refresh_token",
    "client_secret",
)


class SensitivityClassifier:
    def classify(self, value: Any) -> ContextSensitivity:
        text = _stringify(value)
        if any(pattern.search(text) for pattern in SECRET_PATTERNS):
            return ContextSensitivity.SECRET
        if any(pattern.search(text) for pattern in SENSITIVE_PATTERNS):
            return ContextSensitivity.SENSITIVE
        if text:
            return ContextSensitivity.WORKSPACE
        return ContextSensitivity.PUBLIC


class ContextRedactor:
    def __init__(
        self,
        *,
        classifier: SensitivityClassifier | None = None,
    ) -> None:
        self.classifier = classifier or SensitivityClassifier()

    def redact_text(self, text: str) -> str:
        redacted = text
        for pattern in SECRET_PATTERNS:
            redacted = pattern.sub(self._replacement, redacted)
        return redacted

    def redact_value(self, value: Any) -> Any:
        if isinstance(value, str):
            return self.redact_text(value)
        if isinstance(value, list):
            return [self.redact_value(item) for item in value]
        if isinstance(value, tuple):
            return [self.redact_value(item) for item in value]
        if isinstance(value, set):
            return sorted(self.redact_value(item) for item in value)
        if isinstance(value, dict):
            redacted: dict[str, Any] = {}
            for key, item in value.items():
                key_text = str(key)
                if any(marker in key_text.lower() for marker in SENSITIVE_FIELD_MARKERS):
                    redacted[key_text] = self._marker(str(item))
                else:
                    redacted[key_text] = self.redact_value(item)
            return redacted
        if isinstance(value, Enum):
            return value.value
        if is_dataclass(value):
            return self.redact_value(value.__dict__)
        return value

    def hash_value(self, value: Any) -> str:
        return hashlib.sha256(
            json.dumps(value, ensure_ascii=False, sort_keys=True, default=str).encode("utf-8")
        ).hexdigest()

    def _replacement(self, match: re.Match[str]) -> str:
        groups = match.groups()
        if len(groups) >= 2:
            return f"{groups[0]}{self._marker(groups[-1])}"
        return self._marker(match.group(0))

    @staticmethod
    def _marker(value: str) -> str:
        digest = hashlib.sha256(value.encode("utf-8")).hexdigest()[:12]
        return f"<redacted:{digest}>"


def _stringify(value: Any) -> str:
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
    except TypeError:
        return str(value)
