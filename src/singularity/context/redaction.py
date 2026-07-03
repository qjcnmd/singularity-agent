from __future__ import annotations

import hashlib
import json
from typing import Any

from singularity.context.models import ContextSensitivity
from singularity.redaction import RedactionMarker, RedactionProvider


class SensitivityClassifier:
    def __init__(self, *, provider: RedactionProvider | None = None) -> None:
        self.provider = provider or RedactionProvider(marker=RedactionMarker.DIGEST)

    def classify(self, value: Any) -> ContextSensitivity:
        text = _stringify(value)
        if self.provider.contains_context_secret_text(text):
            return ContextSensitivity.SECRET
        if self.provider.contains_context_sensitive_text(text):
            return ContextSensitivity.SENSITIVE
        if text:
            return ContextSensitivity.WORKSPACE
        return ContextSensitivity.PUBLIC


class ContextRedactor:
    def __init__(
        self,
        *,
        classifier: SensitivityClassifier | None = None,
        provider: RedactionProvider | None = None,
    ) -> None:
        self.provider = provider or RedactionProvider(marker=RedactionMarker.DIGEST)
        self.classifier = classifier or SensitivityClassifier(provider=self.provider)

    def redact_text(self, text: str) -> str:
        return self.provider.redact_text(text)

    def redact_value(self, value: Any) -> Any:
        return self.provider.redact_value(value)

    def hash_value(self, value: Any) -> str:
        return hashlib.sha256(
            json.dumps(value, ensure_ascii=False, sort_keys=True, default=str).encode("utf-8")
        ).hexdigest()


def _stringify(value: Any) -> str:
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
    except TypeError:
        return str(value)
